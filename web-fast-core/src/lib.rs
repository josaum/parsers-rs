#![deny(missing_docs)]
#![warn(rust_2018_idioms)]

//! Deterministic, side-effect-free parsing of static HTML documents.
//!
//! `web-fast-core` accepts bytes or text that the caller already owns. It never
//! performs network requests, executes JavaScript, loads subresources, or
//! consults ambient process state. The resulting [`WebDocument`] preserves a
//! source digest and stable document-order evidence suitable for downstream
//! provenance and Arrow pipelines.

use encoding_rs::{Encoding, UTF_8};
use scraper::{node::Node, ElementRef, Html, Selector};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use thiserror::Error;
use url::Url;

#[cfg(feature = "arrow")]
pub mod arrow;
mod nesting;

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
    /// Elements were nested deeper than [`MAX_NESTING_DEPTH`].
    #[error("HTML element nesting exceeds the supported depth")]
    NestingLimitExceeded,
    /// `rowspan`/`colspan` covered more than [`MAX_TABLE_SPAN_CELLS`] cells.
    #[error("HTML table spans cover more cells than the supported limit")]
    TableSpanLimitExceeded,
    /// The `dom_path` strings exceeded [`MAX_DOM_PATH_BYTES`].
    #[error("HTML structural paths exceed the supported total size")]
    DomPathLimitExceeded,
    /// Reading a local source failed.
    #[error("failed to read HTML source: {0}")]
    Io(#[from] std::io::Error),
    /// Arrow materialization or IPC writing failed.
    #[error("failed to encode HTML as Arrow IPC: {0}")]
    Arrow(String),
}

/// Deepest element nesting accepted in one document.
///
/// The HTML tree builder this crate depends on walks its stack of open
/// elements for every block-level start tag, so its cost grows with the
/// square of the nesting depth: 5,000, 10,000 and 20,000 nested `<div>` took
/// 1.1 s, 5.4 s and 24 s in an unoptimized build. Every text block also
/// stores a `dom_path` as long as its depth. A document nested deeper than
/// this is rejected with [`WebParseError::NestingLimitExceeded`], never
/// truncated. 512 is the depth at which browsers stop nesting elements, so
/// a document a browser renders faithfully stays within it.
pub const MAX_NESTING_DEPTH: usize = 512;

/// Most empty cells `rowspan` and `colspan` may add across one document.
///
/// A spanning cell's text is emitted once, at its top-left position; every
/// other position it covers is emitted as an empty cell so columns line up.
/// A 17-byte `<td colspan=1000>` adds 999 such cells, so without a bound a
/// 32 MiB document could ask for tens of gigabytes. Beyond this many the
/// parse fails with [`WebParseError::TableSpanLimitExceeded`].
pub const MAX_TABLE_SPAN_CELLS: usize = 1_000_000;

/// Elements the tree builder may create per byte of input once it has
/// created more than `max_nodes` of them.
///
/// Reading markup creates at most one element per three bytes (`<p>`). The
/// tree builder also re-creates every open formatting element (`<b>`,
/// `<i>`, ...) inside each following block, so 200 unclosed `<b>` with
/// distinct attributes followed by 20,000 `<p>` turn 86 KB into 4,000,000
/// elements. A handful of unclosed formatting elements over many short
/// paragraphs is ordinary and stays near two elements per byte; four leaves
/// room for that. Past both bounds the parse stops with
/// [`WebParseError::NodeLimitExceeded`].
const MAX_ELEMENTS_PER_INPUT_BYTE: usize = 4;

/// Most bytes of `dom_path` one document may emit, over all of its blocks,
/// links, images and tables.
///
/// A `dom_path` is as long as its element is deep, and no other budget
/// counted it: 250,000 blocks at depth 512 would carry about 2 GB. It has
/// its own budget, not the one for extracted text, because an ordinary
/// large document's paths outweigh its text (240,000 short paragraphs in 12
/// sections have 38 MB of paths). 256 MiB is 1 KiB of path for each of the
/// 250,000 nodes the default node limit allows. Beyond it the parse fails
/// with [`WebParseError::DomPathLimitExceeded`].
pub const MAX_DOM_PATH_BYTES: usize = 256 * 1024 * 1024;

/// Largest `colspan` honored: the maximum the HTML standard allows.
const MAX_COLSPAN: usize = 1_000;

/// Largest `rowspan` honored: the maximum the HTML standard allows.
const MAX_ROWSPAN: usize = 65_534;

/// Resource limits and decoding policy for one parse.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ParseOptions {
    /// Maximum accepted source size in bytes.
    pub max_input_bytes: usize,
    /// Maximum number of emitted blocks, links, images, tables, and JSON-LD values.
    pub max_nodes: usize,
    /// Maximum UTF-8 byte size of all emitted text combined. `dom_path`
    /// strings are bounded separately by [`MAX_DOM_PATH_BYTES`].
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
    ///
    /// A cell with `rowspan`/`colspan` appears once, at its top-left
    /// position; the other positions it covers are empty strings. Cells
    /// without text are kept as empty strings, so a column index means the
    /// same column in every row. Rows may still differ in length.
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
    // The tree builder is quadratic in nesting depth, so the limit is
    // enforced while the tree is built; the finished tree is checked again
    // because the tree builder may also move nodes after appending them.
    // Elements the tree builder may create: more than the node limit and
    // more than `MAX_ELEMENTS_PER_INPUT_BYTE` per byte of input together
    // mean the markup is being multiplied, not read.
    let element_limit = options
        .max_nodes
        .max(html.len().saturating_mul(MAX_ELEMENTS_PER_INPUT_BYTE));
    let document = nesting::parse_document_within(html, MAX_NESTING_DEPTH, element_limit).map_err(
        |stopped| match stopped {
            nesting::Stopped::TooDeep => WebParseError::NestingLimitExceeded,
            nesting::Stopped::TooManyElements => WebParseError::NodeLimitExceeded,
        },
    )?;
    check_tree_depth(&document)?;
    let mut budget = Budget::new(options.max_nodes, options.max_text_bytes);

    // Position of every element among its same-named siblings, computed in
    // one pass with a running counter per parent. Counting the previous
    // siblings of each element on demand cost n(n-1)/2 sibling visits for n
    // siblings: 4.5 million for 3,000 paragraphs in one body.
    let mut positions = HashMap::new();
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for parent in document.tree.root().descendants() {
        if !parent.has_children() {
            continue;
        }
        counts.clear();
        for child in parent.children().filter_map(ElementRef::wrap) {
            let count = counts.entry(child.value().name()).or_insert(0);
            *count += 1;
            positions.insert(child.id(), *count);
        }
    }
    // `dom_path` is as long as the element is deep, so it is charged to a
    // budget of its own.
    let structural_path =
        |element: ElementRef<'_>, budget: &mut Budget| -> Result<String, WebParseError> {
            let mut parts = std::iter::once(element)
                .chain(element.ancestors().filter_map(ElementRef::wrap))
                .map(|ancestor| {
                    let position = positions.get(&ancestor.id()).copied().unwrap_or(1);
                    (ancestor.value().name(), position)
                })
                .collect::<Vec<_>>();
            parts.reverse();
            let mut path = String::new();
            for (name, position) in parts {
                path.push('/');
                path.push_str(name);
                path.push('[');
                path.push_str(&position.to_string());
                path.push(']');
            }
            if path.is_empty() {
                path.push('/');
            }
            budget.add_path(&path)?;
            Ok(path)
        };

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
            dom_path: structural_path(element, &mut budget)?,
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
            dom_path: structural_path(element, &mut budget)?,
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
            dom_path: structural_path(element, &mut budget)?,
            src: src.to_owned(),
            resolved_url: resolve_url(effective_base.as_ref(), Some(src)),
            alt: element.value().attr("alt").map(str::to_owned),
            width: element.value().attr("width").map(str::to_owned),
            height: element.value().attr("height").map(str::to_owned),
        });
    }

    let table_selector = selector("table")?;
    let mut tables = Vec::new();
    let mut ignored_spans = 0usize;
    for table in document.select(&table_selector) {
        budget.add_node()?;
        let parts = own_table_parts(table);
        let caption = parts
            .caption
            .map(normalized_text)
            .filter(|value| !value.is_empty());
        if let Some(value) = &caption {
            budget.add_text(value)?;
        }
        let rows = table_rows(&parts.rows, &mut budget, &mut ignored_spans)?;
        tables.push(WebTable {
            ordinal: ordinal(tables.len())?,
            dom_path: structural_path(table, &mut budget)?,
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
    let mut diagnostics = Vec::new();
    if invalid_json_ld != 0 {
        diagnostics.push(format!("invalid_json_ld:{invalid_json_ld}"));
    }
    if ignored_spans != 0 {
        diagnostics.push(format!("ignored_table_spans:{ignored_spans}"));
    }

    Ok(WebDocument {
        source_sha256: lower_hex(&Sha256::digest(source_bytes)),
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

/// Rejects a built tree whose elements nest deeper than [`MAX_NESTING_DEPTH`].
fn check_tree_depth(document: &Html) -> Result<(), WebParseError> {
    // Explicit stack of (node, element depth of its parent): no recursion.
    let mut pending = vec![(document.tree.root(), 0usize)];
    while let Some((node, parent_depth)) = pending.pop() {
        let depth = parent_depth + usize::from(node.value().is_element());
        if depth > MAX_NESTING_DEPTH {
            return Err(WebParseError::NestingLimitExceeded);
        }
        pending.extend(node.children().map(|child| (child, depth)));
    }
    Ok(())
}

/// A cell from an earlier row that still covers columns of the current one.
struct RowSpan {
    column: usize,
    width: usize,
    rows_left: usize,
}

/// Reads a `rowspan`/`colspan` attribute the way browsers do.
///
/// The value is the run of digits at the start, after optional whitespace
/// and one `+`; what follows is ignored, so `2px` and `2.5` are 2. A missing
/// attribute is 1. A value with no leading digits, or outside 1 to
/// `maximum`, is treated as 1 and counted in `ignored`, so a hostile span
/// neither expands the table nor passes unnoticed. `rowspan="0"` (to the end
/// of the row group, per HTML) is returned as `usize::MAX`.
fn span_attr(cell: ElementRef<'_>, name: &str, maximum: usize, ignored: &mut usize) -> usize {
    let Some(raw) = cell.value().attr(name) else {
        return 1;
    };
    let trimmed = raw.trim_start();
    let unsigned = trimmed.strip_prefix('+').unwrap_or(trimmed);
    let digits = unsigned
        .find(|c: char| !c.is_ascii_digit())
        .map_or(unsigned, |end| &unsigned[..end]);
    match digits.parse::<usize>() {
        Ok(0) if name == "rowspan" => usize::MAX,
        Ok(value) if (1..=maximum).contains(&value) => value,
        _ => {
            *ignored += 1;
            1
        }
    }
}

/// Lays a table's rows out on a grid, honoring `rowspan` and `colspan`.
///
/// `rowspan` only ever covers rows that exist: a span that outlives the
/// table's last row is dropped, so it cannot create rows. Every empty cell a
/// span adds is charged to the document-wide [`MAX_TABLE_SPAN_CELLS`].
fn table_rows(
    rows: &[ElementRef<'_>],
    budget: &mut Budget,
    ignored_spans: &mut usize,
) -> Result<Vec<Vec<String>>, WebParseError> {
    // Spans carried down from earlier rows: sorted by column, never overlapping.
    let mut active: Vec<RowSpan> = Vec::new();
    let mut grid = Vec::new();
    let mut group = None;
    for row in rows {
        // A cell cannot span past its row group (`thead`/`tbody`/`tfoot`).
        let row_group = row.parent().map(|parent| parent.id());
        if row_group != group {
            active.clear();
            group = row_group;
        }
        let mut out: Vec<String> = Vec::new();
        let mut carried: Vec<RowSpan> = Vec::new();
        let mut next_active = 0usize;
        let cells = row
            .children()
            .filter_map(ElementRef::wrap)
            .filter(|cell| matches!(cell.value().name(), "th" | "td"));
        for cell in cells {
            // Columns covered from above come before the next real cell.
            while let Some(span) = active.get(next_active).filter(|s| s.column <= out.len()) {
                cover(span, &mut out, &mut carried, budget)?;
                next_active += 1;
            }
            let text = normalized_text(cell);
            budget.add_text(&text)?;
            let width = span_attr(cell, "colspan", MAX_COLSPAN, ignored_spans);
            let height = span_attr(cell, "rowspan", MAX_ROWSPAN, ignored_spans);
            let column = out.len();
            out.push(text);
            add_empty_cells(&mut out, width - 1, budget)?;
            // The cell keeps its full width even where it runs into columns
            // covered from above, as in a browser. Those spans are absorbed:
            // their columns are already emitted, and the merged span is what
            // carries down.
            let mut rows_left = height - 1;
            while let Some(span) = active.get(next_active).filter(|s| s.column < out.len()) {
                let end = span.column + span.width;
                let missing = end.saturating_sub(out.len());
                add_empty_cells(&mut out, missing, budget)?;
                rows_left = rows_left.max(span.rows_left - 1);
                next_active += 1;
            }
            if rows_left > 0 {
                carried.push(RowSpan {
                    column,
                    width: out.len() - column,
                    rows_left,
                });
            }
        }
        for span in &active[next_active..] {
            let gap = span.column.saturating_sub(out.len());
            add_empty_cells(&mut out, gap, budget)?;
            cover(span, &mut out, &mut carried, budget)?;
        }
        active = carried;
        if !out.is_empty() {
            grid.push(out);
        }
    }
    Ok(grid)
}

/// Emits the empty cells of one span carried into the current row.
fn cover(
    span: &RowSpan,
    out: &mut Vec<String>,
    carried: &mut Vec<RowSpan>,
    budget: &mut Budget,
) -> Result<(), WebParseError> {
    let column = out.len();
    add_empty_cells(out, span.width, budget)?;
    if span.rows_left > 1 {
        carried.push(RowSpan {
            column,
            width: span.width,
            rows_left: span.rows_left - 1,
        });
    }
    Ok(())
}

fn add_empty_cells(
    out: &mut Vec<String>,
    count: usize,
    budget: &mut Budget,
) -> Result<(), WebParseError> {
    budget.add_span_cells(count)?;
    out.extend(std::iter::repeat_with(String::new).take(count));
    Ok(())
}

/// The caption and rows that belong to one table, in document order.
struct TableParts<'a> {
    caption: Option<ElementRef<'a>>,
    rows: Vec<ElementRef<'a>>,
}

/// Collects a table's own caption and rows without entering nested tables.
///
/// A nested table is reported as its own [`WebTable`], so its rows are not
/// repeated in every enclosing table. Each node is visited once per table it
/// directly belongs to, which keeps the walk linear in the document size.
fn own_table_parts(table: ElementRef<'_>) -> TableParts<'_> {
    let mut parts = TableParts {
        caption: None,
        rows: Vec::new(),
    };
    // Explicit stack, pushed in reverse so nodes pop in document order.
    let mut pending = table.children().rev().collect::<Vec<_>>();
    while let Some(node) = pending.pop() {
        let Some(element) = ElementRef::wrap(node) else {
            continue;
        };
        match element.value().name() {
            "table" => {}
            "tr" => parts.rows.push(element),
            "caption" => {
                parts.caption.get_or_insert(element);
            }
            // Cells are only read as children of a row.
            "th" | "td" => {}
            _ => pending.extend(node.children().rev()),
        }
    }
    parts
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
    path_bytes: usize,
    span_cells: usize,
    nodes: usize,
    text_bytes: usize,
    max_nodes: usize,
    max_text_bytes: usize,
}

impl Budget {
    fn new(max_nodes: usize, max_text_bytes: usize) -> Self {
        Self {
            path_bytes: 0,
            span_cells: 0,
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

    fn add_path(&mut self, path: &str) -> Result<(), WebParseError> {
        self.path_bytes = self.path_bytes.saturating_add(path.len());
        if self.path_bytes > MAX_DOM_PATH_BYTES {
            return Err(WebParseError::DomPathLimitExceeded);
        }
        Ok(())
    }

    fn add_span_cells(&mut self, count: usize) -> Result<(), WebParseError> {
        self.span_cells = self.span_cells.saturating_add(count);
        if self.span_cells > MAX_TABLE_SPAN_CELLS {
            return Err(WebParseError::TableSpanLimitExceeded);
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

/// Lower-case hexadecimal of `bytes`, two digits per byte.
///
/// The digest type of `sha2` 0.11 has no `LowerHex` implementation, so the
/// digest is written out here; the text is the same as `{:x}` gave before.
fn lower_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        text.push(char::from(DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    text
}
