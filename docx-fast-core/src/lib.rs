#![deny(missing_docs)]
#![warn(rust_2018_idioms)]

//! # docx-fast-core
//!
//! High-performance Word (.docx) parser with full OOXML support.
//!
//! ## Features
//!
//! - **Complete document extraction**: paragraphs, tables, headers, footers
//! - **Style resolution**: paragraph and character styles with inheritance
//! - **Numbering support**: lists, bullets, and multi-level numbering
//! - **Revision tracking**: insertions, deletions, and formatting changes
//! - **Comments and annotations**: threaded comments with replies
//! - **TOC generation**: automatic table of contents from heading styles
//! - **Image references**: embedded and external image collection
//! - **Arrow IPC** output format (optional feature)
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use docx_fast_core::{parse_docx, ParseOptions};
//!
//! let doc = parse_docx("document.docx", ParseOptions::default())?;
//! println!("Title: {:?}", doc.metadata.core.get("title"));
//! println!("Paragraphs: {}", doc.blocks.len());
//! # Ok::<(), anyhow::Error>(())
//! ```
//!
//! ## Document Structure
//!
//! ```text
//! DocxDocument
//! ├── metadata (core/app/custom properties)
//! ├── styles (paragraph, character, table styles)
//! ├── numbering (list definitions and levels)
//! ├── blocks (paragraphs and tables)
//! │   ├── Paragraph { runs: [Run { text, bold, italic, ... }] }
//! │   └── Table { rows: [Row { cells: [Cell { blocks }] }] }
//! ├── headers / footers
//! ├── footnotes / endnotes
//! ├── comments (with thread support)
//! ├── revisions (track changes)
//! └── images (embedded references)
//! ```
//!
//! ## Architecture
//!
//! The parser uses a state machine (`DocxBodyParser`) to handle nested
//! XML structures. Tables can contain paragraphs, which can contain
//! runs, which can contain text, comments, and revisions.
//!
//! # Files on disk
//!
//! Functions that take a path memory-map the file. It must not be modified
//! or truncated, by this process or any other, while it is being read: a
//! truncated file can make reads of the mapping fault (`SIGBUS` on Unix),
//! and a modified one can change the bytes under the parser. Functions that
//! take bytes have no such requirement.

use anyhow::{anyhow, Context, Result};
use chrono::DateTime;
use ooxml_archive::{
    parse_relationships, parse_simple_props, part_dir, relationship_matches, relationship_target,
    rels_path_for_part, resolve_part, source_part_from_rels, OoxmlArchive, PartReader, RelEntry,
    Relationships,
};
use quick_xml::events::{BytesRef, BytesText, Event};
use quick_xml::{Reader, XmlVersion};
use serde::Serialize;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg(feature = "arrow")]
/// Apache Arrow export helpers for shredding `.docx` documents into typed
/// table streams (metadata, paragraphs, runs, images, tables, comments, etc.).
pub mod arrow;
mod symbol;

// The GLiNER extractor and the API embedder that were declared here are
// consumer integrations: they depend on one product's SDK and belong in that
// product, not in a parser shared across consumers.

/// Configuration options for DOCX parsing.
///
/// # Example
///
/// ```rust
/// use docx_fast_core::ParseOptions;
///
/// let options = ParseOptions {
///     max_inflate: 64 * 1024 * 1024, // 64 MiB limit
///     ..ParseOptions::default()
/// };
/// ```
#[derive(Debug, Clone, Copy)]
pub struct ParseOptions {
    /// Maximum uncompressed size of any individual package part (bytes).
    ///
    /// Parsing fails before or during decompression when a part exceeds this
    /// safety ceiling. Default: 128 MiB.
    pub max_inflate: usize,
    /// Parse comments and commentsExtended. Default: `true`.
    pub extract_comments: bool,
    /// Collect image relationship references. Default: `true`.
    pub extract_images: bool,
    /// Parse headers and footers. Default: `true`.
    pub extract_headers: bool,
}

impl Default for ParseOptions {
    fn default() -> Self {
        Self {
            max_inflate: 128 * 1024 * 1024,
            extract_comments: true,
            extract_images: true,
            extract_headers: true,
        }
    }
}

/// The entries of `map` in key order.
///
/// A `HashMap` iterates in an order that changes from one process to the
/// next, so anything written from one (JSON, NDJSON, Arrow rows) goes
/// through here: the same input must give the same bytes every time.
pub(crate) fn sorted_entries<V>(map: &HashMap<String, V>) -> Vec<(&String, &V)> {
    let mut entries: Vec<_> = map.iter().collect();
    entries.sort_unstable_by(|left, right| left.0.cmp(right.0));
    entries
}

/// Serializes a map with its keys in order; see [`sorted_entries`].
fn serialize_sorted_map<S, V>(map: &HashMap<String, V>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
    V: Serialize,
{
    serializer.collect_map(sorted_entries(map))
}

/// [`serialize_sorted_map`] for a field that borrows its map.
fn serialize_sorted_map_ref<S, V>(
    map: &&HashMap<String, V>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
    V: Serialize,
{
    serialize_sorted_map(map, serializer)
}

/// Document metadata from core, app, and custom properties.
///
/// Corresponds to `docProps/core.xml`, `docProps/app.xml`, and
/// `docProps/custom.xml` in the OOXML package.
#[derive(Debug, Serialize, Default)]
pub struct Metadata {
    /// Dublin Core properties (title, creator, subject, etc.).
    #[serde(serialize_with = "serialize_sorted_map")]
    pub core: HashMap<String, String>,
    /// Application properties (Application, AppVersion, etc.).
    #[serde(serialize_with = "serialize_sorted_map")]
    pub app: HashMap<String, String>,
    /// Custom document properties defined by the user.
    #[serde(serialize_with = "serialize_sorted_map")]
    pub custom: HashMap<String, String>,
}

/// A complete Word document with all extracted content.
///
/// This is the main output type returned by [`parse_docx`].
#[derive(Debug, Serialize)]
pub struct DocxDocument {
    /// Document metadata (title, author, etc.).
    pub metadata: Metadata,
    /// Document settings (compatibility, proofing, etc.).
    #[serde(serialize_with = "serialize_sorted_map")]
    pub settings: HashMap<String, String>,
    /// Page count if available in app properties.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_count: Option<u32>,
    /// Generated table of contents from heading styles.
    pub toc: Vec<TocEntry>,
    /// All styles defined in the document.
    pub styles: Vec<Style>,
    /// Numbering definitions for lists and bullets.
    pub numbering: Numbering,
    /// Footnotes in the document.
    pub footnotes: Vec<Note>,
    /// Endnotes in the document.
    pub endnotes: Vec<Note>,
    /// Comments and annotations.
    pub comments: Vec<Comment>,
    /// Tracked changes (insertions, deletions, formatting).
    pub revisions: Vec<Revision>,
    /// Field codes (TOC, page numbers, etc.).
    pub fields: Vec<Field>,
    /// Document headers (first page, odd, even).
    pub headers: Vec<HeaderFooter>,
    /// Document footers (first page, odd, even).
    pub footers: Vec<HeaderFooter>,
    /// Main document body (paragraphs and tables).
    pub blocks: Vec<Block>,
    /// References to embedded images.
    pub images: Vec<ImageRef>,
}

/// Entry in the automatically generated table of contents.
#[derive(Debug, Serialize)]
pub struct TocEntry {
    /// The outline level (nesting depth) of this entry.
    pub level: u8,
    /// The display text of this entry.
    pub text: String,
    /// The source type of this entry (e.g., "heading").
    pub source: String,
    /// Optional ID of the paragraph this entry refers to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paragraph_id: Option<String>,
    /// Optional style ID used to determine TOC level.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub style_id: Option<String>,
}

/// Header or footer section of a document.
#[derive(Debug, Serialize)]
pub struct HeaderFooter {
    /// The type of header/footer: "header" or "footer".
    pub kind: String,
    /// Optional type attribute (e.g., "default", "first", "even").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ref_type: Option<String>,
    /// The internal part path (e.g., "word/header1.xml").
    pub part: String,
    /// The paragraphs and tables contained in this header/footer.
    pub blocks: Vec<Block>,
}

/// A paragraph or character style definition with inheritance relationships.
#[derive(Debug, Serialize)]
pub struct Style {
    /// The unique identifier for this style.
    pub id: String,
    /// The user-facing name of this style.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The type of style: "paragraph", "character", "table", or "list".
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub style_type: Option<String>,
    /// The ID of the parent style this style inherits from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub based_on: Option<String>,
    /// The ID of the style that follows this one (usually for paragraph styles).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    /// The outline level for heading styles (1-9).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outline_level: Option<u8>,
    /// Priority for display in the styles UI (lower = higher priority).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ui_priority: Option<u8>,
    /// Whether this is the default style for its type.
    pub is_default: bool,
    /// Whether this is a quick format (frequently used) style.
    pub is_qformat: bool,
}

/// Document-level numbering and list formatting system with abstract definitions and instances.
#[derive(Debug, Serialize, Default)]
pub struct Numbering {
    /// Abstract numbering definitions (templates for list formatting).
    pub abstract_nums: Vec<AbstractNum>,
    /// Numbered list instances that reference abstract numbering definitions.
    pub nums: Vec<Num>,
}

/// Abstract definition of a numbered/bulleted list with formatting for each level.
#[derive(Debug, Serialize)]
pub struct AbstractNum {
    /// Unique identifier for this abstract numbering definition.
    pub id: u32,
    /// Formatting for each nesting level (0-8).
    pub levels: Vec<NumLevel>,
}

/// Formatting for one level within a numbered or bulleted list.
#[derive(Debug, Serialize)]
pub struct NumLevel {
    /// The nesting level (0-8).
    pub level: u8,
    /// The numbering format (e.g., "decimal", "bullet", "lowerRoman").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// The template text for this level (e.g., "%1." for "1.").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The starting number for this level.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<u32>,
}

/// Instance of an abstract numbering definition applied to paragraphs.
#[derive(Debug, Serialize)]
pub struct Num {
    /// The unique identifier of this numbering instance.
    pub id: u32,
    /// Reference to an abstract numbering definition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub abstract_id: Option<u32>,
}

/// Footnote or endnote with its content and identifier.
#[derive(Debug, Serialize)]
pub struct Note {
    /// The unique identifier of this note.
    pub id: i32,
    /// The paragraphs and tables contained in this note.
    pub blocks: Vec<Block>,
}

/// Comment with metadata and optional threading support for replies.
#[derive(Debug, Serialize)]
pub struct Comment {
    /// The unique identifier of this comment.
    pub id: i32,
    /// The author of the comment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// Author initials displayed in the comment box.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initials: Option<String>,
    /// Date and time when the comment was created.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    /// ID of the parent comment if this is a reply in a threaded discussion.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<i32>,
    /// The paragraphs and tables that make up the comment content.
    pub blocks: Vec<Block>,
}

/// Tracked change (insertion, deletion, formatting change, etc.).
#[derive(Debug, Serialize)]
pub struct Revision {
    /// The unique identifier of this revision.
    pub id: String,
    /// The type of revision: "ins" (insertion), "del" (deletion), or other tracked change types.
    pub kind: String,
    /// The author who made this change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// Date and time when this change was made.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
}

/// Field code (e.g., TOC, page number, merge field, or expression).
#[derive(Debug, Serialize)]
pub struct Field {
    /// The unique identifier of this field.
    pub id: u32,
    /// The field code instruction (e.g., "TOC \\o 1-3" for table of contents).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instr: Option<String>,
    /// The computed result or value of the field.
    pub result: String,
    /// The type of field (e.g., "TOC", "PAGE", "MERGE").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// Reference to an embedded or external image within the document.
#[derive(Debug, Serialize)]
pub struct ImageRef {
    /// The unique identifier for this image.
    pub id: String,
    /// The source part ID within the OOXML structure.
    pub source_part: String,
    /// The target path to the image file (relative within the .docx ZIP).
    pub target: String,
    /// The MIME type of the image (e.g., "image/png", "image/jpeg").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
}

/// Top-level block element in a document: either a paragraph or a table.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Block {
    /// A paragraph containing runs of formatted text.
    Paragraph(Paragraph),
    /// A table containing rows and cells.
    Table(Table),
}

/// A paragraph containing runs of formatted text.
#[derive(Debug, Serialize)]
pub struct Paragraph {
    /// The unique identifier of this paragraph.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The style ID applied to this paragraph.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub style_id: Option<String>,
    /// The human-readable style name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub style_name: Option<String>,
    /// For heading styles, the outline level (1-9).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outline_level: Option<u8>,
    /// Reference to numbering (lists) applied to this paragraph.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub numbering: Option<NumberingRef>,
    /// For table of contents tracking.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toc_level: Option<u8>,
    /// The runs (text spans) that make up this paragraph.
    pub runs: Vec<Run>,
    /// The complete text content of this paragraph.
    pub text: String,
}

/// Reference from a paragraph to list numbering.
#[derive(Debug, Serialize)]
pub struct NumberingRef {
    /// ID of the numbering instance.
    pub num_id: u32,
    /// The level within the numbering (0-8).
    pub level: u8,
}

/// A run is a span of text with consistent formatting properties within a paragraph.
#[derive(Debug, Serialize)]
pub struct Run {
    /// The text content of this run.
    pub text: String,
    /// The character style ID applied to this run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub style_id: Option<String>,
    /// Whether this run is bold.
    pub bold: bool,
    /// Whether this run is italic.
    pub italic: bool,
    /// Whether this run is underlined.
    pub underline: bool,
    /// ID of a field containing this run (for field code computation).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field_id: Option<u32>,
    /// Reference to a tracked change applied to this run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<RevisionRef>,
    /// IDs of comments anchored to this run.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub comment_ids: Vec<i32>,
    /// Footnote and endnote references anchored to this run.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub note_references: Vec<NoteReference>,
}

/// Reference from a run to a footnote or endnote body.
#[derive(Debug, Clone, Serialize)]
pub struct NoteReference {
    /// Reference kind: `footnote` or `endnote`.
    pub kind: String,
    /// Note identifier from the corresponding notes part.
    pub id: i32,
}

/// Reference to a tracked change applied to a run.
#[derive(Debug, Serialize)]
pub struct RevisionRef {
    /// The ID of the revision this run belongs to.
    pub id: String,
    /// The kind of revision ("ins", "del", etc.).
    pub kind: String,
}

/// A table containing rows and cells.
#[derive(Debug, Serialize)]
pub struct Table {
    /// The rows in this table.
    pub rows: Vec<TableRow>,
}

/// A row in a table containing cells.
#[derive(Debug, Serialize)]
pub struct TableRow {
    /// The cells in this row.
    pub cells: Vec<TableCell>,
}

/// A cell in a table row, containing block-level content.
#[derive(Debug, Serialize)]
pub struct TableCell {
    /// The block-level content (paragraphs, nested tables) in this cell.
    pub blocks: Vec<Block>,
}

type DocxArchive = OoxmlArchive;

fn read_optional_part_bytes<'a>(
    archive: &'a DocxArchive,
    name: &str,
    max_inflate: usize,
) -> Result<Option<ooxml_archive::PartBytes<'a>>> {
    if !archive.has_part(name) {
        return Ok(None);
    }
    read_part_bytes(archive, name, max_inflate)
        .map(Some)
        .with_context(|| format!("read optional DOCX part: {name}"))
}

fn open_optional_part_reader<'a>(
    archive: &'a DocxArchive,
    name: &str,
    max_inflate: usize,
) -> Result<Option<PartReader<'a>>> {
    if !archive.has_part(name) {
        return Ok(None);
    }
    open_part_reader(archive, name, max_inflate)
        .map(Some)
        .with_context(|| format!("read optional DOCX part: {name}"))
}

fn open_archive(path: &Path) -> Result<DocxArchive> {
    OoxmlArchive::open(path)
}

fn open_archive_bytes(bytes: Vec<u8>) -> Result<DocxArchive> {
    OoxmlArchive::from_bytes(bytes)
}

fn read_part_bytes<'a>(
    archive: &'a DocxArchive,
    name: &str,
    max_inflate: usize,
) -> Result<ooxml_archive::PartBytes<'a>> {
    archive.read_part(name, max_inflate)
}

fn open_part_reader<'a>(
    archive: &'a DocxArchive,
    name: &str,
    max_inflate: usize,
) -> Result<PartReader<'a>> {
    archive.open_part_reader(name, max_inflate, false)
}

#[derive(Debug)]
struct HeaderFooterRef {
    kind: String,
    rel_id: String,
    ref_type: Option<String>,
}

/// Reads one `headerReference` or `footerReference` element.
///
/// Returns `None` for any other element, and for a reference without a
/// relationship id.
fn header_footer_ref<R: BufRead>(
    reader: &Reader<R>,
    e: &quick_xml::events::BytesStart<'_>,
    name: &[u8],
) -> Result<Option<HeaderFooterRef>> {
    if name != b"headerReference" && name != b"footerReference" {
        return Ok(None);
    }
    let mut rel_id = None;
    let mut ref_type = None;
    for attr in e.attributes().with_checks(false).flatten() {
        let key_raw = attr.key.as_ref();
        let key = local_name(key_raw);
        if key_raw == b"r:id" || key == b"id" {
            rel_id = Some(attribute_string(&attr, reader.decoder()));
        } else if key == b"type" {
            ref_type = Some(attribute_string(&attr, reader.decoder()));
        }
    }
    let kind = if name == b"headerReference" {
        "header"
    } else {
        "footer"
    };
    Ok(rel_id.map(|rel_id| HeaderFooterRef {
        kind: kind.to_string(),
        rel_id,
        ref_type,
    }))
}

/// Finds the header and footer references of a document part in a pass of
/// its own.
///
/// The usual route collects them while the body is parsed
/// ([`parse_document_blocks_with_refs`]). This pass is what that route falls
/// back to, and its error is the one a malformed document reports.
fn parse_header_footer_refs<R: BufRead>(reader: R) -> Result<Vec<HeaderFooterRef>> {
    PASS_COUNTS.with(|counts| counts.set((counts.get().0, counts.get().1 + 1)));
    let mut reader = xml_reader(utf8_stream(reader)?);
    let mut buf = Vec::new();
    let mut refs = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let qname = e.name();
                let name = local_name(qname.as_ref());
                if let Some(reference) = header_footer_ref(&reader, &e, name)? {
                    refs.push(reference);
                }
            }
            Ok(Event::Eof) => break,
            Err(err) => return Err(anyhow!("parse header/footer refs: {err}")),
            _ => {}
        }
        buf.clear();
    }
    Ok(refs)
}

const REL_STYLES: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles";
const REL_NUMBERING: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering";
const REL_FOOTNOTES: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/footnotes";
const REL_ENDNOTES: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/endnotes";
const REL_COMMENTS: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments";
const REL_COMMENTS_EXT: &str =
    "http://schemas.microsoft.com/office/2011/relationships/commentsExtended";
const REL_IMAGE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/image";
#[allow(dead_code)]
const REL_HEADER: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/header";
#[allow(dead_code)]
const REL_FOOTER: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/footer";

/// Parse a Word document (.docx) file and extract its content and metadata.
///
/// Reads the OOXML structure, extracts text, formatting, styles, numbering, tracked changes, comments, and embedded images.
/// Returns a fully structured representation of the document.
pub fn parse_docx(path: impl AsRef<Path>, options: ParseOptions) -> Result<DocxDocument> {
    let path = path.as_ref();
    let archive = open_archive(path)?;
    parse_docx_from_archive(archive, options)
}

/// Parse a Word document (.docx) from raw bytes.
///
/// Accepts the complete OOXML package as a byte slice (e.g. from an HTTP
/// upload or in-memory buffer) and returns a fully structured
/// [`DocxDocument`].  No temporary file is written — the bytes are
/// wrapped in an owned archive buffer and parsed directly.
pub fn parse_docx_bytes(bytes: Vec<u8>, options: ParseOptions) -> Result<DocxDocument> {
    let archive = open_archive_bytes(bytes)?;
    parse_docx_from_archive(archive, options)
}

/// Internal: parse a DOCX from an already-open archive.
fn parse_docx_from_archive(archive: DocxArchive, options: ParseOptions) -> Result<DocxDocument> {
    let document_path = archive.office_document_path(options.max_inflate, "word/document.xml")?;

    let document_dir = part_dir(&document_path);
    let document_rels_path = rels_path_for_part(&document_path);
    // The part the relationships were read from, so that nothing reads it a
    // second time: every read counts against `max_inflate`.
    let mut document_rels_part = None;
    let fallback_rels_part;
    let document_rels = if archive.has_part(&document_rels_path) {
        let xml = read_part_bytes(&archive, &document_rels_path, options.max_inflate)
            .with_context(|| format!("read document rels: {document_rels_path}"))?;
        document_rels_part = Some(document_rels_path.as_str());
        parse_relationships(&utf8_xml(&xml))?
    } else {
        // Compatibility fallback for malformed names seen in legacy producers.
        let fallback = document_rels_path.replace("document.xml.rels", "document.xm.rels");
        if fallback != document_rels_path && archive.has_part(&fallback) {
            let xml = read_part_bytes(&archive, &fallback, options.max_inflate)
                .with_context(|| format!("read document rels fallback: {fallback}"))?;
            fallback_rels_part = fallback;
            document_rels_part = Some(fallback_rels_part.as_str());
            parse_relationships(&utf8_xml(&xml))?
        } else {
            Relationships::default()
        }
    };

    let styles_path =
        relationship_target(&document_rels, REL_STYLES).map(|t| resolve_part(&document_dir, t));
    let numbering_path =
        relationship_target(&document_rels, REL_NUMBERING).map(|t| resolve_part(&document_dir, t));
    let footnotes_path =
        relationship_target(&document_rels, REL_FOOTNOTES).map(|t| resolve_part(&document_dir, t));
    let endnotes_path =
        relationship_target(&document_rels, REL_ENDNOTES).map(|t| resolve_part(&document_dir, t));
    let comments_path =
        relationship_target(&document_rels, REL_COMMENTS).map(|t| resolve_part(&document_dir, t));
    let comments_ext_path = relationship_target(&document_rels, REL_COMMENTS_EXT)
        .map(|t| resolve_part(&document_dir, t));

    let (styles, style_fonts) = if let Some(path) = styles_path.as_deref() {
        if let Some(xml) = read_optional_part_bytes(&archive, path, options.max_inflate)? {
            (parse_styles(&xml)?, parse_style_fonts(&xml)?)
        } else {
            (Vec::new(), StyleFonts::default())
        }
    } else {
        (Vec::new(), StyleFonts::default())
    };

    let numbering = if let Some(path) = numbering_path.as_deref() {
        if let Some(xml) = read_optional_part_bytes(&archive, path, options.max_inflate)? {
            parse_numbering(&xml)?
        } else {
            Numbering::default()
        }
    } else {
        Numbering::default()
    };

    let footnotes = if let Some(path) = footnotes_path.as_deref() {
        if let Some(xml) = read_optional_part_bytes(&archive, path, options.max_inflate)? {
            parse_notes(&xml, b"footnote", &styles, &style_fonts)?
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };

    let endnotes = if let Some(path) = endnotes_path.as_deref() {
        if let Some(xml) = read_optional_part_bytes(&archive, path, options.max_inflate)? {
            parse_notes(&xml, b"endnote", &styles, &style_fonts)?
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };

    let mut comments = if options.extract_comments {
        if let Some(path) = comments_path.as_deref() {
            if let Some(xml) = read_optional_part_bytes(&archive, path, options.max_inflate)? {
                parse_comments(&xml, &styles, &style_fonts)?
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };

    if options.extract_comments {
        if let Some(path) = comments_ext_path.as_deref() {
            if let Some(xml) = read_optional_part_bytes(&archive, path, options.max_inflate)? {
                apply_extended_comments(&mut comments, &xml)?;
            }
        }
    }

    let metadata = parse_metadata(&archive, options.max_inflate)?;
    let settings = parse_settings(&archive, options.max_inflate)?;
    let page_count = metadata
        .app
        .get("Pages")
        .and_then(|v| v.parse::<u32>().ok());
    let images = if options.extract_images {
        let known = document_rels_part.map(|part| (part, &document_rels));
        collect_images(&archive, options.max_inflate, known)?
    } else {
        Vec::new()
    };

    let mut fields = Vec::new();
    let mut revisions = Vec::new();
    // The document part is needed twice, for the header and footer
    // references and for the body. It is read once: a second read would
    // count it against `max_inflate` a second time.
    let document_xml = read_part_bytes(&archive, &document_path, options.max_inflate)
        .with_context(|| format!("read document: {document_path}"))?;
    let document_xml = utf8_xml(&document_xml);
    // The references are needed before the body, because headers and footers
    // are parsed first and number their fields first. Finding them used to
    // take a pass of its own over the whole document part. Instead the body
    // is parsed now, collecting the references on the way, into vectors of
    // its own, and is moved behind the headers and footers afterwards (see
    // [`shift_field_ids`] for what that does to field numbers).
    //
    // A body that does not parse is not parsed again. The order used to be
    // reference search, headers and footers, body, and the error reported
    // must stay the first one that order meets:
    //
    // - ill-formed XML, or a reference that cannot be read, stops the
    //   reference search at the same place it stopped the body, so that
    //   error is known already ([`BodyFailure::ReferenceSearch`]);
    // - any other failure belongs to the body alone. The reference search
    //   and the headers and footers are then run to see whether one of them
    //   fails first, and if none does the body's error is returned
    //   (`body_error` below).
    let mut body_ahead = None;
    let mut body_error = None;
    let header_footer_refs = if options.extract_headers {
        let mut body_fields = Vec::new();
        let mut body_revisions = Vec::new();
        match parse_document_blocks_with_refs(
            Cursor::new(&*document_xml),
            &styles,
            &style_fonts,
            &mut body_fields,
            &mut body_revisions,
        ) {
            Ok((blocks, refs)) => {
                body_ahead = Some((blocks, body_fields, body_revisions));
                refs
            }
            Err(BodyFailure::ReferenceSearch(error)) => return Err(error),
            Err(BodyFailure::Body(error)) => {
                body_error = Some(error);
                parse_header_footer_refs(Cursor::new(&*document_xml))?
            }
        }
    } else {
        Vec::new()
    };
    let mut headers = Vec::new();
    let mut footers = Vec::new();
    for reference in header_footer_refs {
        if let Some(target) = document_rels.by_id.get(&reference.rel_id) {
            let part = resolve_part(&document_dir, target);
            if let Some(reader) = open_optional_part_reader(&archive, &part, options.max_inflate)? {
                let blocks = parse_document_blocks(
                    reader,
                    &styles,
                    &style_fonts,
                    &mut fields,
                    &mut revisions,
                )?;
                let entry = HeaderFooter {
                    kind: reference.kind,
                    ref_type: reference.ref_type,
                    part: part.clone(),
                    blocks,
                };
                if entry.kind == "header" {
                    headers.push(entry);
                } else {
                    footers.push(entry);
                }
            }
        }
    }

    if let Some(error) = body_error {
        return Err(error);
    }
    let blocks = match body_ahead {
        Some((mut blocks, mut body_fields, body_revisions)) => {
            // Field numbers stay within `u32` however many fields there are:
            // the parser itself numbers them with the same width.
            let earlier = fields.len() as u32;
            if earlier > 0 {
                for field in &mut body_fields {
                    field.id = field.id.wrapping_add(earlier);
                }
                shift_field_ids(&mut blocks, earlier);
            }
            fields.append(&mut body_fields);
            revisions.extend(body_revisions);
            blocks
        }
        None => parse_document_blocks(
            Cursor::new(&*document_xml),
            &styles,
            &style_fonts,
            &mut fields,
            &mut revisions,
        )?,
    };

    let mut toc = Vec::new();
    collect_toc(&blocks, &mut toc);

    Ok(DocxDocument {
        metadata,
        settings,
        page_count,
        toc,
        styles,
        numbering,
        footnotes,
        endnotes,
        comments,
        revisions,
        fields,
        headers,
        footers,
        blocks,
        images,
    })
}

/// Parse a Word document and serialize it to newline-delimited JSON (NDJSON) format.
///
/// Each line contains a JSON object representing a document component (metadata, styles, paragraphs, tables, etc.).
/// Useful for streaming processing and pipeline integration.
pub fn parse_docx_to_ndjson(
    path: impl AsRef<Path>,
    out_path: impl AsRef<Path>,
    options: ParseOptions,
) -> Result<()> {
    let doc = parse_docx(path, options)?;
    let mut out = std::io::BufWriter::new(File::create(out_path.as_ref())?);

    serde_json::to_writer(
        &mut out,
        &NdjsonHeader {
            kind: "metadata",
            metadata: &doc.metadata,
            settings: &doc.settings,
            page_count: doc.page_count,
            toc: &doc.toc,
        },
    )?;
    writeln_nl(&mut out)?;

    for style in &doc.styles {
        serde_json::to_writer(
            &mut out,
            &NdjsonStyle {
                kind: "style",
                style,
            },
        )?;
        writeln_nl(&mut out)?;
    }

    serde_json::to_writer(
        &mut out,
        &NdjsonNumbering {
            kind: "numbering",
            numbering: &doc.numbering,
        },
    )?;
    writeln_nl(&mut out)?;

    for note in &doc.footnotes {
        serde_json::to_writer(
            &mut out,
            &NdjsonNote {
                kind: "footnote",
                note,
            },
        )?;
        writeln_nl(&mut out)?;
    }
    for note in &doc.endnotes {
        serde_json::to_writer(
            &mut out,
            &NdjsonNote {
                kind: "endnote",
                note,
            },
        )?;
        writeln_nl(&mut out)?;
    }
    for comment in &doc.comments {
        serde_json::to_writer(
            &mut out,
            &NdjsonComment {
                kind: "comment",
                comment,
            },
        )?;
        writeln_nl(&mut out)?;
    }
    for revision in &doc.revisions {
        serde_json::to_writer(
            &mut out,
            &NdjsonRevision {
                kind: "revision",
                revision,
            },
        )?;
        writeln_nl(&mut out)?;
    }
    for field in &doc.fields {
        serde_json::to_writer(
            &mut out,
            &NdjsonField {
                kind: "field",
                field,
            },
        )?;
        writeln_nl(&mut out)?;
    }
    for header in &doc.headers {
        serde_json::to_writer(
            &mut out,
            &NdjsonHeaderFooter {
                kind: "header",
                entry: header,
            },
        )?;
        writeln_nl(&mut out)?;
    }
    for footer in &doc.footers {
        serde_json::to_writer(
            &mut out,
            &NdjsonHeaderFooter {
                kind: "footer",
                entry: footer,
            },
        )?;
        writeln_nl(&mut out)?;
    }
    for image in &doc.images {
        serde_json::to_writer(
            &mut out,
            &NdjsonImage {
                kind: "image",
                image,
            },
        )?;
        writeln_nl(&mut out)?;
    }
    for block in &doc.blocks {
        serde_json::to_writer(
            &mut out,
            &NdjsonBlock {
                kind: "block",
                block,
            },
        )?;
        writeln_nl(&mut out)?;
    }
    Ok(())
}

#[derive(Serialize)]
struct NdjsonHeader<'a> {
    #[serde(rename = "kind")]
    kind: &'static str,
    metadata: &'a Metadata,
    #[serde(serialize_with = "serialize_sorted_map_ref")]
    settings: &'a HashMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    page_count: Option<u32>,
    toc: &'a Vec<TocEntry>,
}

#[derive(Serialize)]
struct NdjsonStyle<'a> {
    #[serde(rename = "kind")]
    kind: &'static str,
    style: &'a Style,
}

#[derive(Serialize)]
struct NdjsonNumbering<'a> {
    #[serde(rename = "kind")]
    kind: &'static str,
    numbering: &'a Numbering,
}

#[derive(Serialize)]
struct NdjsonNote<'a> {
    #[serde(rename = "kind")]
    kind: &'static str,
    note: &'a Note,
}

#[derive(Serialize)]
struct NdjsonComment<'a> {
    #[serde(rename = "kind")]
    kind: &'static str,
    comment: &'a Comment,
}

#[derive(Serialize)]
struct NdjsonRevision<'a> {
    #[serde(rename = "kind")]
    kind: &'static str,
    revision: &'a Revision,
}

#[derive(Serialize)]
struct NdjsonField<'a> {
    #[serde(rename = "kind")]
    kind: &'static str,
    field: &'a Field,
}

#[derive(Serialize)]
struct NdjsonHeaderFooter<'a> {
    #[serde(rename = "kind")]
    kind: &'static str,
    entry: &'a HeaderFooter,
}

#[derive(Serialize)]
struct NdjsonImage<'a> {
    #[serde(rename = "kind")]
    kind: &'static str,
    image: &'a ImageRef,
}

#[derive(Serialize)]
struct NdjsonBlock<'a> {
    #[serde(rename = "kind")]
    kind: &'static str,
    block: &'a Block,
}

fn writeln_nl(out: &mut std::io::BufWriter<File>) -> Result<()> {
    use std::io::Write;
    out.write_all(b"\n")?;
    Ok(())
}

/// Extract all embedded images from a Word document and save them to a directory.
///
/// Returns a vector of paths to the extracted image files. Images are saved with their original filenames.
pub fn extract_docx_images(
    path: impl AsRef<Path>,
    out_dir: impl AsRef<Path>,
) -> Result<Vec<PathBuf>> {
    let archive = open_archive(path.as_ref())?;
    let images = collect_images(&archive, 256 * 1024 * 1024, None)?;
    let mut out_paths = Vec::new();
    let out_dir = out_dir.as_ref();
    std::fs::create_dir_all(out_dir)?;

    for image in images {
        let target = image.target.trim_start_matches('/');
        if let Ok(bytes) = read_part_bytes(&archive, target, 256 * 1024 * 1024) {
            let name = Path::new(target)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("image");
            let out_path = out_dir.join(name);
            std::fs::write(&out_path, bytes)?;
            out_paths.push(out_path);
        }
    }

    Ok(out_paths)
}

fn parse_metadata(archive: &DocxArchive, max_inflate: usize) -> Result<Metadata> {
    let mut metadata = Metadata::default();

    if let Ok(xml) = read_part_bytes(archive, "docProps/core.xml", max_inflate) {
        metadata.core = parse_simple_props(&utf8_xml(&xml))?;
    }
    if let Ok(xml) = read_part_bytes(archive, "docProps/app.xml", max_inflate) {
        metadata.app = parse_simple_props(&utf8_xml(&xml))?;
    }
    if let Ok(xml) = read_part_bytes(archive, "docProps/custom.xml", max_inflate) {
        metadata.custom = parse_custom_props(&xml)?;
    }

    Ok(metadata)
}

fn parse_settings(archive: &DocxArchive, max_inflate: usize) -> Result<HashMap<String, String>> {
    // The part is optional: a minimal document has no settings.xml and that is
    // not a defect. A part that exists and cannot be read is a defect.
    let Some(xml) = read_optional_part_bytes(archive, "word/settings.xml", max_inflate)? else {
        return Ok(HashMap::new());
    };
    let mut reader = xml_reader(Cursor::new(utf8_xml(&xml)));
    let mut buf = Vec::new();
    let mut settings = HashMap::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let qname = e.name();
                let name = local_name(qname.as_ref());
                if name == b"settings" {
                    continue;
                }
                let mut value = None;
                let mut attrs = Vec::new();
                for attr in e.attributes().with_checks(false).flatten() {
                    let key = local_name(attr.key.as_ref());
                    let val = attribute_string(&attr, reader.decoder());
                    if key == b"val" {
                        value = Some(val.clone());
                    }
                    attrs.push(format!("{}={}", String::from_utf8_lossy(key), val));
                }
                let entry = if let Some(value) = value {
                    value
                } else if !attrs.is_empty() {
                    attrs.join(";")
                } else {
                    "true".to_string()
                };
                settings
                    .entry(String::from_utf8_lossy(name).to_string())
                    .or_insert(entry);
            }
            Ok(Event::Eof) => break,
            Err(err) => return Err(anyhow!("parse settings: {err}")),
            _ => {}
        }
        buf.clear();
    }
    Ok(settings)
}

/// Stores the character data collected for `key`, if there is any.
///
/// A value can arrive in several pieces (text, a reference, a CDATA
/// section), so it is collected until something other than character data
/// ends it. The first run of character data after the key is the value;
/// taking the key stops later runs from replacing it.
///
/// A run of nothing but whitespace is the layout of the file, not a value,
/// in two cases: when another element opens right after it
/// (`before_element`), which is the indentation between two elements and
/// leaves the key open for the value element that follows; and when it
/// holds a line break, which is the indentation inside an element that has
/// no content. Spaces that fill an element up to its end tag are a value.
fn store_property(
    out: &mut HashMap<String, String>,
    key: &mut Option<String>,
    value: &mut String,
    before_element: bool,
) {
    let is_whitespace = value
        .chars()
        .all(|ch| matches!(ch, ' ' | '\t' | '\r' | '\n'));
    let is_layout = is_whitespace && (before_element || value.contains(['\r', '\n']));
    if !value.is_empty() && !is_layout {
        if let Some(key) = key.take() {
            out.insert(key, normalize_datetime(value));
        }
    }
    value.clear();
}

/// Reads `docProps/custom.xml`.
///
/// A property is a `property` element; its name is the `name` attribute and
/// its value the text of the value element inside it.
fn parse_custom_props(xml: &[u8]) -> Result<HashMap<String, String>> {
    let mut reader = xml_reader(Cursor::new(utf8_xml(xml)));
    let mut buf = Vec::new();
    let mut out = HashMap::new();
    let mut current_name: Option<String> = None;
    let mut value = String::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(event @ (Event::Text(_) | Event::CData(_) | Event::GeneralRef(_))) => {
                if current_name.is_some() {
                    value.push_str(&character_data(&event).unwrap_or_default());
                }
            }
            Ok(event) => {
                let before_element = matches!(event, Event::Start(_) | Event::Empty(_));
                store_property(&mut out, &mut current_name, &mut value, before_element);
                match event {
                    Event::Start(e) => {
                        let raw = e.name().as_ref().to_vec();
                        if local_name(&raw) == b"property" {
                            for attr in e.attributes().with_checks(false).flatten() {
                                if local_name(attr.key.as_ref()) == b"name" {
                                    current_name = Some(attribute_string(&attr, reader.decoder()));
                                }
                            }
                        }
                    }
                    Event::End(e) => {
                        let raw = e.name().as_ref().to_vec();
                        if local_name(&raw) == b"property" {
                            current_name = None;
                        }
                    }
                    Event::Eof => break,
                    _ => {}
                }
            }
            Err(err) => return Err(anyhow!("parse custom props: {err}")),
        }
        buf.clear();
    }
    Ok(out)
}

fn normalize_datetime(value: &str) -> String {
    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        return dt.to_rfc3339();
    }
    value.to_string()
}

fn parse_styles(xml: &[u8]) -> Result<Vec<Style>> {
    let mut reader = xml_reader(Cursor::new(utf8_xml(xml)));
    let mut buf = Vec::new();
    let mut styles = Vec::new();
    let mut current: Option<Style> = None;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let qname = e.name();
                let name = local_name(qname.as_ref());
                if name == b"style" {
                    let mut style = Style {
                        id: String::new(),
                        name: None,
                        style_type: None,
                        based_on: None,
                        next: None,
                        outline_level: None,
                        ui_priority: None,
                        is_default: false,
                        is_qformat: false,
                    };
                    for attr in e.attributes().with_checks(false).flatten() {
                        let key = local_name(attr.key.as_ref());
                        if key == b"styleId" {
                            style.id = attribute_string(&attr, reader.decoder());
                        } else if key == b"type" {
                            style.style_type = Some(attribute_string(&attr, reader.decoder()));
                        } else if key == b"default" {
                            let val = attribute_string(&attr, reader.decoder());
                            style.is_default = val == "1" || val.eq_ignore_ascii_case("true");
                        }
                    }
                    current = Some(style);
                } else if let Some(style) = current.as_mut() {
                    if name == b"name" {
                        if let Some(val) = attr_value(&reader, &e, b"val")? {
                            style.name = Some(val);
                        }
                    } else if name == b"basedOn" {
                        style.based_on = attr_value(&reader, &e, b"val")?;
                    } else if name == b"next" {
                        style.next = attr_value(&reader, &e, b"val")?;
                    } else if name == b"uiPriority" {
                        if let Some(val) = attr_value(&reader, &e, b"val")? {
                            style.ui_priority = val.parse::<u8>().ok();
                        }
                    } else if name == b"qFormat" {
                        style.is_qformat = true;
                    } else if name == b"outlineLvl" {
                        if let Some(val) = attr_value(&reader, &e, b"val")? {
                            style.outline_level = val.parse::<u8>().ok();
                        }
                    }
                }
            }
            Ok(Event::End(e)) => {
                let raw = e.name().as_ref().to_vec();
                if local_name(&raw) == b"style" {
                    if let Some(style) = current.take() {
                        styles.push(style);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(err) => return Err(anyhow!("parse styles: {err}")),
            _ => {}
        }
        buf.clear();
    }
    Ok(styles)
}

/// Most `basedOn` links followed from one style.
///
/// Real style sheets nest a handful of levels. The limit is what stops a
/// file whose styles are based on each other in a circle.
const MAX_STYLE_CHAIN: usize = 64;

/// The fonts a run inherits from its styles.
///
/// Kept apart from [`Style`], which is part of the output and does not carry
/// fonts.
#[derive(Debug, Default)]
struct StyleFonts {
    /// The fonts of each style, with those of the styles it is based on
    /// already filled in. A style that names no font anywhere in its chain
    /// has no entry.
    by_id: HashMap<String, symbol::FontChoice>,
    /// The fonts of the default paragraph style, which a paragraph without a
    /// `w:pStyle` uses.
    default_paragraph: symbol::FontChoice,
    /// The fonts of the default table style, which a table without a
    /// `w:tblStyle` uses.
    default_table: symbol::FontChoice,
    /// The fonts of `w:docDefaults` (`w:rPrDefault`), behind every style.
    defaults: symbol::FontChoice,
}

impl StyleFonts {
    /// Whether neither a style nor the document defaults name a font, so
    /// only a run's own font matters.
    fn is_empty(&self) -> bool {
        self.by_id.is_empty() && self.defaults.is_inherit()
    }

    fn of(&self, style_id: &str) -> symbol::FontChoice {
        self.by_id.get(style_id).copied().unwrap_or_default()
    }

    /// The fonts in effect for a run, nearest formatting first: the run's
    /// own, its character style, the paragraph's style, the style of the
    /// table the run is in, then the document defaults (ECMA-376 part 1,
    /// 17.7.2).
    ///
    /// `table_style` is `None` outside a table, and inside one the table's
    /// `w:tblStyle`, itself `None` for a table that names no style.
    fn effective(
        &self,
        run: symbol::FontChoice,
        run_style: Option<&str>,
        paragraph_style: Option<&str>,
        table_style: Option<Option<&str>>,
    ) -> symbol::FontChoice {
        let character = run_style.map(|id| self.of(id)).unwrap_or_default();
        let paragraph = match paragraph_style {
            Some(id) => self.of(id),
            None => self.default_paragraph,
        };
        let table = match table_style {
            Some(Some(id)) => self.of(id),
            Some(None) => self.default_table,
            None => symbol::FontChoice::default(),
        };
        run.or(character).or(paragraph).or(table).or(self.defaults)
    }
}

/// Reads the `w:rFonts` attributes this crate tells fonts apart by.
///
/// Each script class has a named font (`w:ascii`) and a theme font
/// (`w:asciiTheme`). When the theme attribute is present it is the one in
/// use and the named font is ignored (ECMA-376 part 1, 17.3.2.26). A theme
/// font is an ordinary text font, never one of the symbol fonts.
fn font_choice<R: BufRead>(
    reader: &Reader<R>,
    e: &quick_xml::events::BytesStart<'_>,
) -> Result<symbol::FontChoice> {
    let slot = |key: &[u8], theme_key: &[u8]| -> Result<symbol::FontSlot> {
        if attr_value(reader, e, theme_key)?.is_some() {
            return Ok(symbol::FontSlot::Plain);
        }
        Ok(symbol::FontSlot::from_name(
            attr_value(reader, e, key)?.as_deref(),
        ))
    };
    Ok(symbol::FontChoice {
        ascii: slot(b"ascii", b"asciiTheme")?,
        high_ansi: slot(b"hAnsi", b"hAnsiTheme")?,
        complex_script: slot(b"cs", b"cstheme")?,
    })
}

/// Reads the fonts each style gives its runs from `styles.xml`.
fn parse_style_fonts(xml: &[u8]) -> Result<StyleFonts> {
    struct Declared {
        based_on: Option<String>,
        fonts: symbol::FontChoice,
    }

    let mut reader = xml_reader(Cursor::new(utf8_xml(xml)));
    let mut buf = Vec::new();
    let mut declared: HashMap<String, Declared> = HashMap::new();
    let mut default_paragraph: Option<String> = None;
    let mut default_table: Option<String> = None;
    let mut defaults = symbol::FontChoice::default();
    let mut in_doc_defaults = false;
    let mut current: Option<(String, Declared)> = None;
    let mut in_rpr = false;
    // Inside `w:tblStylePr`: formatting for part of a table, not the style's.
    let mut in_table_part = false;
    loop {
        let event = reader.read_event_into(&mut buf);
        match &event {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let is_start = matches!(event, Ok(Event::Start(_)));
                let raw = e.name().as_ref().to_vec();
                match local_name(&raw) {
                    b"style" => {
                        let id = attr_value(&reader, e, b"styleId")?.unwrap_or_default();
                        let is_default = attr_value(&reader, e, b"default")?
                            .is_some_and(|val| val == "1" || val.eq_ignore_ascii_case("true"));
                        let kind = attr_value(&reader, e, b"type")?;
                        let is_paragraph = kind.as_deref() == Some("paragraph");
                        if is_default && is_paragraph && default_paragraph.is_none() {
                            default_paragraph = Some(id.clone());
                        }
                        if is_default && kind.as_deref() == Some("table") && default_table.is_none()
                        {
                            default_table = Some(id.clone());
                        }
                        let style = Declared {
                            based_on: None,
                            fonts: symbol::FontChoice::default(),
                        };
                        if is_start {
                            current = Some((id, style));
                        } else {
                            declared.entry(id).or_insert(style);
                        }
                        in_rpr = false;
                        in_table_part = false;
                    }
                    b"basedOn" => {
                        if let Some((_, style)) = current.as_mut() {
                            style.based_on = attr_value(&reader, e, b"val")?;
                        }
                    }
                    b"rPr" if is_start => in_rpr = true,
                    b"tblStylePr" if is_start => in_table_part = true,
                    b"docDefaults" if is_start => in_doc_defaults = true,
                    b"rFonts" if in_rpr && !in_table_part => {
                        if let Some((_, style)) = current.as_mut() {
                            style.fonts = font_choice(&reader, e)?;
                        } else if in_doc_defaults {
                            defaults = font_choice(&reader, e)?;
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                let raw = e.name().as_ref().to_vec();
                match local_name(&raw) {
                    b"style" => {
                        if let Some((id, style)) = current.take() {
                            declared.entry(id).or_insert(style);
                        }
                    }
                    b"rPr" => in_rpr = false,
                    b"tblStylePr" => in_table_part = false,
                    b"docDefaults" => in_doc_defaults = false,
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(err) => return Err(anyhow!("parse style fonts: {err}")),
            _ => {}
        }
        buf.clear();
    }

    let resolve = |id: &str| {
        let mut fonts = symbol::FontChoice::default();
        let mut next = Some(id);
        for _ in 0..MAX_STYLE_CHAIN {
            let Some(style) = next.and_then(|id| declared.get(id)) else {
                break;
            };
            fonts = fonts.or(style.fonts);
            next = style.based_on.as_deref();
        }
        fonts
    };
    let mut by_id = HashMap::new();
    for id in declared.keys() {
        let fonts = resolve(id);
        if !fonts.is_inherit() {
            by_id.insert(id.clone(), fonts);
        }
    }
    let default_paragraph = default_paragraph
        .as_deref()
        .map(|id| by_id.get(id).copied().unwrap_or_default())
        .unwrap_or_default();
    let default_table = default_table
        .as_deref()
        .map(|id| by_id.get(id).copied().unwrap_or_default())
        .unwrap_or_default();
    Ok(StyleFonts {
        by_id,
        default_paragraph,
        default_table,
        defaults,
    })
}

fn parse_numbering(xml: &[u8]) -> Result<Numbering> {
    let mut reader = xml_reader(Cursor::new(utf8_xml(xml)));
    let mut buf = Vec::new();
    let mut numbering = Numbering::default();
    let mut current_abs: Option<AbstractNum> = None;
    let mut current_level: Option<NumLevel> = None;
    let mut current_num: Option<Num> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let qname = e.name();
                let name = local_name(qname.as_ref());
                if name == b"abstractNum" {
                    let mut id = 0u32;
                    if let Some(val) = attr_value(&reader, &e, b"abstractNumId")? {
                        id = val.parse::<u32>().unwrap_or(0);
                    }
                    current_abs = Some(AbstractNum {
                        id,
                        levels: Vec::new(),
                    });
                } else if name == b"lvl" {
                    let mut level = 0u8;
                    if let Some(val) = attr_value(&reader, &e, b"ilvl")? {
                        level = val.parse::<u8>().unwrap_or(0);
                    }
                    current_level = Some(NumLevel {
                        level,
                        format: None,
                        text: None,
                        start: None,
                    });
                } else if name == b"numFmt" {
                    if let Some(level) = current_level.as_mut() {
                        level.format = attr_value(&reader, &e, b"val")?;
                    }
                } else if name == b"lvlText" {
                    if let Some(level) = current_level.as_mut() {
                        level.text = attr_value(&reader, &e, b"val")?;
                    }
                } else if name == b"start" {
                    if let Some(level) = current_level.as_mut() {
                        if let Some(val) = attr_value(&reader, &e, b"val")? {
                            level.start = val.parse::<u32>().ok();
                        }
                    }
                } else if name == b"num" {
                    let mut id = 0u32;
                    if let Some(val) = attr_value(&reader, &e, b"numId")? {
                        id = val.parse::<u32>().unwrap_or(0);
                    }
                    current_num = Some(Num {
                        id,
                        abstract_id: None,
                    });
                } else if name == b"abstractNumId" {
                    if let Some(num) = current_num.as_mut() {
                        if let Some(val) = attr_value(&reader, &e, b"val")? {
                            num.abstract_id = val.parse::<u32>().ok();
                        }
                    }
                }
            }
            Ok(Event::End(e)) => {
                let qname = e.name();
                let name = local_name(qname.as_ref());
                if name == b"lvl" {
                    if let Some(level) = current_level.take() {
                        if let Some(abs) = current_abs.as_mut() {
                            abs.levels.push(level);
                        }
                    }
                } else if name == b"abstractNum" {
                    if let Some(abs) = current_abs.take() {
                        numbering.abstract_nums.push(abs);
                    }
                } else if name == b"num" {
                    if let Some(num) = current_num.take() {
                        numbering.nums.push(num);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(err) => return Err(anyhow!("parse numbering: {err}")),
            _ => {}
        }
        buf.clear();
    }

    Ok(numbering)
}

fn parse_notes(
    xml: &[u8],
    tag: &[u8],
    styles: &[Style],
    style_fonts: &StyleFonts,
) -> Result<Vec<Note>> {
    let mut reader = xml_reader(Cursor::new(utf8_xml(xml)));
    let mut buf = Vec::new();
    let mut notes = Vec::new();
    let mut current_id: Option<i32> = None;
    let mut fields = Vec::new();
    let mut revisions = Vec::new();
    let mut parser = DocxBodyParser::new(styles, style_fonts, &mut fields, &mut revisions);

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let qname = e.name();
                let name = local_name(qname.as_ref());
                if name == tag {
                    let note_type = attr_value(&reader, &e, b"type")?;
                    let is_user_note = note_type.as_deref().is_none_or(|value| value == "normal");
                    current_id = if is_user_note {
                        attr_value(&reader, &e, b"id")?.and_then(|value| value.parse::<i32>().ok())
                    } else {
                        None
                    };
                    if is_user_note {
                        parser.reset();
                    }
                } else if current_id.is_some() {
                    parser.handle_start(&reader, &e, name)?;
                }
            }
            Ok(Event::Empty(e)) => {
                let qname = e.name();
                let name = local_name(qname.as_ref());
                if current_id.is_some() {
                    parser.handle_empty(&reader, &e, name)?;
                }
            }
            Ok(Event::Text(e)) => {
                if current_id.is_some() {
                    parser.handle_text(&e);
                }
            }
            Ok(Event::GeneralRef(e)) => {
                if current_id.is_some() {
                    parser.handle_text(reference_string(&e).as_bytes());
                }
            }
            Ok(Event::CData(e)) => {
                if current_id.is_some() {
                    parser.handle_text(&e);
                }
            }
            Ok(Event::End(e)) => {
                let qname = e.name();
                let name = local_name(qname.as_ref());
                if name == tag {
                    if let Some(id) = current_id {
                        let blocks = parser.finish();
                        notes.push(Note { id, blocks });
                    }
                    current_id = None;
                } else if current_id.is_some() {
                    parser.handle_end(name)?;
                }
            }
            Ok(Event::Eof) => break,
            Err(err) => return Err(anyhow!("parse notes: {err}")),
            _ => {}
        }
        buf.clear();
    }

    Ok(notes)
}

fn parse_comments(xml: &[u8], styles: &[Style], style_fonts: &StyleFonts) -> Result<Vec<Comment>> {
    let mut reader = xml_reader(Cursor::new(utf8_xml(xml)));
    let mut buf = Vec::new();
    let mut comments = Vec::new();
    let mut current: Option<Comment> = None;
    let mut tmp_fields = Vec::new();
    let mut tmp_revisions = Vec::new();
    let mut parser = DocxBodyParser::new(styles, style_fonts, &mut tmp_fields, &mut tmp_revisions);

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let qname = e.name();
                let name = local_name(qname.as_ref());
                if name == b"comment" {
                    let mut comment = Comment {
                        id: 0,
                        author: None,
                        initials: None,
                        date: None,
                        parent_id: None,
                        blocks: Vec::new(),
                    };
                    for attr in e.attributes().with_checks(false).flatten() {
                        let key = local_name(attr.key.as_ref());
                        if key == b"id" {
                            comment.id = attribute_string(&attr, reader.decoder())
                                .parse::<i32>()
                                .unwrap_or(0);
                        } else if key == b"author" {
                            comment.author = Some(attribute_string(&attr, reader.decoder()));
                        } else if key == b"initials" {
                            comment.initials = Some(attribute_string(&attr, reader.decoder()));
                        } else if key == b"date" {
                            comment.date = Some(attribute_string(&attr, reader.decoder()));
                        } else if key == b"parentId" {
                            comment.parent_id = attribute_string(&attr, reader.decoder())
                                .parse::<i32>()
                                .ok();
                        }
                    }
                    parser.reset();
                    current = Some(comment);
                } else if current.is_some() {
                    parser.handle_start(&reader, &e, name)?;
                }
            }
            Ok(Event::Empty(e)) => {
                let qname = e.name();
                let name = local_name(qname.as_ref());
                if current.is_some() {
                    parser.handle_empty(&reader, &e, name)?;
                }
            }
            Ok(Event::Text(e)) => {
                if current.is_some() {
                    parser.handle_text(&e);
                }
            }
            Ok(Event::GeneralRef(e)) => {
                if current.is_some() {
                    parser.handle_text(reference_string(&e).as_bytes());
                }
            }
            Ok(Event::CData(e)) => {
                if current.is_some() {
                    parser.handle_text(&e);
                }
            }
            Ok(Event::End(e)) => {
                let qname = e.name();
                let name = local_name(qname.as_ref());
                if name == b"comment" {
                    if let Some(mut comment) = current.take() {
                        comment.blocks = parser.finish();
                        comments.push(comment);
                    }
                } else if current.is_some() {
                    parser.handle_end(name)?;
                }
            }
            Ok(Event::Eof) => break,
            Err(err) => return Err(anyhow!("parse comments: {err}")),
            _ => {}
        }
        buf.clear();
    }

    Ok(comments)
}

fn apply_extended_comments(comments: &mut [Comment], xml: &[u8]) -> Result<()> {
    let mut reader = xml_reader(Cursor::new(utf8_xml(xml)));
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let raw = e.name().as_ref().to_vec();
                if local_name(&raw) == b"comment" {
                    let mut current_id: Option<i32> = None;
                    let mut parent_id: Option<i32> = None;
                    for attr in e.attributes().with_checks(false).flatten() {
                        let key = local_name(attr.key.as_ref());
                        if key == b"id" {
                            current_id = attribute_string(&attr, reader.decoder())
                                .parse::<i32>()
                                .ok();
                        } else if key == b"parentId" {
                            parent_id = attribute_string(&attr, reader.decoder())
                                .parse::<i32>()
                                .ok();
                        }
                    }
                    if let (Some(id), Some(parent)) = (current_id, parent_id) {
                        for comment in comments.iter_mut() {
                            if comment.id == id {
                                comment.parent_id = Some(parent);
                                break;
                            }
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(err) => return Err(anyhow!("parse commentsExtended: {err}")),
            _ => {}
        }
        buf.clear();
    }
    Ok(())
}

fn parse_document_blocks<R: BufRead>(
    reader: R,
    styles: &[Style],
    style_fonts: &StyleFonts,
    fields_out: &mut Vec<Field>,
    revisions_out: &mut Vec<Revision>,
) -> Result<Vec<Block>> {
    let mut parser = DocxBodyParser::new(styles, style_fonts, fields_out, revisions_out);
    parser.parse(reader)
}

/// Adds `by` to the field number of every run in `blocks`, tables included.
///
/// A field's number is one more than the count of fields already finished
/// when it begins, counted across the parts in the order they are parsed.
/// (So a field nested in another shares the outer one's number, and a field
/// that is never closed numbers its runs without a `Field` to match; both are
/// kept as they are.) Every number in the body is therefore higher by exactly
/// the count of fields finished before the body, and blocks parsed ahead of
/// the parts that come first in that order are moved into place by adding
/// that count.
fn shift_field_ids(blocks: &mut [Block], by: u32) {
    for block in blocks {
        match block {
            Block::Paragraph(paragraph) => {
                for run in &mut paragraph.runs {
                    if let Some(id) = run.field_id.as_mut() {
                        *id = id.wrapping_add(by);
                    }
                }
            }
            Block::Table(table) => {
                for row in &mut table.rows {
                    for cell in &mut row.cells {
                        shift_field_ids(&mut cell.blocks, by);
                    }
                }
            }
        }
    }
}

/// [`parse_document_blocks`] for the document part, also returning the header
/// and footer references met on the way, in document order.
///
/// # Errors
///
/// Says which of the two jobs failed; see [`BodyFailure`].
fn parse_document_blocks_with_refs<R: BufRead>(
    reader: R,
    styles: &[Style],
    style_fonts: &StyleFonts,
    fields_out: &mut Vec<Field>,
    revisions_out: &mut Vec<Revision>,
) -> std::result::Result<(Vec<Block>, Vec<HeaderFooterRef>), BodyFailure> {
    let mut parser = DocxBodyParser::new(styles, style_fonts, fields_out, revisions_out);
    parser.section_refs = Some(Vec::new());
    match parser.parse(reader) {
        Ok(blocks) => Ok((blocks, parser.section_refs.take().unwrap_or_default())),
        Err(error) => Err(match parser.reference_search_error.take() {
            Some(search_error) => BodyFailure::ReferenceSearch(search_error),
            None => BodyFailure::Body(error),
        }),
    }
}

/// Why [`parse_document_blocks_with_refs`] failed.
enum BodyFailure {
    /// The document is ill-formed, or a header or footer reference cannot be
    /// read. [`parse_header_footer_refs`] stops at the same place, and this
    /// is the error it reports there.
    ReferenceSearch(anyhow::Error),
    /// Anything only the body parser objects to. The reference search may
    /// still fail further on in the document.
    Body(anyhow::Error),
}

thread_local! {
    static PASS_COUNTS: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((0, 0)) };
}

/// How many block-parser runs and how many header/footer reference searches
/// this thread has started, for tests that pin the amount of work a parse
/// does rather than its time.
#[doc(hidden)]
pub fn pass_counts() -> (u64, u64) {
    PASS_COUNTS.with(std::cell::Cell::get)
}

fn collect_toc(blocks: &[Block], out: &mut Vec<TocEntry>) {
    for block in blocks {
        match block {
            Block::Paragraph(p) => {
                let level = p.toc_level.or(p.outline_level);
                if let Some(level) = level {
                    let source = if p.toc_level.is_some() {
                        "toc"
                    } else {
                        "heading"
                    };
                    out.push(TocEntry {
                        level,
                        text: p.text.clone(),
                        source: source.to_string(),
                        paragraph_id: p.id.clone(),
                        style_id: p.style_id.clone(),
                    });
                }
            }
            Block::Table(t) => {
                for row in &t.rows {
                    for cell in &row.cells {
                        collect_toc(&cell.blocks, out);
                    }
                }
            }
        }
    }
}

/// Lists the images every relationships part of the package points at.
///
/// `known` is a relationships part the caller has already read and parsed;
/// it is used as it is, not read again.
fn collect_images(
    archive: &DocxArchive,
    max_inflate: usize,
    known: Option<(&str, &Relationships)>,
) -> Result<Vec<ImageRef>> {
    let content_types = parse_content_types(archive, max_inflate)?;
    let mut images = Vec::new();
    for path in archive.part_names() {
        if !path.ends_with(".rels") {
            continue;
        }
        let read_here;
        let rels = match known {
            Some((known_path, rels)) if known_path == path => rels,
            _ => {
                let xml = read_part_bytes(archive, path, max_inflate)
                    .with_context(|| format!("read DOCX relationships part: {path}"))?;
                read_here = parse_relationships(&utf8_xml(&xml))?;
                &read_here
            }
        };
        {
            let source_part = source_part_from_rels(path).unwrap_or_else(|| path.to_string());
            for RelEntry {
                id,
                rel_type,
                target,
            } in &rels.entries
            {
                if !relationship_matches(rel_type, REL_IMAGE) {
                    continue;
                }
                let target_path = resolve_part(&part_dir(&source_part), target);
                let content_type = content_types
                    .get(&target_path)
                    .cloned()
                    .or_else(|| guess_content_type(&target_path));
                images.push(ImageRef {
                    id: id.clone(),
                    source_part: source_part.clone(),
                    target: target_path,
                    content_type,
                });
            }
        }
    }
    images.sort_by(|left, right| {
        left.source_part
            .cmp(&right.source_part)
            .then_with(|| left.id.cmp(&right.id))
            .then_with(|| left.target.cmp(&right.target))
    });
    Ok(images)
}

fn parse_content_types(
    archive: &DocxArchive,
    max_inflate: usize,
) -> Result<HashMap<String, String>> {
    // Absent is normal (a minimal document omits it); unreadable is a defect.
    let Some(xml) = read_optional_part_bytes(archive, "[Content_Types].xml", max_inflate)? else {
        return Ok(HashMap::new());
    };
    let mut reader = xml_reader(Cursor::new(utf8_xml(&xml)));
    let mut buf = Vec::new();
    let mut map = HashMap::new();
    let mut defaults: HashMap<String, String> = HashMap::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let qname = e.name();
                let name = local_name(qname.as_ref());
                if name == b"Default" {
                    let mut ext = None;
                    let mut ct = None;
                    for attr in e.attributes().with_checks(false).flatten() {
                        let key = local_name(attr.key.as_ref());
                        if key == b"Extension" {
                            ext = Some(attribute_string(&attr, reader.decoder()));
                        } else if key == b"ContentType" {
                            ct = Some(attribute_string(&attr, reader.decoder()));
                        }
                    }
                    if let (Some(ext), Some(ct)) = (ext, ct) {
                        defaults.insert(ext, ct);
                    }
                } else if name == b"Override" {
                    let mut part_name = None;
                    let mut ct = None;
                    for attr in e.attributes().with_checks(false).flatten() {
                        let key = local_name(attr.key.as_ref());
                        if key == b"PartName" {
                            part_name = Some(attribute_string(&attr, reader.decoder()));
                        } else if key == b"ContentType" {
                            ct = Some(attribute_string(&attr, reader.decoder()));
                        }
                    }
                    if let (Some(part_name), Some(ct)) = (part_name, ct) {
                        let part_name = part_name.trim_start_matches('/').to_string();
                        map.insert(part_name, ct);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(err) => return Err(anyhow!("parse content types: {err}")),
            _ => {}
        }
        buf.clear();
    }

    for part in archive.part_names() {
        if map.contains_key(part) {
            continue;
        }
        if let Some(ext) = Path::new(part).extension().and_then(|s| s.to_str()) {
            if let Some(ct) = defaults.get(&ext.to_ascii_lowercase()) {
                map.insert(part.to_string(), ct.clone());
            }
        }
    }

    Ok(map)
}

fn guess_content_type(path: &str) -> Option<String> {
    let ext = Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png".to_string()),
        "jpg" | "jpeg" => Some("image/jpeg".to_string()),
        "gif" => Some("image/gif".to_string()),
        "bmp" => Some("image/bmp".to_string()),
        "tif" | "tiff" => Some("image/tiff".to_string()),
        "svg" => Some("image/svg+xml".to_string()),
        _ => None,
    }
}

/// Reads one attribute value, never failing on what it holds.
///
/// A well-formed value is read as an XML processor reports it: references
/// resolved and literal white space normalized. A value that is not
/// well-formed (an unknown entity, a character reference to no character, a
/// bare `&`, a byte that is not UTF-8) is read by
/// [`lenient_attribute_value`], so one bad character in an attribute costs
/// that character, as it does in text, and not the whole document.
fn attribute_string(
    attr: &quick_xml::events::attributes::Attribute<'_>,
    decoder: quick_xml::encoding::Decoder,
) -> String {
    match attr.decoded_and_normalized_value(XmlVersion::Implicit1_0, decoder) {
        Ok(value) => value.into_owned(),
        Err(_) => lenient_attribute_value(&attr.value),
    }
}

/// Reads an attribute value that is not well-formed.
///
/// Every reference that can be resolved is resolved and literal white space
/// is normalized, as for a well-formed value. A reference that cannot be
/// resolved, and a bare `&`, are kept as written; a byte that is not UTF-8
/// becomes U+FFFD.
fn lenient_attribute_value(raw: &[u8]) -> String {
    let raw = String::from_utf8_lossy(raw);
    let mut out = String::with_capacity(raw.len());
    let mut rest: &str = &raw;
    while let Some(ch) = rest.chars().next() {
        let mut consumed = ch.len_utf8();
        match ch {
            '\t' | '\n' => out.push(' '),
            '\r' => {
                out.push(' ');
                if rest[consumed..].starts_with('\n') {
                    consumed += 1;
                }
            }
            '&' => {
                let resolved = rest[1..].find(';').and_then(|end| {
                    let name = &rest[1..=end];
                    resolve_reference_name(name).map(|text| (text, end + 2))
                });
                match resolved {
                    Some((text, length)) => {
                        out.push_str(&text);
                        consumed = length;
                    }
                    None => out.push('&'),
                }
            }
            other => out.push(other),
        }
        rest = &rest[consumed..];
    }
    out
}

/// The text a reference named `name` (what stands between `&` and `;`)
/// resolves to, `None` when it names nothing.
fn resolve_reference_name(name: &str) -> Option<String> {
    if let Some(number) = name.strip_prefix('#') {
        let code = match number.strip_prefix('x') {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => number.parse::<u32>().ok()?,
        };
        return char::from_u32(code)
            .filter(|ch| *ch != '\0')
            .map(String::from);
    }
    quick_xml::escape::resolve_xml_entity(name).map(str::to_owned)
}

fn attr_value<R: BufRead>(
    reader: &Reader<R>,
    e: &quick_xml::events::BytesStart<'_>,
    key: &[u8],
) -> Result<Option<String>> {
    for attr in e.attributes().with_checks(false).flatten() {
        if local_name(attr.key.as_ref()) == key {
            return Ok(Some(attribute_string(&attr, reader.decoder())));
        }
    }
    Ok(None)
}

/// Byte order of a UTF-16 part, `None` when the part is not UTF-16.
///
/// A byte order mark decides. Without one, the part has to open with an XML
/// declaration, so "<?" with a zero byte on one side of each character is
/// the only other form (XML 1.0, appendix F).
fn utf16_big_endian(start: &[u8]) -> Option<bool> {
    match start {
        [0xFE, 0xFF, ..] | [0x00, b'<', 0x00, b'?', ..] => Some(true),
        [0xFF, 0xFE, ..] | [b'<', 0x00, b'?', 0x00, ..] => Some(false),
        _ => None,
    }
}

/// Transcodes a UTF-16 part to UTF-8.
///
/// An unpaired surrogate or a trailing odd byte becomes U+FFFD. The byte
/// order mark and the XML declaration are dropped: the declaration names an
/// encoding the bytes no longer have.
///
/// The result is built once, in a buffer of exactly the size it needs, so
/// the part is held twice at most: as stored and as UTF-8. UTF-8 needs at
/// most three bytes for a two-byte UTF-16 unit, so the result is never more
/// than one and a half times the stored part, whose size the archive has
/// already checked against the inflate limit.
fn utf16_to_utf8(xml: &[u8], big_endian: bool) -> Vec<u8> {
    let chars = || {
        let units = xml.chunks(2).map(move |pair| match *pair {
            [first, second] if big_endian => u16::from_be_bytes([first, second]),
            [first, second] => u16::from_le_bytes([first, second]),
            // Half a code unit at the end of the part: not a character.
            _ => 0xDC00,
        });
        char::decode_utf16(units).map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
    };
    let mut text = String::with_capacity(chars().map(char::len_utf8).sum());
    text.extend(chars().skip_while(|ch| *ch == '\u{FEFF}'));
    if text.starts_with("<?xml") {
        if let Some(end) = text.find("?>") {
            text.drain(..end + 2);
        }
    }
    text.into_bytes()
}

/// Returns a part as UTF-8, transcoding it when it is UTF-16.
fn utf8_xml(xml: &[u8]) -> Cow<'_, [u8]> {
    match utf16_big_endian(xml) {
        Some(big_endian) => Cow::Owned(utf16_to_utf8(xml, big_endian)),
        None => Cow::Borrowed(xml),
    }
}

/// A part being read: as stored, or transcoded from UTF-16 first.
#[derive(Debug)]
enum Utf8Stream<R> {
    Stored(R),
    Transcoded(Cursor<Vec<u8>>),
}

impl<R: Read> Read for Utf8Stream<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Stored(reader) => reader.read(buf),
            Self::Transcoded(reader) => reader.read(buf),
        }
    }
}

impl<R: BufRead> BufRead for Utf8Stream<R> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        match self {
            Self::Stored(reader) => reader.fill_buf(),
            Self::Transcoded(reader) => reader.fill_buf(),
        }
    }

    fn consume(&mut self, amount: usize) {
        match self {
            Self::Stored(reader) => reader.consume(amount),
            Self::Transcoded(reader) => reader.consume(amount),
        }
    }
}

/// Wraps a part reader so that a UTF-16 part is read as UTF-8.
///
/// The encoding is decided from the bytes already buffered, so a UTF-8 part
/// keeps streaming. A UTF-16 part is read whole, within whatever size limit
/// `reader` enforces, and transcoded.
fn utf8_stream<R: BufRead>(mut reader: R) -> Result<Utf8Stream<R>> {
    let Some(big_endian) = utf16_big_endian(reader.fill_buf()?) else {
        return Ok(Utf8Stream::Stored(reader));
    };
    let mut xml = Vec::new();
    reader.read_to_end(&mut xml)?;
    Ok(Utf8Stream::Transcoded(Cursor::new(utf16_to_utf8(
        &xml, big_endian,
    ))))
}

/// Makes the XML reader every part goes through.
///
/// Text is kept exactly as written: no trimming, so the spaces around a
/// reference survive, and a bare `&` is ordinary text instead of an error
/// that would cost the whole part.
fn xml_reader<R: BufRead>(source: R) -> Reader<R> {
    let mut reader = Reader::from_reader(source);
    let config = reader.config_mut();
    config.trim_text(false);
    config.allow_dangling_amp = true;
    reader
}

/// Decodes a text event leniently.
///
/// A stray non-UTF-8 byte becomes U+FFFD, so one bad character costs that
/// character rather than the whole document. References are not part of a
/// text event; see [`reference_string`].
fn text_event_string(event: &BytesText<'_>) -> String {
    String::from_utf8_lossy(event).into_owned()
}

/// Resolves an entity or character reference to the text it stands for.
///
/// The five entities XML predefines and decimal or hexadecimal character
/// references resolve to their character. Anything else, an unknown entity
/// or a character reference that names no character, is kept as written
/// (`&name;`) rather than dropped.
fn reference_string(reference: &BytesRef<'_>) -> String {
    let name = String::from_utf8_lossy(reference);
    match reference.resolve_char_ref() {
        Ok(Some(ch)) => ch.to_string(),
        Ok(None) => match quick_xml::escape::resolve_xml_entity(&name) {
            Some(text) => text.to_string(),
            None => format!("&{name};"),
        },
        Err(_) => format!("&{name};"),
    }
}

/// Returns the character data of a text, CDATA or reference event, `None`
/// for any other.
///
/// Text is decoded like [`text_event_string`] and a reference is resolved
/// like [`reference_string`]; CDATA is literal character data, decoded but
/// never entity-unescaped.
fn character_data(event: &Event<'_>) -> Option<String> {
    match event {
        Event::Text(text) => Some(text_event_string(text)),
        Event::CData(data) => Some(String::from_utf8_lossy(data).into_owned()),
        Event::GeneralRef(reference) => Some(reference_string(reference)),
        _ => None,
    }
}

/// Deepest nesting of tables inside tables the reader accepts.
///
/// Nested tables are the one place a parsed document is a tree, and every
/// consumer of that tree recurses once per level: dropping it, collecting the
/// table of contents, serializing it, shredding it into Arrow tables. In a
/// debug build on a 256 KiB thread stack the JSON serializer is the first to
/// overflow, between 64 and 80 levels; 32 leaves it less than half of that
/// stack. Real documents nest a handful of levels. Raising this needs the
/// measurement redone: `tests/malformed_input_smoke.rs` walks a document at
/// exactly this depth on such a stack.
const MAX_TABLE_DEPTH: usize = 32;

fn local_name(name: &[u8]) -> &[u8] {
    match name.iter().position(|b| *b == b':') {
        Some(idx) => &name[idx + 1..],
        None => name,
    }
}

#[derive(Debug)]
struct DocxBodyParser<'a> {
    styles_by_id: HashMap<String, StyleRef>,
    style_fonts: &'a StyleFonts,
    fields_out: &'a mut Vec<Field>,
    revisions_out: &'a mut Vec<Revision>,
    revision_ids: HashSet<String>,
    /// Header and footer references met so far, when the caller wants them.
    section_refs: Option<Vec<HeaderFooterRef>>,
    /// Set when a parse that collects references fails the way
    /// [`parse_header_footer_refs`] would at the same place: the error that
    /// search reports.
    reference_search_error: Option<anyhow::Error>,
    containers: Vec<Container>,
    current_paragraph: Option<ParagraphBuilder>,
    current_run: Option<RunBuilder>,
    current_field_stack: Vec<FieldBuilder>,
    current_comment_ids: Vec<i32>,
    pending_comment_refs: Vec<i32>,
    current_revision_stack: Vec<RevisionInfo>,
    /// Table containers currently on `containers`, kept under
    /// [`MAX_TABLE_DEPTH`].
    open_tables: usize,
    /// The `w:tblStyle` of each open table, innermost last; `None` for a
    /// table that names no style.
    table_styles: Vec<Option<String>>,
    in_ppr: bool,
    in_rpr: bool,
    in_numpr: bool,
    /// How many `w:rPrChange` / `w:pPrChange` elements are open. They hold
    /// the properties a run or paragraph had before a tracked formatting
    /// change, which say nothing about how it is formatted now.
    old_properties_depth: usize,
    in_text: bool,
    in_instr: bool,
}

#[derive(Debug, Clone)]
struct StyleRef {
    name: Option<Arc<str>>,
    outline_level: Option<u8>,
}

#[derive(Debug)]
enum Container {
    Doc(Vec<Block>),
    Table(TableBuilder),
    Row(TableRowBuilder),
    Cell(TableCellBuilder),
}

#[derive(Debug, Default)]
struct TableBuilder {
    rows: Vec<TableRow>,
}

#[derive(Debug, Default)]
struct TableRowBuilder {
    cells: Vec<TableCell>,
}

#[derive(Debug, Default)]
struct TableCellBuilder {
    blocks: Vec<Block>,
}

#[derive(Debug, Default)]
struct ParagraphBuilder {
    id: Option<String>,
    style_id: Option<String>,
    outline_level: Option<u8>,
    num_id: Option<u32>,
    num_level: Option<u8>,
    runs: Vec<Run>,
}

#[derive(Debug, Default)]
struct RunBuilder {
    text: String,
    style_id: Option<String>,
    bold: bool,
    italic: bool,
    underline: bool,
    field_id: Option<u32>,
    revision: Option<RevisionRef>,
    comment_ids: Vec<i32>,
    note_references: Vec<NoteReference>,
    /// The run's own `w:rFonts`.
    fonts: symbol::FontChoice,
    /// The symbol fonts in effect for the run, worked out from `fonts` and
    /// the run's styles when its first text arrives. Characters those fonts
    /// give a meaning to are font codes, not text.
    symbol_fonts: Option<symbol::SymbolFonts>,
}

#[derive(Debug)]
struct FieldBuilder {
    id: u32,
    instr: String,
    result: String,
    capturing_result: bool,
}

#[derive(Debug, Clone)]
struct RevisionInfo {
    id: String,
    kind: String,
    author: Option<String>,
    date: Option<String>,
}

impl<'a> DocxBodyParser<'a> {
    fn new(
        styles: &[Style],
        style_fonts: &'a StyleFonts,
        fields_out: &'a mut Vec<Field>,
        revisions_out: &'a mut Vec<Revision>,
    ) -> Self {
        let mut styles_by_id = HashMap::new();
        for style in styles {
            styles_by_id.insert(
                style.id.clone(),
                StyleRef {
                    name: style.name.as_ref().map(|s| Arc::from(s.as_str())),
                    outline_level: style.outline_level,
                },
            );
        }
        Self {
            styles_by_id,
            style_fonts,
            fields_out,
            revisions_out,
            revision_ids: HashSet::new(),
            section_refs: None,
            reference_search_error: None,
            containers: vec![Container::Doc(Vec::new())],
            current_paragraph: None,
            current_run: None,
            current_field_stack: Vec::new(),
            current_comment_ids: Vec::new(),
            pending_comment_refs: Vec::new(),
            current_revision_stack: Vec::new(),
            open_tables: 0,
            table_styles: Vec::new(),
            in_ppr: false,
            in_rpr: false,
            in_numpr: false,
            old_properties_depth: 0,
            in_text: false,
            in_instr: false,
        }
    }

    fn reset(&mut self) {
        self.containers = vec![Container::Doc(Vec::new())];
        self.current_paragraph = None;
        self.current_run = None;
        self.current_field_stack.clear();
        self.current_comment_ids.clear();
        self.pending_comment_refs.clear();
        self.current_revision_stack.clear();
        self.open_tables = 0;
        self.table_styles.clear();
        self.in_ppr = false;
        self.in_rpr = false;
        self.in_numpr = false;
        self.old_properties_depth = 0;
        self.in_text = false;
        self.in_instr = false;
    }

    fn parse<R: BufRead>(&mut self, reader: R) -> Result<Vec<Block>> {
        PASS_COUNTS.with(|counts| counts.set((counts.get().0 + 1, counts.get().1)));
        let mut reader = xml_reader(utf8_stream(reader)?);
        let mut buf = Vec::new();
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(e)) => {
                    let qname = e.name();
                    let name = local_name(qname.as_ref());
                    self.note_section_ref(&reader, &e, name)?;
                    self.handle_start(&reader, &e, name)?;
                }
                Ok(Event::Empty(e)) => {
                    let qname = e.name();
                    let name = local_name(qname.as_ref());
                    self.note_section_ref(&reader, &e, name)?;
                    self.handle_empty(&reader, &e, name)?;
                }
                Ok(Event::Text(e)) => {
                    self.handle_text(&e);
                }
                // A reference arrives on its own, between the pieces of text
                // around it, and is text like them.
                Ok(Event::GeneralRef(e)) => {
                    self.handle_text(reference_string(&e).as_bytes());
                }
                // CDATA is literal character data: no entity unescaping.
                Ok(Event::CData(e)) => {
                    self.handle_text(&e);
                }
                Ok(Event::End(e)) => {
                    let qname = e.name();
                    let name = local_name(qname.as_ref());
                    self.handle_end(name)?;
                }
                Ok(Event::Eof) => break,
                Err(err) => {
                    if self.section_refs.is_some() {
                        self.reference_search_error =
                            Some(anyhow!("parse header/footer refs: {err}"));
                    }
                    return Err(anyhow!("parse document: {err}"));
                }
                _ => {}
            }
            buf.clear();
        }
        Ok(self.finish())
    }

    /// Records `e` if it is a header or footer reference and the caller
    /// asked for them. It is seen here, before anything is skipped, so the
    /// list matches what [`parse_header_footer_refs`] finds.
    fn note_section_ref<R: BufRead>(
        &mut self,
        reader: &Reader<R>,
        e: &quick_xml::events::BytesStart<'_>,
        name: &[u8],
    ) -> Result<()> {
        if let Some(refs) = self.section_refs.as_mut() {
            match header_footer_ref(reader, e, name) {
                Ok(Some(reference)) => refs.push(reference),
                Ok(None) => {}
                // The error is kept twice because it cannot be cloned: once
                // as this parse's own failure, once as what the reference
                // search would say.
                Err(error) => {
                    let message = format!("{error:#}");
                    self.reference_search_error = Some(error);
                    return Err(anyhow!(message));
                }
            }
        }
        Ok(())
    }

    fn handle_start<R: BufRead>(
        &mut self,
        reader: &Reader<R>,
        e: &quick_xml::events::BytesStart<'_>,
        name: &[u8],
    ) -> Result<()> {
        if matches!(name, b"rPrChange" | b"pPrChange" | b"tblPrChange") {
            self.old_properties_depth += 1;
            return Ok(());
        }
        // Old properties are skipped, except for the mark that a paragraph
        // mark was itself inserted or deleted: that is a tracked change and
        // stays in the list of revisions.
        if self.old_properties_depth > 0
            && !matches!(name, b"ins" | b"del" | b"moveFrom" | b"moveTo")
        {
            return Ok(());
        }
        match name {
            b"p" => {
                let mut paragraph = ParagraphBuilder::default();
                for attr in e.attributes().with_checks(false).flatten() {
                    if local_name(attr.key.as_ref()) == b"paraId" {
                        paragraph.id = Some(attribute_string(&attr, reader.decoder()));
                    }
                }
                self.current_paragraph = Some(paragraph);
            }
            b"r" => {
                self.current_run = Some(RunBuilder::default());
                if let Some(rev) = self.current_revision_stack.last() {
                    if let Some(run) = self.current_run.as_mut() {
                        run.revision = Some(RevisionRef {
                            id: rev.id.clone(),
                            kind: rev.kind.clone(),
                        });
                    }
                }
                if !self.current_comment_ids.is_empty() {
                    if let Some(run) = self.current_run.as_mut() {
                        run.comment_ids = self.current_comment_ids.clone();
                    }
                }
                if !self.pending_comment_refs.is_empty() {
                    if let Some(run) = self.current_run.as_mut() {
                        run.comment_ids.append(&mut self.pending_comment_refs);
                    }
                }
            }
            b"t" | b"delText" => {
                self.in_text = true;
            }
            b"instrText" => {
                self.in_instr = true;
            }
            b"tab" | b"ptab" => {
                self.push_text("\t");
            }
            b"br" | b"cr" => {
                self.push_text("\n");
            }
            // The output is plain text for matching, so a non-breaking hyphen
            // is an ordinary hyphen: "non-breaking", not "nonbreaking".
            b"noBreakHyphen" => {
                self.push_text("-");
            }
            // A soft hyphen is only a line-break opportunity; the word is
            // written without it.
            b"softHyphen" => {}
            b"sym" => {
                let font = attr_value(reader, e, b"font")?;
                if let Some(ch) = attr_value(reader, e, b"char")?
                    .and_then(|code| symbol::sym_char(font.as_deref(), &code))
                {
                    self.push_text(ch.encode_utf8(&mut [0; 4]));
                }
            }
            b"rFonts" if self.in_rpr => {
                let fonts = font_choice(reader, e)?;
                if let Some(r) = self.current_run.as_mut() {
                    r.fonts = fonts;
                    r.symbol_fonts = None;
                }
            }
            b"pPr" => self.in_ppr = true,
            b"rPr" => self.in_rpr = true,
            b"numPr" => self.in_numpr = true,
            b"pStyle" if self.in_ppr => {
                if let Some(val) = attr_value(reader, e, b"val")? {
                    if let Some(p) = self.current_paragraph.as_mut() {
                        p.style_id = Some(val);
                    }
                }
            }
            b"outlineLvl" if self.in_ppr => {
                if let Some(val) = attr_value(reader, e, b"val")? {
                    if let Some(p) = self.current_paragraph.as_mut() {
                        p.outline_level = val.parse::<u8>().ok();
                    }
                }
            }
            b"numId" if self.in_numpr => {
                if let Some(val) = attr_value(reader, e, b"val")? {
                    if let Some(p) = self.current_paragraph.as_mut() {
                        p.num_id = val.parse::<u32>().ok();
                    }
                }
            }
            b"ilvl" if self.in_numpr => {
                if let Some(val) = attr_value(reader, e, b"val")? {
                    if let Some(p) = self.current_paragraph.as_mut() {
                        p.num_level = val.parse::<u8>().ok();
                    }
                }
            }
            b"rStyle" if self.in_rpr => {
                if let Some(val) = attr_value(reader, e, b"val")? {
                    if let Some(r) = self.current_run.as_mut() {
                        r.style_id = Some(val);
                        r.symbol_fonts = None;
                    }
                }
            }
            b"b" if self.in_rpr => {
                if let Some(r) = self.current_run.as_mut() {
                    r.bold = parse_bool_attr(reader, e)?;
                }
            }
            b"i" if self.in_rpr => {
                if let Some(r) = self.current_run.as_mut() {
                    r.italic = parse_bool_attr(reader, e)?;
                }
            }
            b"u" if self.in_rpr => {
                if let Some(r) = self.current_run.as_mut() {
                    r.underline = parse_bool_attr(reader, e)?;
                }
            }
            b"commentRangeStart" => {
                if let Some(val) = attr_value(reader, e, b"id")? {
                    if let Ok(id) = val.parse::<i32>() {
                        self.current_comment_ids.push(id);
                    }
                }
            }
            b"commentRangeEnd" => {
                if let Some(val) = attr_value(reader, e, b"id")? {
                    if let Ok(id) = val.parse::<i32>() {
                        self.current_comment_ids.retain(|v| *v != id);
                    }
                }
            }
            b"commentReference" => {
                if let Some(val) = attr_value(reader, e, b"id")? {
                    if let Ok(id) = val.parse::<i32>() {
                        if let Some(run) = self.current_run.as_mut() {
                            run.comment_ids.push(id);
                        } else {
                            self.pending_comment_refs.push(id);
                        }
                    }
                }
            }
            b"footnoteReference" | b"endnoteReference" => {
                if let Some(id) =
                    attr_value(reader, e, b"id")?.and_then(|value| value.parse::<i32>().ok())
                {
                    if let Some(run) = self.current_run.as_mut() {
                        run.note_references.push(NoteReference {
                            kind: if name == b"footnoteReference" {
                                "footnote"
                            } else {
                                "endnote"
                            }
                            .to_string(),
                            id,
                        });
                    }
                }
            }
            b"ins" | b"del" | b"moveFrom" | b"moveTo" => {
                let mut info = RevisionInfo {
                    id: String::new(),
                    kind: String::from_utf8_lossy(name).to_string(),
                    author: None,
                    date: None,
                };
                for attr in e.attributes().with_checks(false).flatten() {
                    let key = local_name(attr.key.as_ref());
                    if key == b"id" {
                        info.id = attribute_string(&attr, reader.decoder());
                    } else if key == b"author" {
                        info.author = Some(attribute_string(&attr, reader.decoder()));
                    } else if key == b"date" {
                        info.date = Some(attribute_string(&attr, reader.decoder()));
                    }
                }
                if !info.id.is_empty() && self.revision_ids.insert(info.id.clone()) {
                    self.revisions_out.push(Revision {
                        id: info.id.clone(),
                        kind: info.kind.clone(),
                        author: info.author.clone(),
                        date: info.date.clone(),
                    });
                }
                self.current_revision_stack.push(info);
            }
            b"fldChar" => {
                if let Some(val) = attr_value(reader, e, b"fldCharType")? {
                    if val == "begin" {
                        let id = (self.fields_out.len() + 1) as u32;
                        self.current_field_stack.push(FieldBuilder {
                            id,
                            instr: String::new(),
                            result: String::new(),
                            capturing_result: false,
                        });
                    } else if val == "separate" {
                        if let Some(field) = self.current_field_stack.last_mut() {
                            field.capturing_result = true;
                        }
                    } else if val == "end" {
                        if let Some(field) = self.current_field_stack.pop() {
                            self.finish_field(field);
                        }
                    }
                }
            }
            b"fldSimple" => {
                let id = (self.fields_out.len() + 1) as u32;
                let instr = attr_value(reader, e, b"instr")?.unwrap_or_default();
                self.current_field_stack.push(FieldBuilder {
                    id,
                    instr,
                    result: String::new(),
                    capturing_result: true,
                });
            }
            b"tbl" => {
                if self.open_tables >= MAX_TABLE_DEPTH {
                    return Err(anyhow!(
                        "tables nested more than {MAX_TABLE_DEPTH} deep are not supported"
                    ));
                }
                self.open_tables += 1;
                self.table_styles.push(None);
                self.containers
                    .push(Container::Table(TableBuilder::default()));
            }
            b"tblStyle" => {
                if let Some(style) = self.table_styles.last_mut() {
                    *style = attr_value(reader, e, b"val")?;
                }
            }
            b"tr" => {
                self.containers
                    .push(Container::Row(TableRowBuilder::default()));
            }
            b"tc" => {
                self.containers
                    .push(Container::Cell(TableCellBuilder::default()));
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_empty<R: BufRead>(
        &mut self,
        reader: &Reader<R>,
        e: &quick_xml::events::BytesStart<'_>,
        name: &[u8],
    ) -> Result<()> {
        // Treat empty tags as start+end for our purposes.
        self.handle_start(reader, e, name)?;
        self.handle_end(name)
    }

    fn handle_text(&mut self, text: &[u8]) {
        let mut value = String::from_utf8_lossy(text);
        if self.in_text {
            let fonts = self.run_symbol_fonts();
            if !fonts.is_empty() {
                value = fonts.map_run_text(&value).into();
            }
        }
        if self.in_instr {
            if let Some(field) = self.current_field_stack.last_mut() {
                field.instr.push_str(&value);
            }
            return;
        }
        if self.in_text {
            if let Some(field) = self.current_field_stack.last_mut() {
                if field.capturing_result {
                    field.result.push_str(&value);
                    if let Some(r) = self.current_run.as_mut() {
                        r.field_id = Some(field.id);
                    }
                }
            }
            self.push_text(&value);
        }
    }

    /// The symbol fonts in effect for the current run.
    ///
    /// Run properties come before the run's text, so the answer is worked
    /// out once, at the first text, and kept for the rest of the run.
    fn run_symbol_fonts(&mut self) -> symbol::SymbolFonts {
        let Some(run) = self.current_run.as_mut() else {
            return symbol::SymbolFonts::default();
        };
        if let Some(fonts) = run.symbol_fonts {
            return fonts;
        }
        let fonts = if self.style_fonts.is_empty() {
            run.fonts.symbol_fonts()
        } else {
            let paragraph_style = self
                .current_paragraph
                .as_ref()
                .and_then(|paragraph| paragraph.style_id.as_deref());
            let table_style = self.table_styles.last().map(Option::as_deref);
            self.style_fonts
                .effective(
                    run.fonts,
                    run.style_id.as_deref(),
                    paragraph_style,
                    table_style,
                )
                .symbol_fonts()
        };
        run.symbol_fonts = Some(fonts);
        fonts
    }

    fn handle_end(&mut self, name: &[u8]) -> Result<()> {
        if matches!(name, b"rPrChange" | b"pPrChange" | b"tblPrChange") {
            self.old_properties_depth = self.old_properties_depth.saturating_sub(1);
            return Ok(());
        }
        // Old properties are skipped, except for the mark that a paragraph
        // mark was itself inserted or deleted: that is a tracked change and
        // stays in the list of revisions.
        if self.old_properties_depth > 0
            && !matches!(name, b"ins" | b"del" | b"moveFrom" | b"moveTo")
        {
            return Ok(());
        }
        match name {
            b"p" => {
                self.flush_run();
                if let Some(p) = self.current_paragraph.take() {
                    let paragraph = self.finish_paragraph(p);
                    self.push_block(Block::Paragraph(paragraph));
                }
            }
            b"r" => {
                self.flush_run();
            }
            b"pPr" => self.in_ppr = false,
            b"rPr" => self.in_rpr = false,
            b"numPr" => self.in_numpr = false,
            b"t" | b"delText" => self.in_text = false,
            b"instrText" => self.in_instr = false,
            b"commentReference" | b"footnoteReference" | b"endnoteReference" => {
                // No-op: handled when the reference is encountered.
            }
            b"ins" | b"del" | b"moveFrom" | b"moveTo" => {
                self.current_revision_stack.pop();
            }
            b"fldSimple" => {
                if let Some(field) = self.current_field_stack.pop() {
                    self.finish_field(field);
                }
            }
            b"tc" => {
                if let Some(Container::Cell(cell)) = self.pop_container() {
                    if let Some(Container::Row(row)) = self.containers.last_mut() {
                        row.cells.push(TableCell {
                            blocks: cell.blocks,
                        });
                    }
                }
            }
            b"tr" => {
                if let Some(Container::Row(row)) = self.pop_container() {
                    if let Some(Container::Table(table)) = self.containers.last_mut() {
                        table.rows.push(TableRow { cells: row.cells });
                    }
                }
            }
            b"tbl" => {
                self.table_styles.pop();
                if let Some(Container::Table(table)) = self.pop_container() {
                    let block = Block::Table(Table { rows: table.rows });
                    self.push_block(block);
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Pops the innermost container, keeping the open-table count in step.
    fn pop_container(&mut self) -> Option<Container> {
        let container = self.containers.pop();
        if matches!(container, Some(Container::Table(_))) {
            self.open_tables = self.open_tables.saturating_sub(1);
        }
        container
    }

    fn push_text(&mut self, value: &str) {
        if let Some(run) = self.current_run.as_mut() {
            run.text.push_str(value);
        }
    }

    fn flush_run(&mut self) {
        if let Some(run) = self.current_run.take() {
            if !run.text.is_empty() || run.field_id.is_some() || !run.note_references.is_empty() {
                if let Some(p) = self.current_paragraph.as_mut() {
                    p.runs.push(Run {
                        text: run.text,
                        style_id: run.style_id,
                        bold: run.bold,
                        italic: run.italic,
                        underline: run.underline,
                        field_id: run.field_id,
                        revision: run.revision,
                        comment_ids: run.comment_ids,
                        note_references: run.note_references,
                    });
                }
            }
        }
    }

    fn finish_paragraph(&self, p: ParagraphBuilder) -> Paragraph {
        let mut text = String::new();
        for run in &p.runs {
            text.push_str(&run.text);
        }
        let style_name = p
            .style_id
            .as_ref()
            .and_then(|id| self.styles_by_id.get(id))
            .and_then(|s| s.name.as_ref().map(|n| n.to_string()));
        let outline_level = p.outline_level.or_else(|| {
            p.style_id
                .as_ref()
                .and_then(|id| self.styles_by_id.get(id))
                .and_then(|s| s.outline_level)
        });
        let numbering = match (p.num_id, p.num_level) {
            (Some(num_id), Some(level)) => Some(NumberingRef { num_id, level }),
            _ => None,
        };
        let toc_level = style_name
            .as_deref()
            .and_then(|name| name.strip_prefix("TOC"))
            .and_then(|rest| rest.parse::<u8>().ok());

        Paragraph {
            id: p.id,
            style_id: p.style_id,
            style_name,
            outline_level,
            numbering,
            toc_level,
            runs: p.runs,
            text,
        }
    }

    fn push_block(&mut self, block: Block) {
        for container in self.containers.iter_mut().rev() {
            match container {
                Container::Doc(blocks) => {
                    blocks.push(block);
                    return;
                }
                Container::Cell(cell) => {
                    cell.blocks.push(block);
                    return;
                }
                _ => {}
            }
        }
    }

    fn finish(&mut self) -> Vec<Block> {
        while self.containers.len() > 1 {
            if let Some(container) = self.pop_container() {
                match container {
                    Container::Cell(cell) => {
                        if let Some(Container::Row(row)) = self.containers.last_mut() {
                            row.cells.push(TableCell {
                                blocks: cell.blocks,
                            });
                        }
                    }
                    Container::Row(row) => {
                        if let Some(Container::Table(table)) = self.containers.last_mut() {
                            table.rows.push(TableRow { cells: row.cells });
                        }
                    }
                    Container::Table(table) => {
                        let block = Block::Table(Table { rows: table.rows });
                        self.push_block(block);
                    }
                    Container::Doc(_) => {}
                }
            }
        }
        if let Some(Container::Doc(blocks)) = self.containers.pop() {
            blocks
        } else {
            Vec::new()
        }
    }

    fn finish_field(&mut self, field: FieldBuilder) {
        let kind = field.instr.split_whitespace().next().map(|s| s.to_string());
        self.fields_out.push(Field {
            id: field.id,
            instr: if field.instr.is_empty() {
                None
            } else {
                Some(field.instr)
            },
            result: field.result,
            kind,
        });
    }
}

fn parse_bool_attr<R: BufRead>(
    reader: &Reader<R>,
    e: &quick_xml::events::BytesStart<'_>,
) -> Result<bool> {
    if let Some(val) = attr_value(reader, e, b"val")? {
        Ok(val == "1" || val.eq_ignore_ascii_case("true"))
    } else {
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ooxml_archive::{parse_root_rels, rels_path_for_part, resolve_part};

    #[test]
    fn parse_root_rels_finds_document_part() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship
    Id="rId1"
    Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument"
    Target="/word/document.xml"/>
</Relationships>"#;

        assert_eq!(parse_root_rels(xml).as_deref(), Some("word/document.xml"));
    }

    #[test]
    fn rels_path_for_part_keeps_full_document_extension() {
        assert_eq!(
            rels_path_for_part("word/document.xml"),
            "word/_rels/document.xml.rels"
        );
    }

    #[test]
    fn resolve_part_handles_parent_segments() {
        assert_eq!(
            resolve_part("word", "../docProps/core.xml"),
            "docProps/core.xml"
        );
    }

    fn body_paragraphs(inner: &str) -> Vec<String> {
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{inner}</w:body></w:document>"#
        );
        paragraphs_of(xml.into_bytes())
    }

    fn paragraphs_of(xml: Vec<u8>) -> Vec<String> {
        let mut fields = Vec::new();
        let mut revisions = Vec::new();
        parse_document_blocks(
            Cursor::new(xml),
            &[],
            &StyleFonts::default(),
            &mut fields,
            &mut revisions,
        )
        .expect("document parses")
        .into_iter()
        .filter_map(|block| match block {
            Block::Paragraph(paragraph) => Some(paragraph.text),
            _ => None,
        })
        .collect()
    }

    #[test]
    fn sym_elements_contribute_their_character() {
        assert_eq!(
            body_paragraphs(
                r#"<w:p><w:r><w:t>x </w:t><w:sym w:font="Symbol" w:char="F0B3"/><w:t> y</w:t></w:r></w:p>"#
            ),
            ["x ≥ y"]
        );
        // A font without a mapping table: the stored code point is kept,
        // never dropped.
        assert_eq!(
            body_paragraphs(
                r#"<w:p><w:r><w:t>ok</w:t><w:sym w:font="Cambria Math" w:char="F0FC"/><w:t>end</w:t></w:r></w:p>"#
            ),
            ["ok\u{F0FC}end"]
        );
    }

    #[test]
    fn symbol_font_run_text_maps_private_use_characters() {
        assert_eq!(
            body_paragraphs(concat!(
                r#"<w:p><w:r><w:rPr><w:rFonts w:ascii="Symbol" w:hAnsi="Symbol"/></w:rPr>"#,
                "<w:t>\u{F061}\u{F062}</w:t></w:r>",
                "<w:r><w:t>\u{F061} plain</w:t></w:r></w:p>",
            )),
            ["αβ\u{F061} plain"]
        );
    }

    #[test]
    fn hyphen_and_tab_elements_become_text() {
        assert_eq!(
            body_paragraphs(concat!(
                "<w:p><w:r><w:t>non</w:t><w:noBreakHyphen/><w:t>breaking co</w:t>",
                "<w:softHyphen/><w:t>op a</w:t><w:ptab w:alignment=\"right\"/><w:t>b</w:t>",
                "<w:tab/><w:t>c</w:t></w:r></w:p>",
            )),
            ["non-breaking coop a\tb\tc"]
        );
    }

    #[test]
    fn cdata_text_is_kept_literally() {
        assert_eq!(
            body_paragraphs("<w:p><w:r><w:t><![CDATA[café <raw> &amp;]]></w:t></w:r></w:p>"),
            ["café <raw> &amp;"]
        );
    }

    #[test]
    fn entities_and_unicode_text_round_trip() {
        assert_eq!(
            body_paragraphs(concat!(
                "<w:p><w:r><w:t xml:space=\"preserve\"> a &amp; b &lt;c&gt; &#233; &#x1F600; ",
                "caf\u{e9}\u{a0}日本語 e\u{301} </w:t></w:r></w:p>",
            )),
            [" a & b <c> é 😀 café\u{a0}日本語 e\u{301} "]
        );
    }

    #[test]
    fn one_invalid_byte_costs_one_character_not_the_document() {
        let mut xml = br#"<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>caf@ ok &amp; fine</w:t></w:r></w:p></w:body></w:document>"#.to_vec();
        let at = xml.iter().position(|byte| *byte == b'@').unwrap();
        xml[at] = 0xE9;
        assert_eq!(paragraphs_of(xml), ["caf\u{fffd} ok & fine"]);
    }

    #[test]
    fn notes_and_comments_share_the_text_handling() {
        let notes = parse_notes(
            br#"<w:footnotes xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:footnote w:id="2"><w:p><w:r><w:t><![CDATA[a<b]]></w:t><w:noBreakHyphen/><w:sym w:font="Symbol" w:char="F061"/></w:r></w:p></w:footnote></w:footnotes>"#,
            b"footnote",
            &[],
            &StyleFonts::default(),
        )
        .unwrap();
        let Block::Paragraph(paragraph) = &notes[0].blocks[0] else {
            panic!("footnote holds a paragraph");
        };
        assert_eq!(paragraph.text, "a<b-α");

        let comments = parse_comments(
            br#"<w:comments xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:comment w:id="1"><w:p><w:r><w:t><![CDATA[x&y]]></w:t><w:sym w:font="Symbol" w:char="F062"/></w:r></w:p></w:comment></w:comments>"#,
            &[],
            &StyleFonts::default(),
        )
        .unwrap();
        let Block::Paragraph(paragraph) = &comments[0].blocks[0] else {
            panic!("comment holds a paragraph");
        };
        assert_eq!(paragraph.text, "x&yβ");
    }
}
