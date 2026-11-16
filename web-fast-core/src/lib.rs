#![deny(missing_docs)]
#![warn(rust_2018_idioms)]

//! Deterministic, side-effect-free parsing of static HTML documents.
//!
//! `web-fast-core` accepts bytes or text that the caller already owns. It never
//! performs network requests, executes JavaScript, loads subresources, or
//! consults ambient process state. The resulting [`WebDocument`] preserves a
//! source digest and stable document-order evidence suitable for downstream
//! provenance and Arrow pipelines.

use scraper::ElementRef;
use std::io::Read;

mod decoding;
mod dom;
mod nesting;
use dom::{DomIndex, TextMode};
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
    /// The parsed DOM exceeded the configured nesting limit.
    #[error("HTML document exceeds the configured depth limit")]
    DepthLimitExceeded,
    /// Extracted text exceeded the configured byte limit.
    #[error("HTML text exceeds the configured byte limit")]
    TextLimitExceeded,
    /// Tree construction exceeded the supported nesting depth.
    #[error("HTML element nesting exceeds the supported depth")]
    NestingLimitExceeded,
    /// Expanded table spans exceeded the supported cell limit.
    #[error("HTML table spans cover more cells than the supported limit")]
    TableSpanLimitExceeded,
    /// Structural paths exceeded their separate size budget.
    #[error("HTML structural paths exceed the supported total size")]
    DomPathLimitExceeded,
    /// Reading a local source failed.
    #[error("failed to read HTML source: {0}")]
    Io(#[from] std::io::Error),
    /// Arrow materialization or IPC writing failed.
    #[error("failed to encode HTML as Arrow IPC: {0}")]
    Arrow(String),
}

/// Hard ceiling on element nesting during HTML tree construction.
pub const MAX_NESTING_DEPTH: usize = 512;
/// Aggregate limit on cells introduced by table spans.
pub const MAX_TABLE_SPAN_CELLS: usize = 1_000_000;
const MAX_COLSPAN: usize = 1_000;
const MAX_ROWSPAN: usize = 65_534;
/// Aggregate limit on structural path bytes.
pub const MAX_DOM_PATH_BYTES: usize = 256 * 1024 * 1024;

/// Resource limits and decoding policy for one parse.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ParseOptions {
    /// Maximum accepted source size in bytes.
    pub max_input_bytes: usize,
    /// Maximum emitted blocks, links, images, tables, rows, cells, and JSON-LD values.
    pub max_nodes: usize,
    /// Maximum parsed DOM nodes, including text, comments, and hidden content.
    pub max_dom_nodes: usize,
    /// Maximum element nesting depth, including implicit HTML/body elements.
    pub max_dom_depth: usize,
    /// Maximum UTF-8 bytes retained in dynamic output strings and JSON-LD source.
    pub max_text_bytes: usize,
    /// Optional strict WHATWG encoding override; conflicting BOMs are rejected.
    /// Otherwise use BOM, an early meta declaration, then strict UTF-8.
    pub encoding_label: Option<String>,
}

impl Default for ParseOptions {
    fn default() -> Self {
        Self {
            max_input_bytes: 32 * 1024 * 1024,
            max_nodes: 250_000,
            max_dom_nodes: 1_000_000,
            max_dom_depth: MAX_NESTING_DEPTH,
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

/// One named HTML metadata declaration, retaining repeated values in DOM order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebMeta {
    /// Attribute identifying the declaration: `name`, `property`, or `http-equiv`.
    pub kind: String,
    /// Original metadata name or property.
    pub name: String,
    /// Original content value.
    pub content: String,
}

/// Strict JSON embedded in a data script or a recognized state assignment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebJson {
    /// `application_json`, `next_data`, `nuxt_data`, or `initial_state`.
    pub kind: String,
    /// Script element ID when present.
    pub id: Option<String>,
    /// Source script path.
    pub dom_path: String,
    /// Parsed JSON. JavaScript expressions are never evaluated by the parser.
    pub value: serde_json::Value,
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
    /// Visible body text in document order, without duplicating nested blocks.
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
    /// Embedded application JSON and hydration state.
    #[serde(default)]
    pub json_data: Vec<WebJson>,
    /// Head-declared next-page and alternate JSON links.
    #[serde(default)]
    pub discovery_links: Vec<WebLink>,
    /// Named metadata, including Open Graph, Twitter, and robots directives.
    #[serde(default)]
    pub metadata: Vec<WebMeta>,
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
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take((options.max_input_bytes as u64).saturating_add(1))
        .read_to_end(&mut bytes)?;
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
    let (encoding, bom_bytes) =
        decoding::resolve_encoding(bytes, options.encoding_label.as_deref())?;
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

fn parse_html_inner(
    html: &str,
    source_bytes: &[u8],
    supplied_base_url: Option<&str>,
    options: &ParseOptions,
) -> Result<WebDocument, WebParseError> {
    let document = nesting::parse_document_within(
        html,
        options.max_dom_depth.min(MAX_NESTING_DEPTH),
        options.max_dom_nodes,
    )
    .map_err(|stopped| match stopped {
        nesting::Stopped::TooDeep => {
            if options.max_dom_depth < MAX_NESTING_DEPTH {
                WebParseError::DepthLimitExceeded
            } else {
                WebParseError::NestingLimitExceeded
            }
        }
        nesting::Stopped::TooManyElements => WebParseError::NodeLimitExceeded,
    })?;
    let index = DomIndex::new(&document, options)?;
    let mut budget = Budget::new(options.max_nodes, options.max_text_bytes);
    if supplied_base_url.is_some_and(|value| value.len() > options.max_text_bytes) {
        return Err(WebParseError::TextLimitExceeded);
    }
    let supplied_base = supplied_base_url.and_then(|value| Url::parse(value).ok());
    let declared_base = index
        .elements
        .iter()
        .find(|element| {
            !index.info[&element.id()].inert
                && element.value().name() == "base"
                && element.value().attr("href").is_some()
        })
        .and_then(|element| element.value().attr("href"));
    let effective_base = resolve_url(supplied_base.as_ref(), declared_base)
        .and_then(|value| Url::parse(&value).ok())
        .filter(|url| !matches!(url.scheme(), "data" | "javascript"))
        .or(supplied_base);
    let base_url = effective_base.as_ref().map(ToString::to_string);
    budget.optional_text(base_url.as_deref())?;

    let mut metadata = Vec::new();
    let mut title = None;
    let mut language = None;
    let mut description = None;
    let mut canonical_url = None;
    let mut blocks = Vec::new();
    let mut links = Vec::new();
    let mut images = Vec::new();
    let mut tables: Vec<WebTable> = Vec::new();
    let mut ignored_spans = 0usize;
    let mut json_ld = Vec::new();
    let mut json_data = Vec::new();
    let mut discovery_links = Vec::new();
    let mut invalid_json_data = 0usize;
    let mut invalid_json_ld = 0usize;
    let mut body = None;
    for &element in &index.elements {
        let tag = element.value().name();
        let info = &index.info[&element.id()];
        if info.inert {
            continue;
        }
        if tag == "meta" {
            if let Some(content) = element.value().attr("content") {
                for kind in ["name", "property", "http-equiv"] {
                    if let Some(name) = element.value().attr(kind) {
                        budget.add_node()?;
                        budget.add_text(kind)?;
                        budget.add_text(name)?;
                        budget.add_text(content)?;
                        metadata.push(WebMeta {
                            kind: kind.into(),
                            name: name.into(),
                            content: content.into(),
                        });
                    }
                }
            }
        }
        match tag {
            "title" if title.is_none() && !info.hidden => {
                let value = index.text(element, TextMode::Normalized, false, &mut budget)?;
                if !value.is_empty() {
                    title = Some(value);
                }
            }
            "html" if language.is_none() => {
                language = owned_attr(element, "lang", &mut budget)?;
            }
            "meta" if description.is_none() && attr_eq(element, "name", "description") => {
                description = owned_attr(element, "content", &mut budget)?;
            }
            "link"
                if canonical_url.is_none()
                    && element.value().attr("rel").is_some_and(|value| {
                        value
                            .split_ascii_whitespace()
                            .any(|part| part.eq_ignore_ascii_case("canonical"))
                    }) =>
            {
                canonical_url = resolve_url(effective_base.as_ref(), element.value().attr("href"));
                budget.optional_text(canonical_url.as_deref())?;
            }
            "script"
                if element
                    .value()
                    .attr("type")
                    .and_then(|value| value.split(';').next())
                    .is_some_and(|value| {
                        value
                            .trim_matches(decoding::ascii_space)
                            .eq_ignore_ascii_case("application/ld+json")
                    })
                    && !element
                        .ancestors()
                        .filter_map(ElementRef::wrap)
                        .any(|ancestor| index.info[&ancestor.id()].hidden) =>
            {
                let mut raw = String::new();
                for part in element.text() {
                    budget.add_text(part)?;
                    raw.push_str(part);
                }
                if !raw.trim().is_empty() {
                    budget.add_node()?;
                    match serde_json::from_str::<serde_json::Value>(&raw) {
                        Ok(value) => json_ld.push(value),
                        Err(_) => invalid_json_ld += 1,
                    }
                }
            }
            "body" if body.is_none() => {
                body = Some(element);
            }
            _ => {}
        }
        if tag == "script"
            && !element
                .ancestors()
                .filter_map(ElementRef::wrap)
                .any(|ancestor| index.info[&ancestor.id()].hidden)
        {
            if let Some((kind, assignment)) = embedded_kind(element) {
                let mut raw = String::new();
                for part in element.text() {
                    budget.add_text(part)?;
                    raw.push_str(part);
                }
                budget.add_node()?;
                let value = if let Some(prefix) = assignment {
                    parse_assigned_json(&raw, prefix)
                } else {
                    serde_json::from_str(&raw).ok()
                };
                if let Some(value) = value {
                    json_data.push(WebJson {
                        kind: kind.into(),
                        id: owned_attr(element, "id", &mut budget)?,
                        dom_path: index.path(element, &mut budget)?,
                        value,
                    });
                } else {
                    invalid_json_data += 1;
                }
            }
        }
        if tag == "link" && element.value().attr("href").is_some() {
            let rel = element.value().attr("rel").unwrap_or("");
            let has_rel = |name: &str| {
                rel.split_ascii_whitespace()
                    .any(|token| token.eq_ignore_ascii_case(name))
            };
            let json_alternate = has_rel("alternate")
                && element.value().attr("type").is_some_and(|value| {
                    value.split(';').next().is_some_and(|kind| {
                        kind.trim().eq_ignore_ascii_case("application/json")
                            || kind.trim().to_ascii_lowercase().ends_with("+json")
                    })
                });
            if has_rel("next") || json_alternate {
                budget.add_node()?;
                let href = owned_attr(element, "href", &mut budget)?.unwrap();
                let resolved_url = resolve_url(effective_base.as_ref(), Some(&href));
                budget.optional_text(resolved_url.as_deref())?;
                discovery_links.push(WebLink {
                    ordinal: ordinal(discovery_links.len())?,
                    dom_path: index.path(element, &mut budget)?,
                    text: String::new(),
                    href,
                    resolved_url,
                    rel: owned_attr(element, "rel", &mut budget)?,
                });
            }
        }
        if info.hidden {
            continue;
        }
        if let Some(kind) = block_kind(tag) {
            let mode = if tag == "pre" {
                TextMode::Preformatted
            } else {
                TextMode::Normalized
            };
            let text = index.text(element, mode, matches!(tag, "td" | "th"), &mut budget)?;
            if !text.is_empty() {
                budget.add_node()?;
                blocks.push(WebBlock {
                    ordinal: ordinal(blocks.len())?,
                    kind,
                    dom_path: index.path(element, &mut budget)?,
                    text,
                    heading_level: tag
                        .strip_prefix('h')
                        .and_then(|value| value.parse::<u8>().ok())
                        .filter(|level| (1..=6).contains(level)),
                });
            }
        }
        match tag {
            "a" | "area" if element.value().attr("href").is_some() => {
                budget.add_node()?;
                let href = owned_attr(element, "href", &mut budget)?.unwrap();
                let resolved_url = resolve_url(effective_base.as_ref(), Some(&href));
                budget.optional_text(resolved_url.as_deref())?;
                links.push(WebLink {
                    ordinal: ordinal(links.len())?,
                    dom_path: index.path(element, &mut budget)?,
                    text: index.text(element, TextMode::Normalized, false, &mut budget)?,
                    href,
                    resolved_url,
                    rel: owned_attr(element, "rel", &mut budget)?,
                });
            }
            "img"
                if element
                    .value()
                    .attr("src")
                    .is_some_and(|value| !value.trim().is_empty()) =>
            {
                budget.add_node()?;
                let src = owned_attr(element, "src", &mut budget)?.unwrap();
                let resolved_url = resolve_url(effective_base.as_ref(), Some(&src));
                budget.optional_text(resolved_url.as_deref())?;
                images.push(WebImage {
                    ordinal: ordinal(images.len())?,
                    dom_path: index.path(element, &mut budget)?,
                    src,
                    resolved_url,
                    alt: owned_attr(element, "alt", &mut budget)?,
                    width: owned_attr(element, "width", &mut budget)?,
                    height: owned_attr(element, "height", &mut budget)?,
                });
            }
            "table" => {
                budget.add_node()?;
                let parts = own_table_parts(element, &index);
                let caption = parts
                    .caption
                    .map(|caption| index.text(caption, TextMode::Normalized, true, &mut budget))
                    .transpose()?
                    .filter(|text| !text.is_empty());
                let rows = table_rows(&parts.rows, &mut budget, &mut ignored_spans, &index)?;
                tables.push(WebTable {
                    ordinal: ordinal(tables.len())?,
                    dom_path: index.path(element, &mut budget)?,
                    caption,
                    rows,
                });
            }
            _ => {}
        }
    }
    let text = match body {
        Some(body) => index.text(body, TextMode::Document, false, &mut budget)?,
        None => String::new(),
    };
    let mut diagnostics = if invalid_json_ld == 0 {
        Vec::new()
    } else {
        vec![format!("invalid_json_ld:{invalid_json_ld}")]
    };
    if ignored_spans > 0 {
        diagnostics.push(format!("ignored_table_spans:{ignored_spans}"));
    }
    if invalid_json_data > 0 {
        diagnostics.push(format!("invalid_embedded_json:{invalid_json_data}"));
    }
    Ok(WebDocument {
        source_sha256: lower_hex(&Sha256::digest(source_bytes)),
        base_url,
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
        metadata,
        json_data,
        discovery_links,
        diagnostics,
    })
}

fn parse_assigned_json(raw: &str, prefix: &str) -> Option<serde_json::Value> {
    let rest = raw
        .trim_start()
        .strip_prefix(prefix)?
        .trim_start()
        .strip_prefix('=')?
        .trim_start();
    let mut stream = serde_json::Deserializer::from_str(rest).into_iter::<serde_json::Value>();
    let value = stream.next()?.ok()?;
    let suffix = rest[stream.byte_offset()..].trim_start();
    (suffix.is_empty() || suffix.starts_with(';')).then_some(value)
}

fn embedded_kind(element: ElementRef<'_>) -> Option<(&'static str, Option<&'static str>)> {
    let media = element
        .value()
        .attr("type")
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim();
    if media.eq_ignore_ascii_case("application/json") {
        let kind = match element.value().attr("id") {
            Some("__NEXT_DATA__") => "next_data",
            Some("__NUXT_DATA__") => "nuxt_data",
            _ => "application_json",
        };
        return Some((kind, None));
    }
    if !media.is_empty()
        && !media.eq_ignore_ascii_case("text/javascript")
        && !media.eq_ignore_ascii_case("application/javascript")
    {
        return None;
    }
    let first = element.text().next()?.trim_start();
    for prefix in [
        "window.__INITIAL_STATE__",
        "window.__PRELOADED_STATE__",
        "window.__APOLLO_STATE__",
        "window.__NEXT_DATA__",
        "window.__NUXT__",
        "var __INITIAL_STATE__",
        "var __PRELOADED_STATE__",
    ] {
        if first
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.trim_start().starts_with('='))
        {
            return Some(("initial_state", Some(prefix)));
        }
    }
    None
}

fn attr_eq(element: ElementRef<'_>, name: &str, expected: &str) -> bool {
    element
        .value()
        .attr(name)
        .is_some_and(|value| value.eq_ignore_ascii_case(expected))
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
    index: &DomIndex<'_>,
) -> Result<Vec<Vec<String>>, WebParseError> {
    // Spans carried down from earlier rows: sorted by column, never overlapping.
    let mut active: Vec<RowSpan> = Vec::new();
    let mut grid = Vec::new();
    let mut group = None;
    for row in rows {
        budget.add_node()?;
        // A cell cannot span past its row group (`thead`/`tbody`/`tfoot`).
        let row_group = row.parent().map(|parent| parent.id());
        if row_group != group {
            active.clear();
            group = row_group;
        }
        let mut out: Vec<String> = Vec::new();
        let mut carried: Vec<RowSpan> = Vec::new();
        let mut next_active = 0usize;
        let cells = row.children().filter_map(ElementRef::wrap).filter(|cell| {
            matches!(cell.value().name(), "th" | "td")
                && !index.info[&cell.id()].hidden
                && !index.info[&cell.id()].inert
        });
        for cell in cells {
            // Columns covered from above come before the next real cell.
            while let Some(span) = active.get(next_active).filter(|s| s.column <= out.len()) {
                cover(span, &mut out, &mut carried, budget)?;
                next_active += 1;
            }
            budget.add_node()?;
            let text = index.text(cell, TextMode::Normalized, true, budget)?;
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
fn own_table_parts<'a>(table: ElementRef<'a>, index: &DomIndex<'a>) -> TableParts<'a> {
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
        if index.info[&element.id()].hidden || index.info[&element.id()].inert {
            continue;
        }
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

fn owned_attr(
    element: ElementRef<'_>,
    name: &str,
    budget: &mut Budget,
) -> Result<Option<String>, WebParseError> {
    let value = element.value().attr(name);
    budget.optional_text(value)?;
    Ok(value.map(str::to_owned))
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

fn resolve_url(base: Option<&Url>, raw: Option<&str>) -> Option<String> {
    let raw = raw?;
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
    span_cells: usize,
    path_bytes: usize,
    nodes: usize,
    text_bytes: usize,
    max_nodes: usize,
    max_text_bytes: usize,
}

impl Budget {
    fn new(max_nodes: usize, max_text_bytes: usize) -> Self {
        Self {
            span_cells: 0,
            path_bytes: 0,
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

    fn add_path(&mut self, segment: &str) -> Result<(), WebParseError> {
        self.path_bytes = self.path_bytes.saturating_add(segment.len());
        if self.path_bytes > MAX_DOM_PATH_BYTES {
            return Err(WebParseError::DomPathLimitExceeded);
        }
        self.add_text(segment)
    }

    fn add_span_cells(&mut self, count: usize) -> Result<(), WebParseError> {
        self.span_cells = self.span_cells.saturating_add(count);
        if self.span_cells > MAX_TABLE_SPAN_CELLS {
            return Err(WebParseError::TableSpanLimitExceeded);
        }
        Ok(())
    }

    fn optional_text(&mut self, text: Option<&str>) -> Result<(), WebParseError> {
        if let Some(text) = text {
            self.add_text(text)?;
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
