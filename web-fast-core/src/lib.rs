#![deny(missing_docs)]
#![warn(rust_2018_idioms)]

//! Deterministic, side-effect-free parsing of static HTML documents.
//!
//! `web-fast-core` accepts bytes or text that the caller already owns. It never
//! performs network requests, executes JavaScript, loads subresources, or
//! consults ambient process state. The resulting [`WebDocument`] preserves a
//! source digest and stable document-order evidence suitable for Mycelia's
//! provenance and Arrow pipelines.

use encoding_rs::{Encoding, UTF_8};
use scraper::{node::Node, ElementRef, Html, Selector};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use thiserror::Error;
use url::Url;

#[cfg(feature = "arrow")]
pub mod arrow;

/// Parser failures that are safe to expose without including source content.
#[derive(Debug, Error)]
pub enum WebParseError {
    /// The source exceeded the configured byte limit.
    #[error("HTML input exceeds the configured byte limit")]
    InputTooLarge,
    /// The requested character encoding was unknown or decoding was lossy.
    #[error("HTML input is not valid in the configured character encoding")]
    InvalidEncoding,
    /// A built-in selector could not be compiled.
    #[error("internal HTML selector is invalid")]
    InvalidSelector,
    /// The extracted document exceeded the configured node limit.
    #[error("HTML document exceeds the configured node limit")]
    NodeLimitExceeded,
    /// Extracted text exceeded the configured byte limit.
    #[error("HTML text exceeds the configured byte limit")]
    TextLimitExceeded,
    /// Reading a local source failed.
    #[error("failed to read HTML source: {0}")]
    Io(#[from] std::io::Error),
    /// Arrow materialization or IPC writing failed.
    #[error("failed to encode HTML as Arrow IPC: {0}")]
    Arrow(String),
}

/// Resource limits and decoding policy for one parse.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ParseOptions {
    /// Maximum accepted source size in bytes.
    pub max_input_bytes: usize,
    /// Maximum number of emitted blocks, links, images, tables, and JSON-LD values.
    pub max_nodes: usize,
    /// Maximum UTF-8 byte size of all emitted text combined.
    pub max_text_bytes: usize,
    /// Optional WHATWG encoding label. UTF-8 is required when absent.
    pub encoding_label: Option<String>,
}

impl Default for ParseOptions {
    fn default() -> Self {
        Self {
            max_input_bytes: 32 * 1024 * 1024,
            max_nodes: 250_000,
            max_text_bytes: 32 * 1024 * 1024,
            encoding_label: None,
        }
    }
}

/// Semantic type of a document-order text block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    /// Heading (`h1` through `h6`).
    Heading,
    /// Paragraph.
    Paragraph,
    /// List item.
    ListItem,
    /// Quoted text.
    Quote,
    /// Preformatted text.
    Preformatted,
    /// Figure caption.
    Caption,
    /// Definition term.
    DefinitionTerm,
    /// Definition description.
    DefinitionDescription,
    /// Table header cell.
    TableHeader,
    /// Table data cell.
    TableCell,
}

/// One text-bearing HTML element in deterministic document order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebBlock {
    /// Zero-based order among emitted blocks.
    pub ordinal: u32,
    /// Semantic block kind.
    pub kind: BlockKind,
    /// Stable structural path derived from tag names and sibling positions.
    pub dom_path: String,
    /// Normalized visible text.
    pub text: String,
    /// Heading level for heading blocks.
    pub heading_level: Option<u8>,
}

/// One link occurrence preserved in document order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebLink {
    /// Zero-based order among emitted links.
    pub ordinal: u32,
    /// Stable structural path of the source element.
    pub dom_path: String,
    /// Normalized anchor text.
    pub text: String,
    /// Original `href` value.
    pub href: String,
    /// URL resolved against the document base when possible.
    pub resolved_url: Option<String>,
    /// Original `rel` attribute.
    pub rel: Option<String>,
}

/// One image occurrence preserved in document order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebImage {
    /// Zero-based order among emitted images.
    pub ordinal: u32,
    /// Stable structural path of the image element.
    pub dom_path: String,
    /// Original `src` value.
    pub src: String,
    /// URL resolved against the document base when possible.
    pub resolved_url: Option<String>,
    /// Alternative text.
    pub alt: Option<String>,
    /// Optional width attribute.
    pub width: Option<String>,
    /// Optional height attribute.
    pub height: Option<String>,
}

/// One table represented as normalized row-major text cells.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebTable {
    /// Zero-based order among emitted tables.
    pub ordinal: u32,
    /// Stable structural path of the table element.
    pub dom_path: String,
    /// Optional normalized caption.
    pub caption: Option<String>,
    /// Row-major normalized cells.
    pub rows: Vec<Vec<String>>,
}

/// Parsed static HTML with content-derived identity and deterministic ordering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebDocument {
    /// SHA-256 of the exact input bytes.
    pub source_sha256: String,
    /// Effective base URL used for URL resolution.
    pub base_url: Option<String>,
    /// Document title.
    pub title: Option<String>,
    /// Root HTML language.
    pub language: Option<String>,
    /// Canonical URL resolved against the effective base URL.
    pub canonical_url: Option<String>,
    /// Metadata description.
    pub description: Option<String>,
    /// Normalized document text assembled from blocks.
    pub text: String,
    /// Document-order semantic blocks.
    pub blocks: Vec<WebBlock>,
    /// Link occurrences.
    pub links: Vec<WebLink>,
    /// Image occurrences.
    pub images: Vec<WebImage>,
    /// Extracted tables.
    pub tables: Vec<WebTable>,
    /// Valid embedded `application/ld+json` values.
    pub json_ld: Vec<serde_json::Value>,
    /// Deterministic, payload-free parse diagnostics.
    pub diagnostics: Vec<String>,
}

/// Parse a local HTML file without fetching any external resource.
pub fn parse_html_file(
    path: impl AsRef<Path>,
    base_url: Option<&str>,
    options: ParseOptions,
) -> Result<WebDocument, WebParseError> {
    let metadata = fs::metadata(path.as_ref())?;
    if metadata.len() > options.max_input_bytes as u64 {
        return Err(WebParseError::InputTooLarge);
    }
    let bytes = fs::read(path)?;
    parse_html_bytes(&bytes, base_url, options)
}

/// Decode and parse HTML bytes without performing network access.
pub fn parse_html_bytes(
    bytes: &[u8],
    base_url: Option<&str>,
    options: ParseOptions,
) -> Result<WebDocument, WebParseError> {
    if bytes.len() > options.max_input_bytes {
        return Err(WebParseError::InputTooLarge);
    }
    let (encoding, bom_bytes) = resolve_encoding(bytes, options.encoding_label.as_deref())?;
    let (decoded, had_errors) = encoding.decode_without_bom_handling(&bytes[bom_bytes..]);
    if had_errors {
        return Err(WebParseError::InvalidEncoding);
    }
    parse_html_inner(&decoded, bytes, base_url, &options)
}

/// Parse an already-decoded HTML string without performing network access.
pub fn parse_html(
    html: &str,
    base_url: Option<&str>,
    options: ParseOptions,
) -> Result<WebDocument, WebParseError> {
    if html.len() > options.max_input_bytes {
        return Err(WebParseError::InputTooLarge);
    }
    parse_html_inner(html, html.as_bytes(), base_url, &options)
}

fn resolve_encoding(
    bytes: &[u8],
    explicit_label: Option<&str>,
) -> Result<(&'static Encoding, usize), WebParseError> {
    if let Some(label) = explicit_label {
        let encoding =
            Encoding::for_label(label.trim().as_bytes()).ok_or(WebParseError::InvalidEncoding)?;
        return Ok((encoding, matching_bom_len(bytes, encoding)));
    }
    if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        return Ok((UTF_8, 3));
    }
    if bytes.starts_with(&[0xff, 0xfe]) {
        return Ok((encoding_rs::UTF_16LE, 2));
    }
    if bytes.starts_with(&[0xfe, 0xff]) {
        return Ok((encoding_rs::UTF_16BE, 2));
    }
    if let Some(label) = meta_charset_label(bytes) {
        if let Some(encoding) = Encoding::for_label(label.as_bytes()) {
            return Ok((encoding, 0));
        }
    }
    Ok((UTF_8, 0))
}

fn matching_bom_len(bytes: &[u8], encoding: &'static Encoding) -> usize {
    if encoding == UTF_8 && bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        3
    } else if (encoding == encoding_rs::UTF_16LE || encoding == encoding_rs::UTF_16BE)
        && (bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]))
    {
        2
    } else {
        0
    }
}

fn meta_charset_label(bytes: &[u8]) -> Option<String> {
    let prefix = &bytes[..bytes.len().min(1024)];
    let lower = prefix
        .iter()
        .map(u8::to_ascii_lowercase)
        .collect::<Vec<_>>();
    let needle = b"charset";
    let charset_start = lower
        .windows(needle.len())
        .position(|window| window == needle)?;
    let meta_start = lower[..charset_start]
        .windows(5)
        .rposition(|window| window == b"<meta")?;
    if lower[meta_start..charset_start].contains(&b'>') {
        return None;
    }
    let mut cursor = charset_start + needle.len();
    while lower
        .get(cursor)
        .is_some_and(|byte| byte.is_ascii_whitespace())
    {
        cursor += 1;
    }
    if lower.get(cursor) == Some(&b'=') {
        cursor += 1;
    }
    while lower
        .get(cursor)
        .is_some_and(|byte| byte.is_ascii_whitespace() || matches!(byte, b'\'' | b'\"'))
    {
        cursor += 1;
    }
    let end = (cursor..lower.len())
        .find(|index| {
            !lower[*index].is_ascii_alphanumeric() && !matches!(lower[*index], b'-' | b'_' | b'.')
        })
        .unwrap_or(lower.len());
    (end > cursor).then(|| String::from_utf8_lossy(&lower[cursor..end]).into_owned())
}

fn selector(value: &str) -> Result<Selector, WebParseError> {
    Selector::parse(value).map_err(|_| WebParseError::InvalidSelector)
}

fn parse_html_inner(
    html: &str,
    source_bytes: &[u8],
    supplied_base_url: Option<&str>,
    options: &ParseOptions,
) -> Result<WebDocument, WebParseError> {
    let document = Html::parse_document(html);
    let mut budget = Budget::new(options.max_nodes, options.max_text_bytes);

    let supplied_base = supplied_base_url.and_then(|value| Url::parse(value).ok());
    let base_selector = selector("base[href]")?;
    let declared_base = document
        .select(&base_selector)
        .next()
        .and_then(|element| element.value().attr("href"));
    let effective_base = resolve_url(supplied_base.as_ref(), declared_base)
        .and_then(|value| Url::parse(&value).ok())
        .or(supplied_base);

    let title = first_text(&document, "title", &mut budget)?;
    let language = first_attr(&document, "html[lang]", "lang");
    let description = first_attr(&document, "meta[name='description'][content]", "content");
    let canonical_url = first_attr(&document, "link[rel~='canonical'][href]", "href")
        .and_then(|value| resolve_url(effective_base.as_ref(), Some(&value)));

    let block_selector = selector("h1,h2,h3,h4,h5,h6,p,li,blockquote,pre,figcaption,dt,dd,th,td")?;
    let mut blocks = Vec::new();
    for element in document.select(&block_selector) {
        if element_is_hidden(element) {
            continue;
        }
        let tag = element.value().name();
        let Some(kind) = block_kind(tag) else {
            continue;
        };
        let text = normalized_text(element);
        if text.is_empty() {
            continue;
        }
        budget.add_node()?;
        budget.add_text(&text)?;
        blocks.push(WebBlock {
            ordinal: ordinal(blocks.len())?,
            kind,
            dom_path: structural_path(element),
            text,
            heading_level: tag
                .strip_prefix('h')
                .and_then(|value| value.parse::<u8>().ok())
                .filter(|level| (1..=6).contains(level)),
        });
    }

    let mut links = Vec::new();
    let link_selector = selector("a[href],area[href]")?;
    for element in document.select(&link_selector) {
        let Some(href) = element.value().attr("href").map(str::trim) else {
            continue;
        };
        if href.is_empty() {
            continue;
        }
        budget.add_node()?;
        let text = normalized_text(element);
        budget.add_text(&text)?;
        links.push(WebLink {
            ordinal: ordinal(links.len())?,
            dom_path: structural_path(element),
            text,
            href: href.to_owned(),
            resolved_url: resolve_url(effective_base.as_ref(), Some(href)),
            rel: element.value().attr("rel").map(str::to_owned),
        });
    }

    let mut images = Vec::new();
    let image_selector = selector("img[src]")?;
    for element in document.select(&image_selector) {
        let Some(src) = element.value().attr("src").map(str::trim) else {
            continue;
        };
        if src.is_empty() {
            continue;
        }
        budget.add_node()?;
        images.push(WebImage {
            ordinal: ordinal(images.len())?,
            dom_path: structural_path(element),
            src: src.to_owned(),
            resolved_url: resolve_url(effective_base.as_ref(), Some(src)),
            alt: element.value().attr("alt").map(str::to_owned),
            width: element.value().attr("width").map(str::to_owned),
            height: element.value().attr("height").map(str::to_owned),
        });
    }

    let row_selector = selector("tr")?;
    let cell_selector = selector("th,td")?;
    let caption_selector = selector("caption")?;
    let table_selector = selector("table")?;
    let mut tables = Vec::new();
    for table in document.select(&table_selector) {
        budget.add_node()?;
        let caption = table
            .select(&caption_selector)
            .next()
            .map(normalized_text)
            .filter(|value| !value.is_empty());
        if let Some(value) = &caption {
            budget.add_text(value)?;
        }
        let mut rows = Vec::new();
        for row in table.select(&row_selector) {
            let cells = row
                .select(&cell_selector)
                .map(normalized_text)
                .filter(|value| !value.is_empty())
                .collect::<Vec<_>>();
            for cell in &cells {
                budget.add_text(cell)?;
            }
            if !cells.is_empty() {
                rows.push(cells);
            }
        }
        tables.push(WebTable {
            ordinal: ordinal(tables.len())?,
            dom_path: structural_path(table),
            caption,
            rows,
        });
    }

    let json_ld_selector = selector("script[type='application/ld+json']")?;
    let mut json_ld = Vec::new();
    let mut invalid_json_ld = 0usize;
    for element in document.select(&json_ld_selector) {
        let raw = element.text().collect::<String>();
        if raw.trim().is_empty() {
            continue;
        }
        budget.add_node()?;
        budget.add_text(&raw)?;
        match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(value) => json_ld.push(value),
            Err(_) => invalid_json_ld += 1,
        }
    }

    let text = if blocks.is_empty() {
        let fallback = fallback_text(&document);
        budget.add_text(&fallback)?;
        fallback
    } else {
        blocks
            .iter()
            .map(|block| block.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let diagnostics = if invalid_json_ld == 0 {
        Vec::new()
    } else {
        vec![format!("invalid_json_ld:{invalid_json_ld}")]
    };

    Ok(WebDocument {
        source_sha256: format!("{:x}", Sha256::digest(source_bytes)),
        base_url: effective_base.map(Into::into),
        title,
        language,
        canonical_url,
        description,
        text,
        blocks,
        links,
        images,
        tables,
        json_ld,
        diagnostics,
    })
}

fn first_text(
    document: &Html,
    query: &str,
    budget: &mut Budget,
) -> Result<Option<String>, WebParseError> {
    let selector = selector(query)?;
    let value = document
        .select(&selector)
        .next()
        .map(normalized_text)
        .filter(|value| !value.is_empty());
    if let Some(value) = &value {
        budget.add_text(value)?;
    }
    Ok(value)
}

fn first_attr(document: &Html, query: &str, attribute: &str) -> Option<String> {
    let selector = Selector::parse(query).ok()?;
    document
        .select(&selector)
        .next()
        .and_then(|element| element.value().attr(attribute))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn block_kind(tag: &str) -> Option<BlockKind> {
    match tag {
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => Some(BlockKind::Heading),
        "p" => Some(BlockKind::Paragraph),
        "li" => Some(BlockKind::ListItem),
        "blockquote" => Some(BlockKind::Quote),
        "pre" => Some(BlockKind::Preformatted),
        "figcaption" => Some(BlockKind::Caption),
        "dt" => Some(BlockKind::DefinitionTerm),
        "dd" => Some(BlockKind::DefinitionDescription),
        "th" => Some(BlockKind::TableHeader),
        "td" => Some(BlockKind::TableCell),
        _ => None,
    }
}

fn normalized_text(element: ElementRef<'_>) -> String {
    element
        .text()
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ")
}

fn fallback_text(document: &Html) -> String {
    let mut words = Vec::new();
    for node in document.tree.root().descendants() {
        let Node::Text(text) = node.value() else {
            continue;
        };
        let ancestors = node
            .ancestors()
            .filter_map(ElementRef::wrap)
            .collect::<Vec<_>>();
        if !ancestors
            .iter()
            .any(|element| element.value().name() == "body")
            || ancestors.iter().copied().any(is_hidden_element)
        {
            continue;
        }
        words.extend(text.split_whitespace().map(str::to_owned));
    }
    words.join(" ")
}

fn element_is_hidden(element: ElementRef<'_>) -> bool {
    is_hidden_element(element)
        || element
            .ancestors()
            .filter_map(ElementRef::wrap)
            .any(is_hidden_element)
}

fn is_hidden_element(element: ElementRef<'_>) -> bool {
    element.value().attr("hidden").is_some()
        || element
            .value()
            .attr("aria-hidden")
            .is_some_and(|value| value.eq_ignore_ascii_case("true"))
        || matches!(
            element.value().name(),
            "script" | "style" | "noscript" | "template" | "svg" | "canvas"
        )
}

fn structural_path(element: ElementRef<'_>) -> String {
    let mut parts = std::iter::once(element)
        .chain(element.ancestors().filter_map(ElementRef::wrap))
        .map(|ancestor| {
            let name = ancestor.value().name();
            let position = ancestor
                .prev_siblings()
                .filter_map(ElementRef::wrap)
                .filter(|sibling| sibling.value().name() == name)
                .count()
                + 1;
            format!("{name}[{position}]")
        })
        .collect::<Vec<_>>();
    parts.reverse();
    format!("/{}", parts.join("/"))
}

fn resolve_url(base: Option<&Url>, raw: Option<&str>) -> Option<String> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    Url::parse(raw)
        .or_else(|_| {
            base.ok_or(url::ParseError::RelativeUrlWithoutBase)?
                .join(raw)
        })
        .ok()
        .map(Into::into)
}

fn ordinal(value: usize) -> Result<u32, WebParseError> {
    u32::try_from(value).map_err(|_| WebParseError::NodeLimitExceeded)
}

struct Budget {
    nodes: usize,
    text_bytes: usize,
    max_nodes: usize,
    max_text_bytes: usize,
}

impl Budget {
    fn new(max_nodes: usize, max_text_bytes: usize) -> Self {
        Self {
            nodes: 0,
            text_bytes: 0,
            max_nodes,
            max_text_bytes,
        }
    }

    fn add_node(&mut self) -> Result<(), WebParseError> {
        self.nodes = self
            .nodes
            .checked_add(1)
            .ok_or(WebParseError::NodeLimitExceeded)?;
        if self.nodes > self.max_nodes {
            return Err(WebParseError::NodeLimitExceeded);
        }
        Ok(())
    }

    fn add_text(&mut self, text: &str) -> Result<(), WebParseError> {
        self.text_bytes = self
            .text_bytes
            .checked_add(text.len())
            .ok_or(WebParseError::TextLimitExceeded)?;
        if self.text_bytes > self.max_text_bytes {
            return Err(WebParseError::TextLimitExceeded);
        }
        Ok(())
    }
}

#[cfg(feature = "arrow")]
pub(crate) fn block_kind_name(kind: BlockKind) -> &'static str {
    match kind {
        BlockKind::Heading => "heading",
        BlockKind::Paragraph => "paragraph",
        BlockKind::ListItem => "list_item",
        BlockKind::Quote => "quote",
        BlockKind::Preformatted => "preformatted",
        BlockKind::Caption => "caption",
        BlockKind::DefinitionTerm => "definition_term",
        BlockKind::DefinitionDescription => "definition_description",
        BlockKind::TableHeader => "table_header",
        BlockKind::TableCell => "table_cell",
    }
}
