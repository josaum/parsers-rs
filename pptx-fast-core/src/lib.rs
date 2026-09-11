#![deny(missing_docs)]
#![warn(rust_2018_idioms)]

//! # pptx-fast-core
//!
//! High-performance PowerPoint (.pptx) parser with full slide extraction.
//!
//! ## Features
//!
//! - **Complete slide extraction**: shapes, tables, notes, comments
//! - **Master and layout support**: template inheritance
//! - **Shape text extraction**: with formatting (bold, italic, underline)
//! - **Table parsing**: rows and cells with text content
//! - **Speaker notes**: extracted per slide
//! - **Comments**: slide-level annotations
//! - **Image references**: embedded media collection
//! - **Arrow IPC** output format (optional feature)
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use pptx_fast_core::{parse_pptx, ParseOptions};
//!
//! let presentation = parse_pptx("slides.pptx", ParseOptions::default())?;
//! for slide in &presentation.slides {
//!     println!("Slide {}: {} shapes", slide.id, slide.shapes.len());
//! }
//! # Ok::<(), anyhow::Error>(())
//! ```
//!
//! ## Document Structure
//!
//! ```text
//! PptxDocument
//! ├── metadata (core/app/custom properties)
//! ├── slides
//! │   ├── shapes (text boxes, titles, content)
//! │   │   └── paragraphs → runs → text
//! │   ├── tables (rows → cells → text)
//! │   └── notes (speaker notes)
//! ├── masters (slide masters)
//! ├── layouts (slide layouts)
//! ├── comments
//! └── images (embedded media)
//! ```
//!
//! ## Architecture
//!
//! The parser extracts slide content by traversing the relationship
//! chain: presentation.xml → slide relationships → individual slides.
//! Masters and layouts are resolved for complete text extraction.

use anyhow::{anyhow, Context, Result};
use chrono::DateTime;
use ooxml_archive::{
    parse_relationships, part_dir, rels_path_for_part, resolve_part, source_part_from_rels,
    OoxmlArchive, PartReader, RelEntry, Relationships,
};
use quick_xml::events::Event;
use quick_xml::Reader;
use serde::Serialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, Cursor};
use std::path::{Path, PathBuf};

#[cfg(feature = "arrow")]
/// Apache Arrow export helpers for shredding `.pptx` presentations into typed
/// table streams (slides, masters, layouts, shapes, text runs, tables, images).
pub mod arrow;

/// Configuration options for PPTX parsing.
///
/// # Example
///
/// ```rust
/// use pptx_fast_core::ParseOptions;
///
/// let options = ParseOptions {
///     max_inflate: 64 * 1024 * 1024, // 64 MiB limit
///     ..ParseOptions::default()
/// };
/// ```
#[derive(Debug, Clone, Copy)]
pub struct ParseOptions {
    /// Max uncompressed part size to load into memory (bytes).
    ///
    /// Parts larger than this threshold are streamed rather than
    /// loaded entirely into memory. Default: 128 MiB.
    pub max_inflate: usize,
    /// Parse speaker notes. Default: `true`.
    pub extract_notes: bool,
    /// Parse slide comments. Default: `true`.
    pub extract_comments: bool,
    /// Collect image relationship references. Default: `true`.
    pub extract_images: bool,
}

impl Default for ParseOptions {
    fn default() -> Self {
        Self {
            max_inflate: 128 * 1024 * 1024,
            extract_notes: true,
            extract_comments: true,
            extract_images: true,
        }
    }
}

/// Document metadata from core, app, and custom properties.
///
/// Corresponds to `docProps/core.xml`, `docProps/app.xml`, and
/// `docProps/custom.xml` in the OOXML package.
#[derive(Debug, Serialize, Default)]
pub struct Metadata {
    /// Dublin Core properties (title, creator, subject, etc.).
    pub core: HashMap<String, String>,
    /// Application properties (Application, AppVersion, etc.).
    pub app: HashMap<String, String>,
    /// Custom document properties defined by the user.
    pub custom: HashMap<String, String>,
}

/// A complete PowerPoint presentation with all extracted content.
///
/// This is the main output type returned by [`parse_pptx`].
#[derive(Debug, Serialize)]
pub struct PptxDocument {
    /// Presentation metadata (title, author, etc.).
    pub metadata: Metadata,
    /// All slides in presentation order.
    pub slides: Vec<Slide>,
    /// Slide masters (template definitions).
    pub masters: Vec<SlideMaster>,
    /// Slide layouts (layout templates).
    pub layouts: Vec<SlideLayout>,
    /// Presentation-level comments.
    pub comments: Vec<Comment>,
    /// References to embedded images and media.
    pub images: Vec<ImageRef>,
}

/// A single slide in the presentation.
#[derive(Debug, Serialize)]
pub struct Slide {
    /// Slide number (1-based).
    pub id: u32,
    /// Optional slide name/title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Text shapes on the slide (titles, content boxes, etc.).
    pub shapes: Vec<ShapeText>,
    /// Tables on the slide.
    pub tables: Vec<Table>,
    /// Speaker notes for this slide.
    pub notes: Vec<ShapeText>,
}

/// A text-containing shape on a slide.
#[derive(Debug, Serialize)]
pub struct ShapeText {
    /// Shape ID within the slide.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shape_id: Option<u32>,
    /// Shape name (e.g., "Title 1", "Content Placeholder 2").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Placeholder type (title, body, subtitle, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    /// Text content as paragraphs.
    pub paragraphs: Vec<Paragraph>,
}

/// A paragraph within a shape's text frame.
///
/// Contains formatted text runs and an optional indentation level. The `text`
/// field aggregates all runs' text for convenience when fine-grained formatting
/// is not needed.
#[derive(Debug, Serialize)]
pub struct Paragraph {
    /// Paragraph indentation level (0–9 by convention in OOXML).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<u8>,
    /// Formatted text runs that make up this paragraph.
    pub runs: Vec<Run>,
    /// Plain-text concatenation of all runs in this paragraph.
    pub text: String,
}

/// A text run with formatting attributes.
///
/// A run (`<a:r>`) is the smallest unit of formatted text within a paragraph.
/// Formatting flags (bold, italic, underline) apply to the entire run's text.
#[derive(Debug, Serialize)]
pub struct Run {
    /// Text content of the run.
    pub text: String,
    /// True if bold formatting (`b="1"` in run properties) is applied.
    pub bold: bool,
    /// True if italic formatting (`i="1"` in run properties) is applied.
    pub italic: bool,
    /// True if underline formatting (`u="1"` in run properties) is applied.
    pub underline: bool,
}

/// A table on a slide.
///
/// Tables in OOXML presentations (`<p:tbl>`) are organized as rows of cells.
#[derive(Debug, Serialize)]
pub struct Table {
    /// Rows in this table.
    pub rows: Vec<TableRow>,
}

/// A single row in a table.
#[derive(Debug, Serialize)]
pub struct TableRow {
    /// Cells in this row.
    pub cells: Vec<TableCell>,
}

/// A single cell in a table row.
#[derive(Debug, Serialize)]
pub struct TableCell {
    /// Concatenated text of all paragraphs in this cell.
    pub text: String,
}

/// A slide-level comment or annotation.
///
/// Comments in OOXML presentations carry optional author, timestamp, and text.
/// The `slide_id` is assigned during parsing to link the comment back to its
/// containing slide.
#[derive(Debug, Serialize)]
pub struct Comment {
    /// Comment ID within the comments part.
    pub id: u32,
    /// Author name from the comment authors list, if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// ISO 8601 timestamp of the comment, if present in the XML.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    /// Comment text content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// ID of the slide containing this comment (populated by [`parse_pptx`]).
    pub slide_id: u32,
}

/// A slide master (template) in the presentation.
///
/// Masters (`ppt/slideMasters/slideMaster*.xml`) define reusable layout and
/// formatting for one or more slide layouts. Shapes and tables are parsed from
/// the master part once and stored here.
#[derive(Debug, Serialize)]
pub struct SlideMaster {
    /// Relationship ID of this master within the presentation.
    pub id: String,
    /// Resolved path to the master XML part (e.g., `ppt/slideMasters/slideMaster1.xml`).
    pub target: String,
    /// Text shapes in the master (placeholders, etc.).
    pub shapes: Vec<ShapeText>,
    /// Tables in the master.
    pub tables: Vec<Table>,
}

/// A slide layout (template) in the presentation.
///
/// Layouts (`ppt/slideLayouts/slideLayout*.xml`) define which placeholders and
/// shapes appear on slides that use them, typically inheriting from a master.
/// Shapes and tables are parsed once and stored here.
#[derive(Debug, Serialize)]
pub struct SlideLayout {
    /// Relationship ID of this layout within the presentation.
    pub id: String,
    /// Resolved path to the layout XML part (e.g., `ppt/slideLayouts/slideLayout1.xml`).
    pub target: String,
    /// Text shapes in the layout (e.g., title, content placeholders).
    pub shapes: Vec<ShapeText>,
    /// Tables in the layout.
    pub tables: Vec<Table>,
}

/// A reference to an embedded image or media file in the presentation.
///
/// Images are collected by traversing all relationship parts and matching
/// relationships with OOXML image relationship type. The target path points to
/// the image within the archive; content_type is inferred from the part or file
/// extension if available.
#[derive(Debug, Serialize)]
pub struct ImageRef {
    /// Relationship ID of this image (e.g., `rId1`).
    pub id: String,
    /// Source part that references this image (e.g., `ppt/slides/slide1.xml.rels`).
    pub source_part: String,
    /// Resolved path to the image within the archive (e.g., `ppt/media/image1.png`).
    pub target: String,
    /// MIME type of the image (e.g., `image/png`, `image/jpeg`), if determined.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
}

type PptxArchive = OoxmlArchive;

fn open_archive(path: &Path) -> Result<PptxArchive> {
    OoxmlArchive::open(path)
}

fn open_archive_bytes(bytes: Vec<u8>) -> Result<PptxArchive> {
    OoxmlArchive::from_bytes(bytes)
}

fn read_part_bytes<'a>(
    archive: &'a PptxArchive,
    name: &str,
    max_inflate: usize,
) -> Result<ooxml_archive::PartBytes<'a>> {
    archive.read_part(name, max_inflate)
}

fn open_part_reader<'a>(
    archive: &'a PptxArchive,
    name: &str,
    max_inflate: usize,
) -> Result<PartReader<'a>> {
    archive.open_part_reader(name, max_inflate, false)
}

fn read_optional_part_bytes<'a>(
    archive: &'a PptxArchive,
    name: &str,
    max_inflate: usize,
) -> Result<Option<ooxml_archive::PartBytes<'a>>> {
    if !archive.has_part(name) {
        return Ok(None);
    }
    read_part_bytes(archive, name, max_inflate)
        .map(Some)
        .with_context(|| format!("read optional PPTX part: {name}"))
}

fn open_optional_part_reader<'a>(
    archive: &'a PptxArchive,
    name: &str,
    max_inflate: usize,
) -> Result<Option<PartReader<'a>>> {
    if !archive.has_part(name) {
        return Ok(None);
    }
    open_part_reader(archive, name, max_inflate)
        .map(Some)
        .with_context(|| format!("read optional PPTX part: {name}"))
}

#[allow(dead_code)]
const REL_SLIDE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide";
#[allow(dead_code)]
const REL_SLIDE_MASTER: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster";
#[allow(dead_code)]
const REL_SLIDE_LAYOUT: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout";
const REL_NOTES_SLIDE: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesSlide";
const REL_COMMENTS: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments";
const REL_IMAGE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/image";

/// Parse a .pptx file and return a fully materialized [`PptxDocument`].
///
/// Memory-maps or reads the archive, decodes all metadata, masters, layouts,
/// slides, notes, comments, and image references in a single pass. Suitable for
/// presentations that fit comfortably in memory.
pub fn parse_pptx(path: impl AsRef<Path>, options: ParseOptions) -> Result<PptxDocument> {
    let archive = open_archive(path.as_ref())?;
    parse_pptx_from_archive(archive, options)
}

/// Parse a PowerPoint (.pptx) presentation from raw bytes.
///
/// Accepts the complete OOXML package as a byte slice (e.g. from an HTTP
/// upload or in-memory buffer) and returns a fully structured
/// [`PptxDocument`].  No temporary file is written.
pub fn parse_pptx_bytes(bytes: Vec<u8>, options: ParseOptions) -> Result<PptxDocument> {
    let archive = open_archive_bytes(bytes)?;
    parse_pptx_from_archive(archive, options)
}

/// One NDJSON record, tagged by `kind`.
///
/// The records are intentionally thin wrappers: the payload is the same
/// [`Serialize`] type the JSON entry point returns, so a stream cannot drift
/// from the document shape.
#[derive(Serialize)]
struct NdjsonHeader<'a> {
    #[serde(rename = "kind")]
    kind: &'static str,
    metadata: &'a Metadata,
}

#[derive(Serialize)]
struct NdjsonMaster<'a> {
    #[serde(rename = "kind")]
    kind: &'static str,
    master: &'a SlideMaster,
}

#[derive(Serialize)]
struct NdjsonLayout<'a> {
    #[serde(rename = "kind")]
    kind: &'static str,
    layout: &'a SlideLayout,
}

#[derive(Serialize)]
struct NdjsonComment<'a> {
    #[serde(rename = "kind")]
    kind: &'static str,
    comment: &'a Comment,
}

#[derive(Serialize)]
struct NdjsonImage<'a> {
    #[serde(rename = "kind")]
    kind: &'static str,
    image: &'a ImageRef,
}

#[derive(Serialize)]
struct NdjsonSlide<'a> {
    #[serde(rename = "kind")]
    kind: &'static str,
    slide: &'a Slide,
}

fn writeln_nl<W: std::io::Write>(out: &mut W) -> Result<()> {
    out.write_all(b"\n")?;
    Ok(())
}

/// Stream a presentation to `out_path` as NDJSON.
///
/// One record per document-level element, in a fixed order: `metadata`,
/// then every `master`, `layout`, `comment`, `image`, and finally one `slide`
/// record per slide. A reader that only wants the slides still has to pass the
/// headers, but never has to hold the whole document.
pub fn parse_pptx_to_ndjson(
    path: impl AsRef<Path>,
    out_path: impl AsRef<Path>,
    options: ParseOptions,
) -> Result<()> {
    let doc = parse_pptx(path, options)?;
    let mut out = std::io::BufWriter::new(
        File::create(out_path.as_ref())
            .with_context(|| format!("create output: {}", out_path.as_ref().display()))?,
    );

    serde_json::to_writer(
        &mut out,
        &NdjsonHeader {
            kind: "metadata",
            metadata: &doc.metadata,
        },
    )?;
    writeln_nl(&mut out)?;

    for master in &doc.masters {
        serde_json::to_writer(
            &mut out,
            &NdjsonMaster {
                kind: "master",
                master,
            },
        )?;
        writeln_nl(&mut out)?;
    }
    for layout in &doc.layouts {
        serde_json::to_writer(
            &mut out,
            &NdjsonLayout {
                kind: "layout",
                layout,
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
    for slide in &doc.slides {
        serde_json::to_writer(
            &mut out,
            &NdjsonSlide {
                kind: "slide",
                slide,
            },
        )?;
        writeln_nl(&mut out)?;
    }

    Ok(())
}

/// Write every embedded media part of a presentation into `out_dir`, returning
/// the paths written, in presentation order.
///
/// `max_inflate` is the caller's budget, not a constant of this function: a
/// caller that sets a decompression budget on the parse must be able to set the
/// same budget on the extraction, or the extraction becomes the way around it.
///
/// A part named by the package but unreadable is an error. Skipping it would
/// return a list that looks like the complete set of images while silently
/// omitting one, which is the failure mode this parser refuses elsewhere.
pub fn extract_pptx_images(
    path: impl AsRef<Path>,
    out_dir: impl AsRef<Path>,
    options: ParseOptions,
) -> Result<Vec<PathBuf>> {
    let max_inflate = options.max_inflate;
    let archive = open_archive(path.as_ref())?;
    let images = collect_images(&archive, max_inflate)?;
    let out_dir = out_dir.as_ref();
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("create output dir: {}", out_dir.display()))?;

    let mut out_paths = Vec::new();
    for image in images {
        let target = image.target.trim_start_matches('/');
        let bytes = read_part_bytes(&archive, target, max_inflate)
            .with_context(|| format!("read embedded image: {target}"))?;
        let name = Path::new(target)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("image");
        let out_path = out_dir.join(name);
        std::fs::write(&out_path, bytes)
            .with_context(|| format!("write extracted image: {}", out_path.display()))?;
        out_paths.push(out_path);
    }

    Ok(out_paths)
}

/// Internal: parse a PPTX from an already-open archive.
fn parse_pptx_from_archive(archive: PptxArchive, options: ParseOptions) -> Result<PptxDocument> {
    let presentation_path =
        archive.office_document_path(options.max_inflate, "ppt/presentation.xml")?;

    let presentation_rels_path = rels_path_for_part(&presentation_path);
    let presentation_rels_xml =
        read_part_bytes(&archive, &presentation_rels_path, options.max_inflate)
            .with_context(|| format!("read presentation rels: {presentation_rels_path}"))?;
    let presentation_rels = parse_relationships(&presentation_rels_xml)?;

    let (slide_ids, mut masters, mut layouts) =
        parse_presentation(&archive, &presentation_path, options.max_inflate)?;

    for master in masters.iter_mut() {
        if let Some(target) = presentation_rels.by_id.get(&master.id) {
            master.target = resolve_part(&part_dir(&presentation_path), target);
            if let Some(reader) =
                open_optional_part_reader(&archive, &master.target, options.max_inflate)?
            {
                let content = parse_slide(reader)?;
                master.shapes = content.shapes;
                master.tables = content.tables;
            }
        }
    }
    for layout in layouts.iter_mut() {
        if let Some(target) = presentation_rels.by_id.get(&layout.id) {
            layout.target = resolve_part(&part_dir(&presentation_path), target);
            if let Some(reader) =
                open_optional_part_reader(&archive, &layout.target, options.max_inflate)?
            {
                let content = parse_slide(reader)?;
                layout.shapes = content.shapes;
                layout.tables = content.tables;
            }
        }
    }

    let mut slides = Vec::new();
    let mut comments = Vec::new();
    for (id, rid) in slide_ids {
        if let Some(target) = presentation_rels.by_id.get(&rid) {
            let slide_path = resolve_part(&part_dir(&presentation_path), target);
            let slide_rels_path = rels_path_for_part(&slide_path);
            let slide_rels = if let Some(xml) =
                read_optional_part_bytes(&archive, &slide_rels_path, options.max_inflate)?
            {
                parse_relationships(&xml)?
            } else {
                Relationships::default()
            };
            let slide_reader = open_part_reader(&archive, &slide_path, options.max_inflate)
                .with_context(|| format!("read slide: {slide_path}"))?;
            let mut slide = parse_slide(slide_reader)?;
            slide.id = id;

            if options.extract_notes {
                if let Some(notes_target) = slide_rels.by_type.get(REL_NOTES_SLIDE) {
                    let notes_path = resolve_part(&part_dir(&slide_path), notes_target);
                    if let Some(reader) =
                        open_optional_part_reader(&archive, &notes_path, options.max_inflate)?
                    {
                        slide.notes = parse_notes(reader)?;
                    }
                }
            }

            if options.extract_comments {
                if let Some(comments_target) = slide_rels.by_type.get(REL_COMMENTS) {
                    let comments_path = resolve_part(&part_dir(&slide_path), comments_target);
                    if let Some(xml) =
                        read_optional_part_bytes(&archive, &comments_path, options.max_inflate)?
                    {
                        let authors = parse_comment_authors(&archive, options.max_inflate)?;
                        let mut slide_comments = parse_comments(&xml, &authors)?;
                        for comment in slide_comments.iter_mut() {
                            comment.slide_id = id;
                        }
                        comments.extend(slide_comments);
                    }
                }
            }

            slides.push(slide);
        }
    }

    let metadata = parse_metadata(&archive, options.max_inflate)?;
    let images = if options.extract_images {
        collect_images(&archive, options.max_inflate)?
    } else {
        Vec::new()
    };

    Ok(PptxDocument {
        metadata,
        slides,
        masters,
        layouts,
        comments,
        images,
    })
}

/// Parsed `presentation.xml` payload: ordered `(slide_id, rel_id)` pairs,
/// slide masters, and slide layouts.
type PresentationParts = (Vec<(u32, String)>, Vec<SlideMaster>, Vec<SlideLayout>);

fn parse_presentation(
    archive: &PptxArchive,
    path: &str,
    max_inflate: usize,
) -> Result<PresentationParts> {
    let xml = read_part_bytes(archive, path, max_inflate)?;
    let mut reader = Reader::from_reader(Cursor::new(xml));
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut slide_ids = Vec::new();
    let mut masters = Vec::new();
    let mut layouts = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let raw = e.name().as_ref().to_vec();
                let name = local_name(&raw);
                if name == b"sldId" {
                    let mut id = 0u32;
                    let mut rid = None;
                    for attr in e.attributes().flatten() {
                        let raw = attr.key.as_ref();
                        if raw == b"id" {
                            id = attr
                                .decode_and_unescape_value(reader.decoder())?
                                .parse::<u32>()
                                .unwrap_or(0);
                        } else if raw == b"r:id" {
                            rid = Some(
                                attr.decode_and_unescape_value(reader.decoder())?
                                    .to_string(),
                            );
                        }
                    }
                    if let Some(rid) = rid {
                        slide_ids.push((id, rid));
                    }
                } else if name == b"sldMasterId" {
                    let mut rid = None;
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"r:id" {
                            rid = Some(
                                attr.decode_and_unescape_value(reader.decoder())?
                                    .to_string(),
                            );
                        }
                    }
                    if let Some(rid) = rid {
                        masters.push(SlideMaster {
                            id: rid,
                            target: String::new(),
                            shapes: Vec::new(),
                            tables: Vec::new(),
                        });
                    }
                } else if name == b"sldLayoutId" {
                    let mut rid = None;
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"r:id" {
                            rid = Some(
                                attr.decode_and_unescape_value(reader.decoder())?
                                    .to_string(),
                            );
                        }
                    }
                    if let Some(rid) = rid {
                        layouts.push(SlideLayout {
                            id: rid,
                            target: String::new(),
                            shapes: Vec::new(),
                            tables: Vec::new(),
                        });
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(err) => return Err(anyhow!("parse presentation: {err}")),
            _ => {}
        }
        buf.clear();
    }

    Ok((slide_ids, masters, layouts))
}

fn parse_slide<R: BufRead>(reader: R) -> Result<Slide> {
    let mut reader = Reader::from_reader(reader);
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut slide = Slide {
        id: 0,
        name: None,
        shapes: Vec::new(),
        tables: Vec::new(),
        notes: Vec::new(),
    };
    let mut current_shape: Option<ShapeText> = None;
    let mut current_paragraph: Option<ParagraphBuilder> = None;
    let mut current_run: Option<Run> = None;
    let mut current_table: Option<TableBuilder> = None;
    let mut in_text = false;
    let mut in_table = false;
    let mut in_cell = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let raw = e.name().as_ref().to_vec();
                let name = local_name(&raw);
                if name == b"cSld" {
                    if let Some(val) = attr_value(&reader, &e, b"name")? {
                        slide.name = Some(val);
                    }
                } else if name == b"sp" {
                    current_shape = Some(ShapeText {
                        shape_id: None,
                        name: None,
                        placeholder: None,
                        paragraphs: Vec::new(),
                    });
                } else if name == b"cNvPr" {
                    if let Some(shape) = current_shape.as_mut() {
                        if let Some(val) = attr_value(&reader, &e, b"id")? {
                            shape.shape_id = val.parse::<u32>().ok();
                        }
                        if let Some(val) = attr_value(&reader, &e, b"name")? {
                            shape.name = Some(val);
                        }
                    }
                } else if name == b"ph" {
                    if let Some(shape) = current_shape.as_mut() {
                        shape.placeholder = attr_value(&reader, &e, b"type")?;
                    }
                } else if name == b"p" {
                    current_paragraph = Some(ParagraphBuilder::default());
                } else if name == b"pPr" {
                    if let Some(p) = current_paragraph.as_mut() {
                        if let Some(val) = attr_value(&reader, &e, b"lvl")? {
                            p.level = val.parse::<u8>().ok();
                        }
                    }
                } else if name == b"r" {
                    current_run = Some(Run {
                        text: String::new(),
                        bold: false,
                        italic: false,
                        underline: false,
                    });
                } else if name == b"rPr" {
                    if let Some(run) = current_run.as_mut() {
                        if let Some(val) = attr_value(&reader, &e, b"b")? {
                            run.bold = val == "1" || val.eq_ignore_ascii_case("true");
                        }
                        if let Some(val) = attr_value(&reader, &e, b"i")? {
                            run.italic = val == "1" || val.eq_ignore_ascii_case("true");
                        }
                        if let Some(val) = attr_value(&reader, &e, b"u")? {
                            run.underline = val == "1" || val.eq_ignore_ascii_case("true");
                        }
                    }
                } else if name == b"t" {
                    in_text = true;
                } else if name == b"tbl" {
                    in_table = true;
                    current_table = Some(TableBuilder::default());
                } else if name == b"tr" && in_table {
                    if let Some(table) = current_table.as_mut() {
                        table.rows.push(TableRowBuilder::default());
                    }
                } else if name == b"tc" && in_table {
                    if let Some(table) = current_table.as_mut() {
                        if let Some(row) = table.rows.last_mut() {
                            row.cells.push(TableCell {
                                text: String::new(),
                            });
                        }
                    }
                    in_cell = true;
                }
            }
            Ok(Event::Empty(e)) => {
                let raw = e.name().as_ref().to_vec();
                let name = local_name(&raw);
                if name == b"br" {
                    if let Some(run) = current_run.as_mut() {
                        run.text.push('\n');
                    }
                }
            }
            Ok(Event::Text(e)) => {
                if in_text {
                    let value = e.unescape()?.to_string();
                    if in_table {
                        if let Some(table) = current_table.as_mut() {
                            if let Some(row) = table.rows.last_mut() {
                                if in_cell {
                                    if let Some(cell) = row.cells.last_mut() {
                                        cell.text.push_str(&value);
                                    }
                                }
                            }
                        }
                    } else if let Some(run) = current_run.as_mut() {
                        run.text.push_str(&value);
                    }
                }
            }
            Ok(Event::End(e)) => {
                let raw = e.name().as_ref().to_vec();
                let name = local_name(&raw);
                if name == b"t" {
                    in_text = false;
                } else if name == b"r" {
                    if let Some(run) = current_run.take() {
                        if let Some(p) = current_paragraph.as_mut() {
                            if !run.text.is_empty() {
                                p.runs.push(run);
                            }
                        }
                    }
                } else if name == b"p" {
                    if let Some(p) = current_paragraph.take() {
                        let paragraph = p.finish();
                        if in_table {
                            if let Some(table) = current_table.as_mut() {
                                if let Some(row) = table.rows.last_mut() {
                                    if let Some(cell) = row.cells.last_mut() {
                                        cell.text.push_str(&paragraph.text);
                                    }
                                }
                            }
                        } else if let Some(shape) = current_shape.as_mut() {
                            shape.paragraphs.push(paragraph);
                        }
                    }
                } else if name == b"sp" {
                    if let Some(shape) = current_shape.take() {
                        if !shape.paragraphs.is_empty() {
                            slide.shapes.push(shape);
                        }
                    }
                } else if name == b"tbl" {
                    in_table = false;
                    if let Some(table) = current_table.take() {
                        slide.tables.push(table.finish());
                    }
                } else if name == b"tc" {
                    in_cell = false;
                }
            }
            Ok(Event::Eof) => break,
            Err(err) => return Err(anyhow!("parse slide: {err}")),
            _ => {}
        }
        buf.clear();
    }

    Ok(slide)
}

fn parse_notes<R: BufRead>(reader: R) -> Result<Vec<ShapeText>> {
    let mut reader = Reader::from_reader(reader);
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut shapes = Vec::new();
    let mut current_shape: Option<ShapeText> = None;
    let mut current_paragraph: Option<ParagraphBuilder> = None;
    let mut current_run: Option<Run> = None;
    let mut in_text = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let raw = e.name().as_ref().to_vec();
                let name = local_name(&raw);
                if name == b"sp" {
                    current_shape = Some(ShapeText {
                        shape_id: None,
                        name: None,
                        placeholder: None,
                        paragraphs: Vec::new(),
                    });
                } else if name == b"cNvPr" {
                    if let Some(shape) = current_shape.as_mut() {
                        if let Some(val) = attr_value(&reader, &e, b"id")? {
                            shape.shape_id = val.parse::<u32>().ok();
                        }
                        if let Some(val) = attr_value(&reader, &e, b"name")? {
                            shape.name = Some(val);
                        }
                    }
                } else if name == b"p" {
                    current_paragraph = Some(ParagraphBuilder::default());
                } else if name == b"pPr" {
                    if let Some(p) = current_paragraph.as_mut() {
                        if let Some(val) = attr_value(&reader, &e, b"lvl")? {
                            p.level = val.parse::<u8>().ok();
                        }
                    }
                } else if name == b"r" {
                    current_run = Some(Run {
                        text: String::new(),
                        bold: false,
                        italic: false,
                        underline: false,
                    });
                } else if name == b"rPr" {
                    if let Some(run) = current_run.as_mut() {
                        if let Some(val) = attr_value(&reader, &e, b"b")? {
                            run.bold = val == "1" || val.eq_ignore_ascii_case("true");
                        }
                        if let Some(val) = attr_value(&reader, &e, b"i")? {
                            run.italic = val == "1" || val.eq_ignore_ascii_case("true");
                        }
                        if let Some(val) = attr_value(&reader, &e, b"u")? {
                            run.underline = val == "1" || val.eq_ignore_ascii_case("true");
                        }
                    }
                } else if name == b"t" {
                    in_text = true;
                }
            }
            Ok(Event::Text(e)) => {
                if in_text {
                    let value = e.unescape()?.to_string();
                    if let Some(run) = current_run.as_mut() {
                        run.text.push_str(&value);
                    }
                }
            }
            Ok(Event::End(e)) => {
                let raw = e.name().as_ref().to_vec();
                let name = local_name(&raw);
                if name == b"t" {
                    in_text = false;
                } else if name == b"r" {
                    if let Some(run) = current_run.take() {
                        if let Some(p) = current_paragraph.as_mut() {
                            if !run.text.is_empty() {
                                p.runs.push(run);
                            }
                        }
                    }
                } else if name == b"p" {
                    if let Some(p) = current_paragraph.take() {
                        let paragraph = p.finish();
                        if let Some(shape) = current_shape.as_mut() {
                            shape.paragraphs.push(paragraph);
                        }
                    }
                } else if name == b"sp" {
                    if let Some(shape) = current_shape.take() {
                        if !shape.paragraphs.is_empty() {
                            shapes.push(shape);
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(err) => return Err(anyhow!("parse notes: {err}")),
            _ => {}
        }
        buf.clear();
    }

    Ok(shapes)
}

fn parse_comment_authors(
    archive: &PptxArchive,
    max_inflate: usize,
) -> Result<HashMap<u32, String>> {
    let Some(xml) = read_optional_part_bytes(archive, "ppt/commentAuthors.xml", max_inflate)?
    else {
        return Ok(HashMap::new());
    };
    let mut reader = Reader::from_reader(Cursor::new(xml));
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut authors = HashMap::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let raw = e.name().as_ref().to_vec();
                if local_name(&raw) == b"cmAuthor" {
                    let mut id = None;
                    let mut name = None;
                    for attr in e.attributes().flatten() {
                        let key = local_name(attr.key.as_ref());
                        if key == b"id" {
                            id = attr
                                .decode_and_unescape_value(reader.decoder())?
                                .parse::<u32>()
                                .ok();
                        } else if key == b"name" {
                            name = Some(
                                attr.decode_and_unescape_value(reader.decoder())?
                                    .to_string(),
                            );
                        }
                    }
                    if let (Some(id), Some(name)) = (id, name) {
                        authors.insert(id, name);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(err) => return Err(anyhow!("parse comment authors: {err}")),
            _ => {}
        }
        buf.clear();
    }
    Ok(authors)
}

fn parse_comments(xml: &[u8], authors: &HashMap<u32, String>) -> Result<Vec<Comment>> {
    let mut reader = Reader::from_reader(Cursor::new(xml));
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut comments = Vec::new();
    let mut current: Option<Comment> = None;
    let mut in_text = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let raw = e.name().as_ref().to_vec();
                let name = local_name(&raw);
                if name == b"cm" {
                    let mut id = 0u32;
                    let mut author_id = None;
                    let mut date = None;
                    for attr in e.attributes().flatten() {
                        let key = local_name(attr.key.as_ref());
                        if key == b"idx" || key == b"id" {
                            id = attr
                                .decode_and_unescape_value(reader.decoder())?
                                .parse::<u32>()
                                .unwrap_or(0);
                        } else if key == b"authorId" {
                            author_id = attr
                                .decode_and_unescape_value(reader.decoder())?
                                .parse::<u32>()
                                .ok();
                        } else if key == b"dt" {
                            date = Some(
                                attr.decode_and_unescape_value(reader.decoder())?
                                    .to_string(),
                            );
                        }
                    }
                    let author = author_id.and_then(|id| authors.get(&id).cloned());
                    current = Some(Comment {
                        id,
                        author,
                        date,
                        text: None,
                        slide_id: 0,
                    });
                } else if name == b"t" {
                    in_text = true;
                }
            }
            Ok(Event::Text(e)) => {
                if in_text {
                    let value = e.unescape()?.to_string();
                    if let Some(comment) = current.as_mut() {
                        let mut text = comment.text.take().unwrap_or_default();
                        text.push_str(&value);
                        comment.text = Some(text);
                    }
                }
            }
            Ok(Event::End(e)) => {
                let raw = e.name().as_ref().to_vec();
                let name = local_name(&raw);
                if name == b"t" {
                    in_text = false;
                } else if name == b"cm" {
                    if let Some(comment) = current.take() {
                        comments.push(comment);
                    }
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

fn parse_metadata(archive: &PptxArchive, max_inflate: usize) -> Result<Metadata> {
    let mut metadata = Metadata::default();
    if let Some(xml) = read_optional_part_bytes(archive, "docProps/core.xml", max_inflate)? {
        metadata.core = parse_simple_props(&xml)?;
    }
    if let Some(xml) = read_optional_part_bytes(archive, "docProps/app.xml", max_inflate)? {
        metadata.app = parse_simple_props(&xml)?;
    }
    if let Some(xml) = read_optional_part_bytes(archive, "docProps/custom.xml", max_inflate)? {
        metadata.custom = parse_custom_props(&xml)?;
    }
    Ok(metadata)
}

fn parse_simple_props(xml: &[u8]) -> Result<HashMap<String, String>> {
    let mut reader = Reader::from_reader(Cursor::new(xml));
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut out = HashMap::new();
    let mut current_key: Option<String> = None;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let raw = e.name().as_ref().to_vec();
                let key = local_name(&raw);
                current_key = Some(String::from_utf8_lossy(key).to_string());
            }
            Ok(Event::Text(e)) => {
                if let Some(key) = current_key.take() {
                    let value = e.unescape()?.to_string();
                    if !value.is_empty() {
                        out.insert(key, normalize_datetime(&value));
                    }
                }
            }
            Ok(Event::End(_)) => {
                current_key = None;
            }
            Ok(Event::Eof) => break,
            Err(err) => return Err(anyhow!("parse props: {err}")),
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

fn parse_custom_props(xml: &[u8]) -> Result<HashMap<String, String>> {
    let mut reader = Reader::from_reader(Cursor::new(xml));
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut out = HashMap::new();
    let mut current_name: Option<String> = None;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let raw = e.name().as_ref().to_vec();
                if local_name(&raw) == b"property" {
                    for attr in e.attributes().flatten() {
                        if local_name(attr.key.as_ref()) == b"name" {
                            current_name = Some(
                                attr.decode_and_unescape_value(reader.decoder())?
                                    .to_string(),
                            );
                        }
                    }
                }
            }
            Ok(Event::Text(e)) => {
                if let Some(name) = current_name.take() {
                    let value = e.unescape()?.to_string();
                    if !value.is_empty() {
                        out.insert(name, normalize_datetime(&value));
                    }
                }
            }
            Ok(Event::End(e)) => {
                let raw = e.name().as_ref().to_vec();
                if local_name(&raw) == b"property" {
                    current_name = None;
                }
            }
            Ok(Event::Eof) => break,
            Err(err) => return Err(anyhow!("parse custom props: {err}")),
            _ => {}
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

fn collect_images(archive: &PptxArchive, max_inflate: usize) -> Result<Vec<ImageRef>> {
    let content_types = parse_content_types(archive, max_inflate)?;
    let mut images = Vec::new();
    for path in archive.part_names() {
        if !path.ends_with(".rels") {
            continue;
        }
        let xml = read_part_bytes(archive, path, max_inflate)
            .with_context(|| format!("read PPTX relationships part: {path}"))?;
        let rels = parse_relationships(&xml)?;
        let source_part = source_part_from_rels(path).unwrap_or_else(|| path.to_string());
        for RelEntry {
            id,
            rel_type,
            target,
        } in rels.entries
        {
            if rel_type != REL_IMAGE {
                continue;
            }
            let target_path = resolve_part(&part_dir(&source_part), &target);
            let content_type = content_types
                .get(&target_path)
                .cloned()
                .or_else(|| guess_content_type(&target_path));
            images.push(ImageRef {
                id,
                source_part: source_part.clone(),
                target: target_path,
                content_type,
            });
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
    archive: &PptxArchive,
    max_inflate: usize,
) -> Result<HashMap<String, String>> {
    let Some(xml) = read_optional_part_bytes(archive, "[Content_Types].xml", max_inflate)? else {
        return Ok(HashMap::new());
    };
    let mut reader = Reader::from_reader(Cursor::new(xml));
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut map = HashMap::new();
    let mut defaults: HashMap<String, String> = HashMap::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let raw = e.name().as_ref().to_vec();
                let name = local_name(&raw);
                if name == b"Default" {
                    let mut ext = None;
                    let mut ct = None;
                    for attr in e.attributes().flatten() {
                        let key = local_name(attr.key.as_ref());
                        if key == b"Extension" {
                            ext = Some(
                                attr.decode_and_unescape_value(reader.decoder())?
                                    .to_string(),
                            );
                        } else if key == b"ContentType" {
                            ct = Some(
                                attr.decode_and_unescape_value(reader.decoder())?
                                    .to_string(),
                            );
                        }
                    }
                    if let (Some(ext), Some(ct)) = (ext, ct) {
                        defaults.insert(ext, ct);
                    }
                } else if name == b"Override" {
                    let mut part_name = None;
                    let mut ct = None;
                    for attr in e.attributes().flatten() {
                        let key = local_name(attr.key.as_ref());
                        if key == b"PartName" {
                            part_name = Some(
                                attr.decode_and_unescape_value(reader.decoder())?
                                    .to_string(),
                            );
                        } else if key == b"ContentType" {
                            ct = Some(
                                attr.decode_and_unescape_value(reader.decoder())?
                                    .to_string(),
                            );
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

fn attr_value<R: BufRead>(
    reader: &Reader<R>,
    e: &quick_xml::events::BytesStart<'_>,
    key: &[u8],
) -> Result<Option<String>> {
    for attr in e.attributes().flatten() {
        if local_name(attr.key.as_ref()) == key {
            return Ok(Some(
                attr.decode_and_unescape_value(reader.decoder())?
                    .to_string(),
            ));
        }
    }
    Ok(None)
}

fn local_name(name: &[u8]) -> &[u8] {
    match name.iter().position(|b| *b == b':') {
        Some(idx) => &name[idx + 1..],
        None => name,
    }
}

#[derive(Debug, Default)]
struct ParagraphBuilder {
    level: Option<u8>,
    runs: Vec<Run>,
}

impl ParagraphBuilder {
    fn finish(self) -> Paragraph {
        let mut text = String::new();
        for run in &self.runs {
            text.push_str(&run.text);
        }
        Paragraph {
            level: self.level,
            runs: self.runs,
            text,
        }
    }
}

#[derive(Debug, Default)]
struct TableBuilder {
    rows: Vec<TableRowBuilder>,
}

#[derive(Debug, Default)]
struct TableRowBuilder {
    cells: Vec<TableCell>,
}

impl TableBuilder {
    fn finish(self) -> Table {
        Table {
            rows: self
                .rows
                .into_iter()
                .map(|row| TableRow { cells: row.cells })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use ooxml_archive::{parse_root_rels, rels_path_for_part, resolve_part};

    #[test]
    fn parse_root_rels_finds_presentation_part() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship
    Id="rId1"
    Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument"
    Target="/ppt/presentation.xml"/>
</Relationships>"#;

        assert_eq!(
            parse_root_rels(xml).as_deref(),
            Some("ppt/presentation.xml")
        );
    }

    #[test]
    fn rels_path_for_part_keeps_full_slide_extension() {
        // WHY: an earlier off-by-one dropped the last char of the part name
        // (`slide1.xml` → `slide1.xm.rels`), so notes/comments never loaded.
        assert_eq!(
            rels_path_for_part("ppt/slides/slide1.xml"),
            "ppt/slides/_rels/slide1.xml.rels"
        );
        assert_eq!(
            rels_path_for_part("ppt/presentation.xml"),
            "ppt/_rels/presentation.xml.rels"
        );
    }

    #[test]
    fn resolve_part_handles_relative_parent_paths() {
        assert_eq!(
            resolve_part("ppt/slides", "../notesSlides/notesSlide1.xml"),
            "ppt/notesSlides/notesSlide1.xml"
        );
    }
}
