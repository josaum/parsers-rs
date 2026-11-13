#![deny(missing_docs)]
#![warn(rust_2018_idioms)]

//! # pdf-fast-core
//!
//! Byte-level PDF scanner that works without xref traversal.
//!
//! The parser treats a PDF as an opaque byte stream:
//! - scan for `N G obj`
//! - recover shallow dictionaries between `<<` and `>>`
//! - find `stream ... endstream`
//! - decode supported filters in order
//! - extract content-stream text and CMap mappings from decoded bytes
//!
//! The scanner accepts PDF headers from 1.0 through 2.0. It does not depend on
//! classic xref tables, so xref streams and hybrid-reference files are read by
//! scanning their indirect objects directly. PDF 1.5 compressed object streams
//! are expanded, and the standard lossless stream filters (Flate, LZW,
//! ASCIIHex, ASCII85, and RunLength) support ordered filter chains. Flate/LZW
//! `/DecodeParms` predictors are applied after decompression. Image codecs such
//! as JPEG, JPEG 2000, JBIG2, and CCITT remain encoded payloads for their
//! dedicated image paths. Password-encrypted documents require decryption by a
//! caller before parsing.
//!
//! The raw file stays zero-copy inside [`PdfDocument`]. Allocations only happen
//! when a stream must be decoded.

use anyhow::{anyhow, Context, Result};
pub mod cff;
pub mod raster_glyphs;
pub mod tessellation;
pub mod truetype;
#[cfg(feature = "arrow")]
use arrow::array::{
    Array, BinaryBuilder, FixedSizeListBuilder, Float32Array, Float32Builder, Float64Array,
    Int32Builder, LargeStringArray, ListBuilder, StringArray, StringBuilder, UInt32Builder,
};
#[cfg(feature = "arrow")]
use arrow::datatypes::{DataType, Field, Schema};
#[cfg(feature = "arrow")]
use arrow::record_batch::RecordBatch;
#[cfg(feature = "arrow")]
use bumpalo::Bump;
use libdeflater::{DecompressionError, Decompressor};
use memchr::memmem;
#[cfg(feature = "mmap")]
use memmap2::Mmap;
#[cfg(feature = "parallel")]
use rayon::prelude::*;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::File;
use std::io::Read;
#[cfg(feature = "rasterize")]
use std::io::Write as IoWrite;
use std::path::Path;
use std::rc::Rc;
#[cfg(feature = "arrow")]
use std::sync::Arc;
use std::sync::Mutex;
#[cfg(feature = "rasterize")]
use tiny_skia::{
    Color, FillRule, LineCap, LineJoin, Mask, Paint, PathBuilder, Pixmap, Stroke,
    Transform as SkTransform,
};

pub mod vector_text;
pub use vector_text::*;

pub mod encoding;
#[cfg(feature = "glyph-catalog")]
pub mod glyph_catalog;
pub mod lzw;

#[cfg(feature = "rasterize")]
pub mod bitmap_components;
#[cfg(feature = "rasterize")]
mod ccitt;
#[cfg(feature = "rasterize")]
mod render_guard;
#[cfg(feature = "rasterize")]
pub use bitmap_components::{
    BitmapComponentOptions, BitmapComponentPage, BitmapGlyphCandidate, BitmapTextLine,
};

pub mod paragraph;

/// Configuration for byte-level PDF parsing.
#[derive(Debug, Clone, Copy)]
pub struct ParseOptions {
    /// Maximum source document size accepted before object scanning.
    pub max_document_bytes: usize,
    /// Maximum pages recovered from the document.
    pub max_pages: usize,
    /// Hard ceiling on the number of objects scanned before aborting.
    pub max_objects: usize,
    /// Maximum size of a decoded stream kept in memory.
    pub max_decoded_stream_len: usize,
    /// Maximum decoded bytes retained or inspected across the full document.
    pub max_total_decoded_bytes: usize,
    /// Keep decoded stream bytes for later consumers.
    pub keep_decoded_streams: bool,
    /// Extract text operators from decoded content streams.
    pub extract_text: bool,
    /// Parse CMap payloads from decoded streams.
    pub extract_cmaps: bool,
}

impl Default for ParseOptions {
    fn default() -> Self {
        Self {
            max_document_bytes: 256 * 1024 * 1024,
            max_pages: 10_000,
            max_objects: 1_000_000,
            max_decoded_stream_len: 128 * 1024 * 1024,
            max_total_decoded_bytes: 256 * 1024 * 1024,
            keep_decoded_streams: true,
            extract_text: true,
            extract_cmaps: true,
        }
    }
}

/// Zero-copy backing storage for a parsed PDF.
#[derive(Debug)]
enum PdfStorage {
    #[cfg(feature = "mmap")]
    Mmap(Mmap),
    Owned(Vec<u8>),
}

impl AsRef<[u8]> for PdfStorage {
    fn as_ref(&self) -> &[u8] {
        match self {
            #[cfg(feature = "mmap")]
            Self::Mmap(mmap) => mmap.as_ref(),
            Self::Owned(bytes) => bytes.as_slice(),
        }
    }
}

/// Byte range in the underlying storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct ByteRange {
    /// Inclusive start offset in bytes.
    pub start: usize,
    /// Exclusive end offset in bytes.
    pub end: usize,
}

impl ByteRange {
    fn len(self) -> usize {
        self.end.saturating_sub(self.start)
    }
}

#[derive(Debug)]
struct DecodeBudgetExceeded {
    message: String,
}

impl fmt::Display for DecodeBudgetExceeded {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for DecodeBudgetExceeded {}

const MAX_FORM_XOBJECT_DEPTH: usize = 8;

/// Deepest `q` nesting the content interpreters keep saved state for.
///
/// Every `q` copies the current state, and in the rasterizer a state can own
/// a clip mask the size of the page surface, so an unbounded run of `q`
/// turns a few kilobytes of compressed content into gigabytes. PDF 32000-1
/// Annex C names 28 as the nesting a reader must support; this allows about
/// ten times that. Saves past the limit are counted rather than stored, so
/// the `Q` operators that close them still pair up correctly.
///
/// A `Q` that closes a counted save has no state of its own to restore.
/// Leaving the state alone would let a color, matrix or clip set inside the
/// pair leak out to everything after it, and popping a stored state would
/// unbalance every pair around it. Instead the state is copied once, at the
/// first save past the limit, and every `Q` past the limit restores that
/// copy. This is exact for the outermost pair past the limit and for any
/// run of saves that change nothing; inside deeper pairs a restore goes back
/// further than it should, but never to a state from outside the region
/// past the limit, and nothing set there survives the `Q` that leaves it.
/// All three content interpreters (text, rasterizer, table primitives)
/// behave this way.
const MAX_GRAPHICS_STATE_DEPTH: usize = 256;

/// Most components one color operator can set.
///
/// A DeviceN color space has at most 32 colorants (PDF 32000-1 §8.6.6.5), and
/// no other space has more. Operands beyond that are not a color; keeping them
/// would let one operator hold an arbitrary amount of memory that every
/// following `q` then copies.
const MAX_COLOR_COMPONENTS: usize = 32;

type PageImagePayload = (Option<Vec<u8>>, Option<String>);

/// A page-tree node still to visit, with the `/Resources` it inherits.
type PendingPageNode = ((u32, u16), Option<Rc<PdfDictionary>>);

#[derive(Debug)]
struct FormXObjectTraversalExceeded {
    message: String,
}

impl fmt::Display for FormXObjectTraversalExceeded {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for FormXObjectTraversalExceeded {}

fn ensure_form_xobject_traversal(stack: &[(u32, u16)], object_id: (u32, u16)) -> Result<()> {
    if stack.contains(&object_id) {
        return Err(anyhow!(FormXObjectTraversalExceeded {
            message: format!(
                "PDF Form XObject traversal cycle at {} {}",
                object_id.0, object_id.1
            ),
        }));
    }
    if stack.len() >= MAX_FORM_XOBJECT_DEPTH {
        return Err(anyhow!(FormXObjectTraversalExceeded {
            message: format!(
                "PDF Form XObject traversal exceeds max depth {}",
                MAX_FORM_XOBJECT_DEPTH
            ),
        }));
    }
    Ok(())
}

#[derive(Debug)]
struct DecodedByteBudget {
    remaining: Mutex<usize>,
    max_total: usize,
    max_stream: usize,
}

impl DecodedByteBudget {
    fn new(options: ParseOptions) -> Self {
        Self {
            remaining: Mutex::new(options.max_total_decoded_bytes),
            max_total: options.max_total_decoded_bytes,
            max_stream: options.max_decoded_stream_len,
        }
    }

    fn decode_filters(
        &self,
        raw: &[u8],
        filters: &[String],
        decode_parms: Option<&PdfValue>,
    ) -> Result<Vec<u8>> {
        let mut remaining = self
            .remaining
            .lock()
            .map_err(|_| anyhow!("PDF decoded-byte budget lock poisoned"))?;
        let available = self.max_stream.min(*remaining);
        if available == 0 {
            return if *remaining == 0 {
                Err(self.total_limit_error())
            } else {
                Err(self.stream_limit_error())
            };
        }

        match decode_filters(raw, filters, decode_parms, available) {
            Ok(decoded) => {
                *remaining = remaining
                    .checked_sub(decoded.len())
                    .ok_or_else(|| self.total_limit_error())?;
                Ok(decoded)
            }
            Err(error) if is_decode_length_limit(&error) && available < self.max_stream => {
                Err(self.total_limit_error())
            }
            Err(error) if is_decode_length_limit(&error) => Err(self.stream_limit_error()),
            Err(error) => Err(error),
        }
    }

    /// Fail when `bytes` would not fit in what is left of the budget.
    ///
    /// Unlike [`Self::charge_resource_bytes`] nothing is deducted: this is for
    /// transient memory that is released again before the call returns.
    #[cfg(feature = "rasterize")]
    fn ensure_available(&self, bytes: usize) -> Result<()> {
        let remaining = self
            .remaining
            .lock()
            .map_err(|_| anyhow!("PDF decoded-byte budget lock poisoned"))?;
        if bytes > *remaining {
            return Err(self.total_limit_error());
        }
        Ok(())
    }

    /// Bytes the budget could still grant. Reading it spends nothing.
    #[cfg(feature = "rasterize")]
    fn available_bytes(&self) -> usize {
        self.remaining.lock().map_or(0, |remaining| *remaining)
    }

    fn charge_resource_bytes(&self, bytes: usize) -> Result<()> {
        let mut remaining = self
            .remaining
            .lock()
            .map_err(|_| anyhow!("PDF decoded-byte budget lock poisoned"))?;
        if bytes > *remaining {
            return Err(self.total_limit_error());
        }
        *remaining -= bytes;
        Ok(())
    }

    #[cfg(feature = "rasterize")]
    fn encode_rgba_png(&self, rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
        use image::ImageEncoder;

        let mut remaining = self
            .remaining
            .lock()
            .map_err(|_| anyhow!("PDF decoded-byte budget lock poisoned"))?;
        let mut output = BoundedVecWriter::new(*remaining);
        let encode_result = image::codecs::png::PngEncoder::new(&mut output).write_image(
            rgba,
            width,
            height,
            image::ExtendedColorType::Rgba8,
        );
        if output.limit_exceeded {
            return Err(self.total_limit_error());
        }
        encode_result.map_err(|error| anyhow!("rasterized PNG encode: {error}"))?;
        let bytes = output.into_inner();
        *remaining = remaining
            .checked_sub(bytes.len())
            .ok_or_else(|| self.total_limit_error())?;
        Ok(bytes)
    }

    fn total_limit_error(&self) -> anyhow::Error {
        anyhow!(DecodeBudgetExceeded {
            message: format!(
                "PDF decoded bytes exceed max_total_decoded_bytes {}",
                self.max_total
            ),
        })
    }

    fn stream_limit_error(&self) -> anyhow::Error {
        anyhow!(DecodeBudgetExceeded {
            message: format!(
                "PDF decoded stream exceeds max_decoded_stream_len {}",
                self.max_stream
            ),
        })
    }
}

#[cfg(feature = "rasterize")]
struct BoundedVecWriter {
    bytes: Vec<u8>,
    max_len: usize,
    limit_exceeded: bool,
}

#[cfg(feature = "rasterize")]
impl BoundedVecWriter {
    fn new(max_len: usize) -> Self {
        Self {
            bytes: Vec::new(),
            max_len,
            limit_exceeded: false,
        }
    }

    fn into_inner(self) -> Vec<u8> {
        self.bytes
    }
}

#[cfg(feature = "rasterize")]
impl IoWrite for BoundedVecWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let Some(next_len) = self.bytes.len().checked_add(buffer.len()) else {
            self.limit_exceeded = true;
            return Err(std::io::Error::other("PDF raster PNG budget exhausted"));
        };
        if next_len > self.max_len {
            self.limit_exceeded = true;
            return Err(std::io::Error::other("PDF raster PNG budget exhausted"));
        }
        self.bytes
            .try_reserve_exact(buffer.len())
            .map_err(std::io::Error::other)?;
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn is_decode_length_limit(error: &anyhow::Error) -> bool {
    let message = error.to_string();
    message.contains("max_decoded_stream_len") || message.contains("output size overflow")
}

fn is_decode_budget_exceeded(error: &anyhow::Error) -> bool {
    error.downcast_ref::<DecodeBudgetExceeded>().is_some()
}

fn document_byte_limit_error(max_document_bytes: usize) -> anyhow::Error {
    anyhow!(
        "PDF document exceeds max_document_bytes {}",
        max_document_bytes
    )
}

fn read_pdf_bytes_bounded(reader: &mut impl Read, max_document_bytes: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let read_limit = max_document_bytes.saturating_add(1) as u64;
    reader
        .take(read_limit)
        .read_to_end(&mut bytes)
        .context("read bounded PDF input")?;
    if bytes.len() > max_document_bytes {
        return Err(document_byte_limit_error(max_document_bytes));
    }
    Ok(bytes)
}

fn ensure_file_within_document_limit(file: &File, max_document_bytes: usize) -> Result<()> {
    let byte_len = file.metadata().context("read PDF file metadata")?.len();
    if byte_len > max_document_bytes as u64 {
        return Err(document_byte_limit_error(max_document_bytes));
    }
    Ok(())
}

/// Parsed PDF document with zero-copy access to raw bytes.
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct PdfDocument {
    #[cfg_attr(feature = "serde", serde(skip))]
    storage: PdfStorage,
    #[cfg_attr(feature = "serde", serde(skip))]
    object_index: HashMap<(u32, u16), usize>,
    /// The catalog the newest trailer's `/Root` names, when the
    /// cross-reference chain could be followed.
    #[cfg_attr(feature = "serde", serde(skip))]
    catalog: Option<(u32, u16)>,
    /// Entries and sections examined while following the cross-reference
    /// chain.
    #[cfg_attr(feature = "serde", serde(skip))]
    xref_work: usize,
    /// In-use entries the chain kept, before it was applied or discarded.
    #[cfg_attr(feature = "serde", serde(skip))]
    xref_entries: usize,
    #[cfg_attr(feature = "serde", serde(skip))]
    decoded_byte_budget: DecodedByteBudget,
    /// PDF header line (e.g. `%PDF-1.7`) when one was recovered.
    pub header: Option<String>,
    /// Recovered PDF indirect objects.
    pub objects: Vec<PdfObject>,
    /// Recovered PDF stream objects.
    pub streams: Vec<PdfStream>,
    /// Text fragments parsed from content streams.
    pub text_fragments: Vec<PdfTextFragment>,
    /// CMap (`/ToUnicode`) tables harvested from font objects.
    pub cmaps: Vec<PdfCMap>,
    /// Non-fatal parser warnings.
    pub warnings: Vec<PdfWarning>,
}

impl PdfDocument {
    /// Parse a PDF from a file path using memory mapping when possible.
    ///
    /// The file is mapped read-only and the mapping lives as long as the
    /// returned document; when it cannot be mapped, it is read into memory
    /// instead.
    ///
    /// The file must not be modified or truncated, by this process or any
    /// other, while the document is alive. Nothing here can enforce that: a
    /// truncated file makes reads of the mapping fault (`SIGBUS` on Unix), and
    /// a modified one changes the bytes under the parsed document. Copy the
    /// file first, or parse it with `PdfDocument::from_bytes`, when another
    /// writer may touch it.
    #[cfg(feature = "mmap")]
    pub fn open(path: impl AsRef<Path>, options: ParseOptions) -> Result<Self> {
        let path = path.as_ref();
        let mut file =
            File::open(path).with_context(|| format!("open file: {}", path.display()))?;
        ensure_file_within_document_limit(&file, options.max_document_bytes)
            .with_context(|| format!("preflight PDF file: {}", path.display()))?;
        // SAFETY: `Mmap::map` is unsafe because the mapping is only sound
        // while no one modifies or truncates the file, which no code here can
        // enforce. The crate states that contract on this function: the
        // caller must not let the file change while the document lives.
        // Within this process the mapping is read-only and is never written
        // through; it is owned by the document and unmapped when it drops.
        let storage = if let Ok(mmap) = unsafe { Mmap::map(&file) } {
            PdfStorage::Mmap(mmap)
        } else {
            PdfStorage::Owned(
                read_pdf_bytes_bounded(&mut file, options.max_document_bytes)
                    .with_context(|| format!("read file: {}", path.display()))?,
            )
        };
        Self::from_storage(storage, options)
    }

    /// Parse a PDF from a file path, reading the entire file into memory.
    #[cfg(not(feature = "mmap"))]
    pub fn open(path: impl AsRef<Path>, options: ParseOptions) -> Result<Self> {
        let path = path.as_ref();
        let mut file =
            File::open(path).with_context(|| format!("open file: {}", path.display()))?;
        ensure_file_within_document_limit(&file, options.max_document_bytes)
            .with_context(|| format!("preflight PDF file: {}", path.display()))?;
        let bytes = read_pdf_bytes_bounded(&mut file, options.max_document_bytes)
            .with_context(|| format!("read file: {}", path.display()))?;
        Self::from_storage(PdfStorage::Owned(bytes), options)
    }

    /// Parse a PDF from owned bytes without copying.
    pub fn from_bytes(bytes: Vec<u8>, options: ParseOptions) -> Result<Self> {
        Self::from_storage(PdfStorage::Owned(bytes), options)
    }

    /// Parse a PDF from borrowed bytes. This copies into owned storage.
    pub fn from_slice(bytes: &[u8], options: ParseOptions) -> Result<Self> {
        if bytes.len() > options.max_document_bytes {
            return Err(document_byte_limit_error(options.max_document_bytes));
        }
        Self::from_storage(PdfStorage::Owned(bytes.to_vec()), options)
    }

    fn from_storage(storage: PdfStorage, options: ParseOptions) -> Result<Self> {
        let bytes = storage.as_ref();
        if bytes.len() > options.max_document_bytes {
            return Err(document_byte_limit_error(options.max_document_bytes));
        }
        let header = find_pdf_header(bytes);
        let mut objects = Vec::new();
        let mut streams = Vec::new();
        let mut text_fragments = Vec::new();
        let mut cmaps = Vec::new();
        let mut warnings = Vec::new();
        let decoded_byte_budget = DecodedByteBudget::new(options);
        let mut cursor = 0usize;
        let headers = find_all_object_headers(bytes, options.max_objects.saturating_add(1));
        let indirect_integers = collect_indirect_integers(bytes, &headers);

        for header_match in headers {
            if objects.len() >= options.max_objects {
                break;
            }
            if header_match.start < cursor {
                continue;
            }

            let object = scan_object(
                bytes,
                &header_match,
                options,
                &mut streams,
                &mut text_fragments,
                &mut cmaps,
                &mut warnings,
                &indirect_integers,
                &decoded_byte_budget,
            )?;
            cursor = object.span.end.max(header_match.header_end);
            objects.push(object);
        }

        let mut objstm_objects = Vec::new();
        for object in &objects {
            if objects.len().saturating_add(objstm_objects.len()) >= options.max_objects {
                break;
            }
            if let Some(stream_index) = object.stream_index {
                let stream = &streams[stream_index];
                if stream.kind == PdfStreamKind::ObjectStream {
                    if let Some(decoded) = &stream.decoded {
                        if let Some(dict) = &object.dictionary {
                            if let (Some(n), Some(first)) = (
                                dict.get_integer("N")
                                    .and_then(|value| usize::try_from(value).ok()),
                                dict.get_integer("First")
                                    .and_then(|value| usize::try_from(value).ok()),
                            ) {
                                let mut cursor = 0;
                                let mut offsets = Vec::new();
                                let remaining_capacity = options
                                    .max_objects
                                    .saturating_sub(objects.len() + objstm_objects.len());
                                for _ in 0..n.min(remaining_capacity) {
                                    skip_whitespace_and_comments(decoded, &mut cursor);
                                    let mut parser = ValueParser::new(decoded, cursor);
                                    if let Ok(obj_num) = parser.parse_number_token() {
                                        cursor = parser.pos;
                                        skip_whitespace_and_comments(decoded, &mut cursor);
                                        let mut p2 = ValueParser::new(decoded, cursor);
                                        if let Ok(offset) = p2.parse_number_token() {
                                            cursor = p2.pos;
                                            let Some(obj_num) = number_to_u32(obj_num) else {
                                                break;
                                            };
                                            let Some(offset) = number_to_usize(offset) else {
                                                break;
                                            };
                                            offsets.push((obj_num, offset));
                                        } else {
                                            break;
                                        }
                                    } else {
                                        break;
                                    }
                                }

                                // Objects in an object stream are stored one
                                // after another. An offset that lands inside
                                // an object already read is refused: N offsets
                                // aimed at one large value would otherwise
                                // materialize it N times, turning a few
                                // kilobytes of input into gigabytes.
                                //
                                // The table is walked in offset order, not
                                // table order: a writer that lists the
                                // objects out of order must not have the
                                // earlier ones mistaken for overlaps.
                                offsets.sort_by_key(|(_, offset)| *offset);
                                let mut parsed_end = 0usize;
                                for (obj_num, offset) in offsets {
                                    let mut obj_cursor = first.saturating_add(offset);
                                    if obj_cursor >= decoded.len() || obj_cursor < parsed_end {
                                        continue;
                                    }
                                    skip_whitespace_and_comments(decoded, &mut obj_cursor);
                                    let mut parser = ValueParser::new(decoded, obj_cursor);
                                    let parsed = parser.parse_value();
                                    parsed_end = parsed_end.max(parser.pos);
                                    if let Ok(value) = parsed {
                                        let (dictionary, array) = match value {
                                            PdfValue::Dictionary(d) => (Some(d), None),
                                            PdfValue::Array(a) => (None, Some(a)),
                                            _ => (None, None),
                                        };
                                        objstm_objects.push(PdfObject {
                                            object_number: obj_num,
                                            generation: 0,
                                            span: ByteRange {
                                                start: object.span.start,
                                                end: object.span.end,
                                            },
                                            dictionary_range: None,
                                            dictionary,
                                            array,
                                            stream_index: None,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        objects.extend(objstm_objects);

        // Scan order first: a later definition of an object replaces an
        // earlier one, which is right for a file whose revisions were
        // appended in order and is all there is to go on when the
        // cross-reference data is missing or broken.
        let mut object_index: HashMap<(u32, u16), usize> = objects
            .iter()
            .enumerate()
            .map(|(index, object)| ((object.object_number, object.generation), index))
            .collect();
        // Then the document as its cross-reference chain defines it.
        let (catalog, xref_work, xref_entries) =
            apply_xref_chain(bytes, &objects, &streams, &mut object_index);

        if objects.len() >= options.max_objects {
            warnings.push(PdfWarning {
                object_number: None,
                generation: None,
                message: format!("aborted after reaching max_objects={}", options.max_objects),
            });
        }

        let total_decoded_bytes = streams.iter().try_fold(0usize, |total, stream| {
            total.checked_add(stream.decoded_len.unwrap_or(0))
        });
        if total_decoded_bytes
            .map(|total| total > options.max_total_decoded_bytes)
            .unwrap_or(true)
        {
            return Err(anyhow!("PDF streams exceed max_total_decoded_bytes"));
        }

        let mut document = Self {
            storage,
            object_index,
            catalog,
            xref_work,
            xref_entries,
            decoded_byte_budget,
            header,
            objects,
            streams,
            text_fragments,
            cmaps,
            warnings,
        };
        if document.pages().len() > options.max_pages {
            return Err(anyhow!("PDF document exceeds max_pages"));
        }
        if options.extract_text {
            document.populate_text_fragments();
        }
        if !options.extract_cmaps {
            document.cmaps.clear();
        }
        Ok(document)
    }

    /// Access the raw PDF bytes.
    pub fn bytes(&self) -> &[u8] {
        self.storage.as_ref()
    }

    /// Borrow the raw bytes for an object span.
    pub fn object_bytes(&self, index: usize) -> Option<&[u8]> {
        let span = self.objects.get(index)?.span;
        self.bytes().get(span.start..span.end)
    }

    /// Borrow the raw bytes for a stream body.
    pub fn stream_raw_bytes(&self, index: usize) -> Option<&[u8]> {
        let span = self.streams.get(index)?.raw_range;
        self.bytes().get(span.start..span.end)
    }

    /// Borrow decoded bytes when the parser retained them.
    pub fn stream_decoded_bytes(&self, index: usize) -> Option<&[u8]> {
        self.streams.get(index)?.decoded.as_deref()
    }

    /// Resolve an indirect object by `(object_number, generation)`.
    pub fn object_by_id(&self, object_number: u32, generation: u16) -> Option<&PdfObject> {
        let index = *self.object_index.get(&(object_number, generation))?;
        self.objects.get(index)
    }

    /// Resolve a dictionary value, following one indirect reference hop.
    pub fn resolve_dictionary<'a>(&'a self, value: &'a PdfValue) -> Option<&'a PdfDictionary> {
        match value {
            PdfValue::Dictionary(dictionary) => Some(dictionary),
            PdfValue::Reference {
                object_number,
                generation,
            } => self
                .object_by_id(*object_number, *generation)?
                .dictionary
                .as_ref(),
            _ => None,
        }
    }

    /// Units of work spent following the cross-reference chain: one per
    /// entry examined and a fixed cost per section. Exposed so tests can
    /// bound the work a hostile file causes without timing it.
    #[doc(hidden)]
    pub fn xref_chain_work(&self) -> usize {
        self.xref_work
    }

    /// In-use cross-reference entries kept while following the chain,
    /// whether or not the chain was then applied. Exposed so tests can
    /// show nothing is kept for objects the scan never produced.
    #[doc(hidden)]
    pub fn xref_chain_entries(&self) -> usize {
        self.xref_entries
    }

    /// Resolve pages and their associated content/image streams.
    pub fn pages(&self) -> Vec<PdfPage> {
        if let Some(mut pages) = self.pages_from_catalog() {
            for (page_index, page) in pages.iter_mut().enumerate() {
                page.page_index = page_index;
            }
            return pages;
        }

        self.objects
            .iter()
            .filter(|object| {
                object.dictionary.as_ref().and_then(|d| d.get_name("Type")) == Some("Page")
            })
            .enumerate()
            .map(|(page_index, object)| self.page_from_object(object, page_index, None))
            .collect()
    }

    /// Resolve page-aware text runs using the page tree, inherited resources, and `/ToUnicode`.
    pub fn semantic_text_runs(&self) -> Vec<PdfTextRun> {
        let pages = self.pages();
        let mut runs = Vec::new();
        for page in &pages {
            runs.extend(self.semantic_text_runs_for_page(page));
        }
        runs
    }

    /// Materialize semantic text runs into an Arrow batch for zero-copy downstream bridges.
    #[cfg(feature = "arrow")]
    pub fn semantic_text_runs_record_batch(&self) -> Result<RecordBatch> {
        let rows = self
            .semantic_text_runs()
            .into_iter()
            .map(ParallelTextRow::from_run)
            .collect::<Vec<_>>();
        build_text_run_record_batch(rows)
    }

    /// Parallel Arrow exhaust with thread-local arenas and owned row batches.
    #[cfg(all(feature = "arrow", feature = "parallel"))]
    pub fn semantic_text_runs_record_batch_parallel(&self) -> Result<RecordBatch> {
        let pages = self.pages();
        let batches = pages
            .par_iter()
            .fold(ThreadLocalTextBatch::new, |mut batch, page| {
                for run in self.semantic_text_runs_for_page(page) {
                    batch.push_run(run);
                }
                batch
            })
            .collect::<Vec<_>>();

        let rows = batches
            .into_iter()
            .flat_map(|batch| batch.rows)
            .collect::<Vec<_>>();
        build_text_run_record_batch(rows)
    }

    /// Serial Arrow exhaust with thread-local arenas and owned row batches.
    #[cfg(all(feature = "arrow", not(feature = "parallel")))]
    pub fn semantic_text_runs_record_batch_parallel(&self) -> Result<RecordBatch> {
        let pages = self.pages();
        let batch = pages
            .iter()
            .fold(ThreadLocalTextBatch::new(), |mut batch, page| {
                for run in self.semantic_text_runs_for_page(page) {
                    batch.push_run(run);
                }
                batch
            });
        let rows = batch.rows;
        build_text_run_record_batch(rows)
    }

    /// Page-level multimodal batch for VL models: text, token ids, 2D boxes, and raw image bytes.
    #[cfg(all(feature = "arrow", feature = "parallel"))]
    pub fn page_multimodal_record_batch_parallel(&self) -> Result<RecordBatch> {
        let pages = self.pages();
        let rows = pages
            .par_iter()
            .map(|page| self.page_multimodal_row(page))
            .collect::<Result<Vec<_>>>()?;
        build_page_multimodal_record_batch(rows)
    }

    /// Serial fallback of multimodal batch when `parallel` feature is disabled.
    #[cfg(all(feature = "arrow", not(feature = "parallel")))]
    pub fn page_multimodal_record_batch_parallel(&self) -> Result<RecordBatch> {
        let pages = self.pages();
        let rows = pages
            .iter()
            .map(|page| self.page_multimodal_row(page))
            .collect::<Result<Vec<_>>>()?;
        build_page_multimodal_record_batch(rows)
    }

    /// Single-page multimodal batch for zero-copy FFI handoff.
    #[cfg(feature = "arrow")]
    pub fn page_multimodal_record_batch(&self, page_index: usize) -> Result<RecordBatch> {
        let pages = self.pages();
        let page = pages
            .iter()
            .find(|page| page.page_index == page_index)
            .ok_or_else(|| anyhow!("page index {} out of bounds", page_index))?;
        build_page_multimodal_record_batch(vec![self.page_multimodal_row(page)?])
    }

    /// Rasterize a page's vector content into a PNG using the page crop/media box.
    #[cfg(feature = "rasterize")]
    pub fn page_rasterized_png(&self, page_index: usize, dpi: f32) -> Result<Option<Vec<u8>>> {
        if !dpi.is_finite() || !(1.0..=600.0).contains(&dpi) {
            return Err(anyhow!("raster DPI must be within 1..=600"));
        }
        let pages = self.pages();
        let page = pages
            .iter()
            .find(|page| page.page_index == page_index)
            .ok_or_else(|| anyhow!("page index {} out of bounds", page_index))?;
        // Refuse a malformed `/Rotate` before rendering. Normalization folds a
        // multiple of 90 and cannot represent anything else, so without this
        // check a file declaring `/Rotate 45` rasterizes upright and the
        // sideways page reaches OCR looking entirely normal.
        if let Some(raw) = page.rotate_raw {
            normalize_rotate(raw)?;
        }
        let fast_path = self.rasterized_page_image_payload(page, dpi)?;
        let (png_bytes, apply_fast_path_rotation) =
            if let Some(png) = fast_path.and_then(|(bytes, _format)| bytes) {
                (png, true)
            } else if let Some(png) = self
                .rasterized_page_image_payload_hayro((page.object_number, page.generation), dpi)?
            {
                // Hayro applies the page's initial transform, including
                // `/Rotate`, while rendering.
                (png, false)
            } else {
                return Ok(None);
            };
        if page.rotate == 0 || !apply_fast_path_rotation {
            return Ok(Some(png_bytes));
        }
        // Apply /Rotate correction before handing the image to OCR.
        let crop_box = page
            .page_box()
            .ok_or_else(|| anyhow!("fast raster page box unavailable during rotation"))?;
        let Some((px_width, px_height, rgba_bytes)) = raster_surface_layout(crop_box, dpi) else {
            return Err(anyhow!(
                "fast raster dimensions unavailable during rotation"
            ));
        };
        self.decoded_byte_budget.charge_resource_bytes(rgba_bytes)?;
        let img = image::load_from_memory(&png_bytes)
            .map_err(|e| anyhow!("rasterized PNG decode: {e}"))?;
        if img.width() != px_width || img.height() != px_height || img.as_rgba8().is_none() {
            return Err(anyhow!(
                "fast raster PNG changed dimensions or color type during bounded rotation decode"
            ));
        }
        self.decoded_byte_budget.charge_resource_bytes(rgba_bytes)?;
        let rotated = match page.rotate {
            90 => img.rotate90(),
            180 => img.rotate180(),
            270 => img.rotate270(),
            _ => {
                return Err(anyhow!(
                    "unsupported normalized PDF page rotation {}",
                    page.rotate
                ));
            }
        };
        let rotated_rgba = rotated.as_rgba8().ok_or_else(|| {
            anyhow!("fast raster rotation changed color type before bounded PNG encode")
        })?;
        if rotated_rgba.as_raw().len() != rgba_bytes {
            return Err(anyhow!(
                "fast raster rotation changed allocation size after bounded preflight"
            ));
        }
        self.decoded_byte_budget
            .encode_rgba_png(rotated_rgba.as_raw(), rotated.width(), rotated.height())
            .map(Some)
    }

    /// Render a page with the full PDF interpreter when the lightweight
    /// vector path cannot paint it.
    ///
    /// The fast rasterizer intentionally handles a narrow operator set. Some
    /// scanned contracts represent each page as several Flate-compressed
    /// `/Indexed` image XObjects. Returning `None` for those pages used to
    /// leave the OCR fallback with undecodable raw palette indices. Hayro is
    /// only invoked after the fast path fails, preserving the common-case
    /// latency while covering image mosaics and other valid PDF constructs.
    /// Render a page with the FULL interpreter (fonts included),
    /// bypassing the vectors-only fast path. The fast path paints paths but
    /// not font text: on a born-digital table page it returns just the grid
    /// — visually "successful", textually blank (measured: two different
    /// price-table pages produced byte-identical rule-only renders that were
    /// first misread as a page-mapping bug). Measurement tooling and OCR
    /// consumers that need the TEXT should use this entry. Requires the
    /// `rasterize` feature; returns `None` without it.
    #[cfg(feature = "rasterize")]
    pub fn page_rasterized_png_interpreted(&self, page_index: usize, dpi: f32) -> Option<Vec<u8>> {
        self.page_rasterized_png_interpreted_checked(page_index, dpi)
            .ok()
            .flatten()
    }

    /// Render a page with the full PDF interpreter and propagate resource-
    /// budget failures to the caller.
    ///
    /// `page_index` counts the pages [`Self::pages`] returns. An index past
    /// the last of them gives `Ok(None)`, with or without the `rasterize`
    /// feature, which is what the language bindings have always passed on;
    /// [`Self::page_rasterized_png`] reports the same index as an error.
    ///
    /// # Errors
    ///
    /// Returns an error when the page is refused as unsafe to interpret or
    /// when a resource budget is spent.
    #[cfg(feature = "rasterize")]
    pub fn page_rasterized_png_interpreted_checked(
        &self,
        page_index: usize,
        dpi: f32,
    ) -> Result<Option<Vec<u8>>> {
        let pages = self.pages();
        let Some(page) = pages.iter().find(|page| page.page_index == page_index) else {
            return Ok(None);
        };
        self.rasterized_page_image_payload_hayro((page.object_number, page.generation), dpi)
    }

    /// Stub without the `rasterize` feature: the full interpreter is not
    /// compiled in.
    #[cfg(not(feature = "rasterize"))]
    pub fn page_rasterized_png_interpreted(
        &self,
        _page_index: usize,
        _dpi: f32,
    ) -> Option<Vec<u8>> {
        None
    }

    /// Stub without `rasterize`: no full PDF interpreter is compiled in.
    #[cfg(not(feature = "rasterize"))]
    pub fn page_rasterized_png_interpreted_checked(
        &self,
        _page_index: usize,
        _dpi: f32,
    ) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }

    /// Renders the page that is indirect object `page_object` with the full
    /// interpreter, after [`render_guard::vet_page`] has accepted it.
    #[cfg(feature = "rasterize")]
    fn rasterized_page_image_payload_hayro(
        &self,
        page_object: (u32, u16),
        dpi: f32,
    ) -> Result<Option<Vec<u8>>> {
        if !dpi.is_finite() || !(1.0..=600.0).contains(&dpi) {
            return Ok(None);
        }

        let replay_bytes = self.streams.iter().try_fold(0usize, |total, stream| {
            let decoded_len = stream.decoded_len.unwrap_or(stream.raw_len);
            let image_rgba_len = stream.image.as_ref().and_then(|image| {
                let width = usize::try_from(image.width?).ok()?;
                let height = usize::try_from(image.height?).ok()?;
                width.checked_mul(height)?.checked_mul(4)
            });
            total.checked_add(decoded_len.max(image_rgba_len.unwrap_or(0)))
        });
        let reserved_before_render = replay_bytes
            .and_then(|bytes| bytes.checked_add(self.bytes().len()))
            .ok_or_else(|| self.decoded_byte_budget.total_limit_error())?;
        self.decoded_byte_budget
            .charge_resource_bytes(reserved_before_render)?;

        // The interpreter builds its page list by recursing over `/Kids`
        // as soon as the document is opened, before anything can look at
        // the tree through its object model. Bound the tree on this crate's
        // reading of it first.
        if self.page_tree_depth_exceeds(render_guard::MAX_PAGE_TREE_DEPTH) {
            return Err(anyhow!(
                "interpreted render refused: the page tree is nested deeper than {} levels",
                render_guard::MAX_PAGE_TREE_DEPTH
            ));
        }

        // The interpreter recurses, and a stack overflow aborts the process
        // rather than unwinding. Run it on a thread whose stack size is
        // known, so the guard's budget does not depend on how much stack the
        // caller happens to have left. The stack is reserved address space;
        // the system commits pages only as they are touched. If the thread
        // cannot be created the render fails: it never falls back to the
        // caller's stack.
        let pdf_bytes = self.bytes().to_vec();
        let budget = &self.decoded_byte_budget;
        std::thread::scope(|scope| {
            let worker = std::thread::Builder::new()
                .name("pdf-fast-render".to_string())
                .stack_size(render_guard::RENDER_STACK_BYTES)
                .spawn_scoped(scope, move || {
                    Self::interpret_on_render_stack(budget, pdf_bytes, page_object, dpi)
                })
                .map_err(|error| {
                    anyhow!("interpreted render refused: no render thread ({error})")
                })?;
            match worker.join() {
                Ok(result) => result,
                Err(panic) => std::panic::resume_unwind(panic),
            }
        })
    }

    /// Whether any chain of `/Kids` is longer than `limit`, on this crate's
    /// own reading of the objects. Iterative; a cycle counts as too deep.
    #[cfg(feature = "rasterize")]
    fn page_tree_depth_exceeds(&self, limit: usize) -> bool {
        let kids_of = |id: (u32, u16)| -> Vec<(u32, u16)> {
            let Some(kids) = self
                .object_by_id(id.0, id.1)
                .and_then(|object| object.dictionary.as_ref())
                .and_then(|dictionary| dictionary.get("Kids"))
                .and_then(|kids| self.resolve_array(kids))
            else {
                return Vec::new();
            };
            kids.iter()
                .filter_map(|kid| match kid {
                    PdfValue::Reference {
                        object_number,
                        generation,
                    } => Some((*object_number, *generation)),
                    _ => None,
                })
                .collect()
        };
        // Height of each node once known; a node still on the path has none.
        let mut heights: HashMap<(u32, u16), usize> = HashMap::new();
        let mut on_path: HashSet<(u32, u16)> = HashSet::new();
        for object in &self.objects {
            let root = (object.object_number, object.generation);
            if heights.contains_key(&root)
                || object
                    .dictionary
                    .as_ref()
                    .is_none_or(|dictionary| dictionary.get("Kids").is_none())
            {
                continue;
            }
            let mut path = vec![(root, kids_of(root), 0usize)];
            on_path.insert(root);
            while let Some((id, kids, tallest)) = path.last_mut() {
                if let Some(kid) = kids.pop() {
                    if let Some(height) = heights.get(&kid) {
                        *tallest = (*tallest).max(*height);
                    } else if on_path.contains(&kid) {
                        return true;
                    } else {
                        on_path.insert(kid);
                        let next = kids_of(kid);
                        path.push((kid, next, 0));
                        if path.len() > limit {
                            return true;
                        }
                    }
                    continue;
                }
                let (id, height) = (*id, *tallest + 1);
                if height > limit {
                    return true;
                }
                on_path.remove(&id);
                heights.insert(id, height);
                path.pop();
                if let Some((_, _, tallest)) = path.last_mut() {
                    *tallest = (*tallest).max(height);
                }
            }
        }
        false
    }

    /// Opens the document with the full interpreter, vets the page and
    /// renders it. Must run on a stack of
    /// [`render_guard::RENDER_STACK_BYTES`].
    #[cfg(feature = "rasterize")]
    fn interpret_on_render_stack(
        budget: &DecodedByteBudget,
        pdf_bytes: Vec<u8>,
        page_object: (u32, u16),
        dpi: f32,
    ) -> Result<Option<Vec<u8>>> {
        use hayro::hayro_interpret::InterpreterSettings;
        use hayro::hayro_syntax::object::ObjectIdentifier;
        use hayro::hayro_syntax::Pdf;
        use hayro::RenderSettings;
        use std::sync::Arc;

        let Ok(pdf) = Pdf::new(Arc::new(pdf_bytes)) else {
            return Ok(None);
        };
        // A page index counts this crate's pages, which list a page once
        // however often `/Kids` names it. The interpreter lists it once per
        // entry, so the same index can be a different page there. Find the
        // interpreter's copy of the page that was asked for by its object
        // number; a page it does not have is not rendered at all.
        let Ok(object_number) = i32::try_from(page_object.0) else {
            return Ok(None);
        };
        let wanted = ObjectIdentifier::new(object_number, i32::from(page_object.1));
        let Some(page) = pdf
            .pages()
            .iter()
            .find(|page| page.raw().obj_id() == Some(wanted))
        else {
            return Ok(None);
        };
        // The interpreter enters nested content streams with no cycle or
        // depth guard of its own, and a stack overflow there aborts the
        // process. Vet the exact page object it is about to render, on its
        // own object model, so there is no second parser to disagree with
        // it. The check may look at as much decoded data as the budget has
        // left, and takes none of it.
        render_guard::vet_page(page, budget.available_bytes())?;
        let (width, height) = page.render_dimensions();
        if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
            return Ok(None);
        }

        let scale = dpi / 72.0;
        let px_width = (width * scale) as u16;
        let px_height = (height * scale) as u16;
        if px_width == 0 || px_height == 0 {
            return Ok(None);
        }
        let pixel_count = u64::from(px_width)
            .checked_mul(u64::from(px_height))
            .ok_or_else(|| budget.total_limit_error())?;
        if pixel_count > 100_000_000 {
            return Ok(None);
        }
        let rgba_bytes = usize::try_from(
            pixel_count
                .checked_mul(4)
                .ok_or_else(|| budget.total_limit_error())?,
        )
        .map_err(|_| budget.total_limit_error())?;
        budget.charge_resource_bytes(rgba_bytes)?;
        let settings = RenderSettings {
            x_scale: scale,
            y_scale: scale,
            width: Some(px_width),
            height: Some(px_height),
            bg_color: hayro::vello_cpu::color::palette::css::WHITE,
        };
        let pixmap = hayro::render(page, &InterpreterSettings::default(), &settings);
        let width = u32::from(pixmap.width());
        let height = u32::from(pixmap.height());
        let rgba = pixmap.data_as_u8_slice();
        if rgba.len() != rgba_bytes {
            return Err(anyhow!("Hayro raster size changed after bounded preflight"));
        }
        budget.encode_rgba_png(rgba, width, height).map(Some)
    }

    /// Extract the largest embedded image from a page (JPEG, JBIG2, etc.).
    ///
    /// Returns `(raw_bytes, format_hint)`.  Useful for scanned PDFs where
    /// each page is a single bitmap — feed the result to an OCR engine.
    pub fn page_embedded_image(
        &self,
        page_index: usize,
    ) -> Result<Option<(Vec<u8>, Option<String>)>> {
        let pages = self.pages();
        let page = pages
            .iter()
            .find(|page| page.page_index == page_index)
            .ok_or_else(|| anyhow!("page index {} out of bounds", page_index))?;
        let (bytes, format) = self.extract_page_image_payload(page)?;
        Ok(bytes.map(|b| (b, format)))
    }

    /// Extract connected bitmap glyph candidates from the page's largest scanned image.
    #[cfg(feature = "rasterize")]
    pub fn page_bitmap_components(
        &self,
        page_index: usize,
        options: BitmapComponentOptions,
    ) -> Result<Option<BitmapComponentPage>> {
        let pages = self.pages();
        let page = pages
            .iter()
            .find(|page| page.page_index == page_index)
            .ok_or_else(|| anyhow!("page index {} out of bounds", page_index))?;

        if let Some(luma) = self.extract_page_ccitt_luma_image(page)? {
            return bitmap_components::components_from_luma(
                luma.width,
                luma.height,
                &luma.pixels,
                options,
            )
            .map(Some);
        }

        let Some((bytes, _format)) = self.page_embedded_image(page_index)? else {
            return Ok(None);
        };
        let image = image::load_from_memory(&bytes)
            .with_context(|| format!("decode page {page_index} embedded image for components"))?;
        let gray = image.to_luma8();
        let (width, height) = gray.dimensions();
        bitmap_components::components_from_luma(width, height, gray.as_raw(), options).map(Some)
    }

    /// Recover axis-aligned line primitives for a page after CTM resolution.
    pub fn page_line_segments(&self, page_index: usize) -> Result<Vec<LineSegment>> {
        let pages = self.pages();
        let page = pages
            .iter()
            .find(|page| page.page_index == page_index)
            .ok_or_else(|| anyhow!("page index {} out of bounds", page_index))?;
        self.extract_page_line_segments(page)
    }

    /// Recover bounding boxes and identifiers for bitmap image XObjects.
    pub fn page_image_placements(&self, page_index: usize) -> Result<Vec<PdfImagePlacement>> {
        let pages = self.pages();
        let page = pages
            .iter()
            .find(|page| page.page_index == page_index)
            .ok_or_else(|| anyhow!("page index {} out of bounds", page_index))?;
        self.extract_page_image_placements(page)
    }

    /// Build a canonical ruled-table grid from page primitives.
    pub fn page_table_grid(&self, page_index: usize, epsilon: f32) -> Result<Option<TableGrid>> {
        let segments = filter_table_rule_segments(&self.page_line_segments(page_index)?, epsilon);
        if segments.is_empty() {
            return Ok(None);
        }
        let grid = TableGrid::build(&segments, epsilon);
        if !grid.has_cells() || grid.row_count().saturating_mul(grid.col_count()) > 16_384 {
            return Ok(None);
        }
        Ok(Some(grid))
    }

    /// Route native text runs into table cells when a ruled grid exists.
    pub fn page_table_text_assignments(
        &self,
        page_index: usize,
        epsilon: f32,
    ) -> Result<Vec<TableTextAssignment>> {
        let pages = self.pages();
        let page = pages
            .iter()
            .find(|page| page.page_index == page_index)
            .ok_or_else(|| anyhow!("page index {} out of bounds", page_index))?;
        let Some(grid) = self.page_table_grid(page_index, epsilon)? else {
            return Ok(Vec::new());
        };
        Ok(table_text_assignments_for_page(self, page, &grid))
    }

    /// Recover merged table cells and assign native text into rowspan/colspan regions.
    pub fn page_table_layout(
        &self,
        page_index: usize,
        epsilon: f32,
    ) -> Result<Option<TableLayout>> {
        let pages = self.pages();
        let page = pages
            .iter()
            .find(|page| page.page_index == page_index)
            .ok_or_else(|| anyhow!("page index {} out of bounds", page_index))?;
        let grid = match self.page_table_grid(page_index, epsilon)? {
            Some(grid) => grid,
            None => return Ok(None),
        };
        let assignments = table_text_assignments_for_page(self, page, &grid);
        Ok(Some(grid.to_layout(&assignments)))
    }

    /// Recover table layout by projecting OCR boxes from rendered pixel space back into PDF space.
    pub fn page_table_layout_from_ocr(
        &self,
        page_index: usize,
        epsilon: f32,
        dpi: f32,
        ocr_runs: &[OcrTextRun],
    ) -> Result<Option<TableLayout>> {
        let pages = self.pages();
        let page = pages
            .iter()
            .find(|page| page.page_index == page_index)
            .ok_or_else(|| anyhow!("page index {} out of bounds", page_index))?;
        let crop_box = page.page_box().unwrap_or([0.0, 0.0, 612.0, 792.0]);
        let grid = match self.page_table_grid(page_index, epsilon)? {
            Some(grid) => grid,
            None => return Ok(None),
        };
        let assignments = table_text_assignments_from_ocr(&grid, crop_box, dpi, ocr_runs);
        Ok(Some(grid.to_layout(&assignments)))
    }

    /// Recover table layout from a columnar OCR Arrow batch.
    ///
    /// Required columns: `text` (Utf8/LargeUtf8), `px_x`/`px_y`/`px_height`
    /// (Float32 or Float64). Walks columns in place — no intermediate
    /// [`OcrTextRun`] vector. Text is still cloned into assignments when a
    /// run routes into a cell (same as the slice path).
    #[cfg(feature = "arrow")]
    pub fn page_table_layout_from_ocr_batch(
        &self,
        page_index: usize,
        epsilon: f32,
        dpi: f32,
        ocr_batch: &RecordBatch,
    ) -> Result<Option<TableLayout>> {
        let pages = self.pages();
        let page = pages
            .iter()
            .find(|page| page.page_index == page_index)
            .ok_or_else(|| anyhow!("page index {} out of bounds", page_index))?;
        let crop_box = page.page_box().unwrap_or([0.0, 0.0, 612.0, 792.0]);
        let grid = match self.page_table_grid(page_index, epsilon)? {
            Some(grid) => grid,
            None => return Ok(None),
        };
        let assignments = table_text_assignments_from_ocr_batch(&grid, crop_box, dpi, ocr_batch)?;
        Ok(Some(grid.to_layout(&assignments)))
    }

    /// Group merged cells into full table objects on a page.
    pub fn page_tables(&self, page_index: usize, epsilon: f32) -> Result<Vec<PdfTable>> {
        let Some(layout) = self.page_table_layout(page_index, epsilon)? else {
            return Ok(Vec::new());
        };
        Ok(layout.into_tables(page_index))
    }

    /// Group merged OCR-populated cells into full table objects on a page.
    pub fn page_tables_from_ocr(
        &self,
        page_index: usize,
        epsilon: f32,
        dpi: f32,
        ocr_runs: &[OcrTextRun],
    ) -> Result<Vec<PdfTable>> {
        let Some(layout) = self.page_table_layout_from_ocr(page_index, epsilon, dpi, ocr_runs)?
        else {
            return Ok(Vec::new());
        };
        Ok(layout.into_tables(page_index))
    }

    /// Group merged OCR-populated cells from a columnar Arrow OCR batch.
    #[cfg(feature = "arrow")]
    pub fn page_tables_from_ocr_batch(
        &self,
        page_index: usize,
        epsilon: f32,
        dpi: f32,
        ocr_batch: &RecordBatch,
    ) -> Result<Vec<PdfTable>> {
        let Some(layout) =
            self.page_table_layout_from_ocr_batch(page_index, epsilon, dpi, ocr_batch)?
        else {
            return Ok(Vec::new());
        };
        Ok(layout.into_tables(page_index))
    }

    /// Materialize table cells across all pages into an Arrow batch.
    #[cfg(feature = "arrow")]
    pub fn table_layout_record_batch(&self, epsilon: f32) -> Result<RecordBatch> {
        let pages = self.pages();
        let mut rows = Vec::new();
        for page in &pages {
            rows.extend(self.page_table_cell_rows(page.page_index, epsilon)?);
        }
        build_table_layout_record_batch(rows)
    }

    /// Parallel Arrow exhaust for table cells.
    #[cfg(all(feature = "arrow", feature = "parallel"))]
    pub fn table_layout_record_batch_parallel(&self, epsilon: f32) -> Result<RecordBatch> {
        let pages = self.pages();
        let batches = pages
            .par_iter()
            .map(|page| self.page_table_cell_rows(page.page_index, epsilon))
            .collect::<Vec<_>>();
        let rows = batches
            .into_iter()
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        build_table_layout_record_batch(rows)
    }

    /// Serial fallback for table cell Arrow exhaust when `parallel` is disabled.
    #[cfg(all(feature = "arrow", not(feature = "parallel")))]
    pub fn table_layout_record_batch_parallel(&self, epsilon: f32) -> Result<RecordBatch> {
        let pages = self.pages();
        let batches = pages
            .iter()
            .map(|page| self.page_table_cell_rows(page.page_index, epsilon))
            .collect::<Vec<_>>();
        let rows = batches
            .into_iter()
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        build_table_layout_record_batch(rows)
    }

    /// Materialize a single page's recovered table cells into an Arrow batch.
    #[cfg(feature = "arrow")]
    pub fn page_table_layout_record_batch(
        &self,
        page_index: usize,
        epsilon: f32,
    ) -> Result<RecordBatch> {
        build_table_layout_record_batch(self.page_table_cell_rows(page_index, epsilon)?)
    }

    /// Materialize a single page's OCR-populated table cells into an Arrow batch.
    #[cfg(feature = "arrow")]
    pub fn page_table_layout_record_batch_from_ocr(
        &self,
        page_index: usize,
        epsilon: f32,
        dpi: f32,
        ocr_runs: &[OcrTextRun],
    ) -> Result<RecordBatch> {
        let Some(layout) = self.page_table_layout_from_ocr(page_index, epsilon, dpi, ocr_runs)?
        else {
            return build_table_layout_record_batch(Vec::new());
        };
        build_table_layout_record_batch(table_cell_rows_from_layout(page_index, layout))
    }

    /// Materialize OCR-populated table cells from a columnar Arrow OCR batch.
    ///
    /// Prefer this over [`Self::page_table_layout_record_batch_from_ocr`] when
    /// OCR input is already Arrow — avoids allocating `Vec<OcrTextRun>`.
    #[cfg(feature = "arrow")]
    pub fn page_table_layout_record_batch_from_ocr_batch(
        &self,
        page_index: usize,
        epsilon: f32,
        dpi: f32,
        ocr_batch: &RecordBatch,
    ) -> Result<RecordBatch> {
        let Some(layout) =
            self.page_table_layout_from_ocr_batch(page_index, epsilon, dpi, ocr_batch)?
        else {
            return build_table_layout_record_batch(Vec::new());
        };
        build_table_layout_record_batch(table_cell_rows_from_layout(page_index, layout))
    }

    /// Materialize grouped table objects across all pages into an Arrow batch.
    #[cfg(feature = "arrow")]
    pub fn table_record_batch(&self, epsilon: f32) -> Result<RecordBatch> {
        let pages = self.pages();
        let mut rows = Vec::new();
        for page in &pages {
            rows.extend(self.page_table_rows(page.page_index, epsilon)?);
        }
        build_table_record_batch(rows)
    }

    /// Parallel Arrow exhaust for grouped table objects.
    #[cfg(all(feature = "arrow", feature = "parallel"))]
    pub fn table_record_batch_parallel(&self, epsilon: f32) -> Result<RecordBatch> {
        let pages = self.pages();
        let batches = pages
            .par_iter()
            .map(|page| self.page_table_rows(page.page_index, epsilon))
            .collect::<Vec<_>>();
        let rows = batches
            .into_iter()
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        build_table_record_batch(rows)
    }

    /// Serial fallback for grouped table objects when `parallel` is disabled.
    #[cfg(all(feature = "arrow", not(feature = "parallel")))]
    pub fn table_record_batch_parallel(&self, epsilon: f32) -> Result<RecordBatch> {
        let pages = self.pages();
        let batches = pages
            .iter()
            .map(|page| self.page_table_rows(page.page_index, epsilon))
            .collect::<Vec<_>>();
        let rows = batches
            .into_iter()
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        build_table_record_batch(rows)
    }

    /// Materialize grouped table objects for a single page into an Arrow batch.
    #[cfg(feature = "arrow")]
    pub fn page_table_record_batch(&self, page_index: usize, epsilon: f32) -> Result<RecordBatch> {
        build_table_record_batch(self.page_table_rows(page_index, epsilon)?)
    }

    /// Materialize OCR-populated grouped table objects for a single page into an Arrow batch.
    #[cfg(feature = "arrow")]
    pub fn page_table_record_batch_from_ocr(
        &self,
        page_index: usize,
        epsilon: f32,
        dpi: f32,
        ocr_runs: &[OcrTextRun],
    ) -> Result<RecordBatch> {
        build_table_record_batch(table_rows_from_tables(
            page_index,
            self.page_tables_from_ocr(page_index, epsilon, dpi, ocr_runs)?,
        ))
    }

    /// Materialize OCR-populated grouped tables from a columnar Arrow OCR batch.
    #[cfg(feature = "arrow")]
    pub fn page_table_record_batch_from_ocr_batch(
        &self,
        page_index: usize,
        epsilon: f32,
        dpi: f32,
        ocr_batch: &RecordBatch,
    ) -> Result<RecordBatch> {
        build_table_record_batch(table_rows_from_tables(
            page_index,
            self.page_tables_from_ocr_batch(page_index, epsilon, dpi, ocr_batch)?,
        ))
    }

    /// Resolve page-aware text runs for a specific page.
    pub fn semantic_text_runs_for_page(&self, page: &PdfPage) -> Vec<PdfTextRun> {
        let Some(object) = self.object_by_id(page.object_number, page.generation) else {
            return Vec::new();
        };
        let Some(dictionary) = object.dictionary.as_ref() else {
            return Vec::new();
        };
        let resources = page
            .resources
            .as_ref()
            .or_else(|| self.resolve_inherited_resources(dictionary));
        let font_bindings = resources
            .map(|resources| self.resolve_font_bindings(resources))
            .unwrap_or_default();

        let mut runs = Vec::new();
        for stream_index in &page.content_streams {
            let Some(stream) = self.streams.get(*stream_index) else {
                continue;
            };
            let stream_bytes = if stream.kind == PdfStreamKind::Content {
                self.stream_decoded_bytes(*stream_index).or_else(|| {
                    if stream.filters.is_empty()
                        || matches!(stream.decode_status, DecodeStatus::Raw)
                    {
                        self.stream_raw_bytes(*stream_index)
                    } else {
                        None
                    }
                })
            } else if stream.filters.is_empty() {
                let raw = self.stream_raw_bytes(*stream_index);
                if raw.is_some_and(contains_text_operators) {
                    raw
                } else {
                    None
                }
            } else {
                None
            };
            let Some(stream_bytes) = stream_bytes else {
                continue;
            };
            if !contains_text_operators(stream_bytes) {
                continue;
            }
            runs.extend(extract_semantic_text_runs(
                stream_bytes,
                page.page_index,
                *stream_index,
                &font_bindings,
            ));
        }
        runs
    }

    /// Return the page's text runs grouped into blocks sorted in reading
    /// order: columns left-to-right, top-to-bottom within each column.
    ///
    /// Each block carries the indices into the slice returned by
    /// `semantic_text_runs_for_page` that belong to it. Within a block
    /// the indices are *not* re-sorted — callers should keep the runs in
    /// PDF emission order so kerning sequences and per-glyph runs stay
    /// adjacent.
    ///
    /// Reading order is derived from tessellation block bboxes via an
    /// XY-cut style rule: cluster blocks by x_min into columns (a column
    /// gap is ≥10% of page width or 30 pt, whichever is larger), then sort
    /// each column by descending y_max.  Full-width title/headline blocks
    /// (bbox spanning ≥80% of page width) get pulled to the top before any
    /// column content so 2-column papers with a centered title still read
    /// correctly.
    pub fn reading_order_blocks(
        &self,
        page: &PdfPage,
        options: crate::tessellation::TessellationOptions,
    ) -> Result<Vec<ReadingOrderBlock>> {
        let runs = self.semantic_text_runs_for_page(page);
        if runs.is_empty() {
            return Ok(Vec::new());
        }

        let mut sites = Vec::with_capacity(runs.len());
        for (i, run) in runs.iter().enumerate() {
            sites.push(crate::tessellation::GlyphSite {
                index: i,
                x: run.x.unwrap_or(0.0) as f64,
                y: run.y.unwrap_or(0.0) as f64,
                x_end: (run.x.unwrap_or(0.0) + run.w.unwrap_or(0.0)) as f64,
                font_size: run.font_size.unwrap_or(10.0) as f64,
                text: run.text.clone(),
                font_name: run.font.clone(),
            });
        }

        let segments = self.page_line_segments(page.page_index).unwrap_or_default();
        let mut h_barriers = Vec::new();
        let mut v_barriers = Vec::new();
        for seg in &segments {
            if (seg.y0 - seg.y1).abs() < 1.0 {
                h_barriers.push(crate::tessellation::HBarrier {
                    y: seg.y0 as f64,
                    x_start: seg.x0.min(seg.x1) as f64,
                    x_end: seg.x0.max(seg.x1) as f64,
                });
            }
            if (seg.x0 - seg.x1).abs() < 1.0 {
                v_barriers.push(crate::tessellation::VBarrier {
                    x: seg.x0 as f64,
                    y_start: seg.y0.min(seg.y1) as f64,
                    y_end: seg.y0.max(seg.y1) as f64,
                });
            }
        }

        let page_width = page
            .page_box()
            .map(|b| (b[2] - b[0]).abs() as f64)
            .unwrap_or(612.0);

        // Detect column gutters and add VBarriers so the convergence
        // doesn't merge across columns. Without these the energy-thresholded
        // merge greedily clusters left-column and right-column blocks at the
        // same y because the cross-column horizontal distance falls below
        // `block_threshold` for narrow columns.
        v_barriers.extend(detect_column_gutters(&sites, page_width));

        let constraints = crate::tessellation::PathConstraints {
            h_barriers,
            v_barriers,
            cells: Vec::new(),
        };

        let edges = crate::tessellation::build_adjacency(&sites, 5);
        let regions = crate::tessellation::converge(&sites, &edges, options, &constraints);

        let mut blocks: Vec<ReadingOrderBlock> = regions
            .into_iter()
            .filter(|r| !r.site_indices.is_empty())
            .map(|r| ReadingOrderBlock {
                run_indices: r.site_indices.iter().map(|&i| sites[i].index).collect(),
                x_min: r.x_min as f32,
                y_min: r.y_min as f32,
                x_max: r.x_max as f32,
                y_max: r.y_max as f32,
            })
            .collect();

        sort_blocks_reading_order(&mut blocks, page_width as f32);
        Ok(blocks)
    }

    /// Run the full page-level tessellation pipeline (glyph adjacency →
    /// converge → role assignment) for a single page and return the
    /// [`PageExtraction`](crate::tessellation::PageExtraction).
    pub fn tessellate_page(
        &self,
        page_index: usize,
        options: crate::tessellation::TessellationOptions,
    ) -> Result<crate::tessellation::PageExtraction> {
        let Some(page) = self
            .pages()
            .into_iter()
            .find(|p| p.page_index == page_index)
        else {
            return Err(anyhow!("Page not found"));
        };

        let runs = self.semantic_text_runs_for_page(&page);

        let mut sites = Vec::new();
        for (i, run) in runs.iter().enumerate() {
            sites.push(crate::tessellation::GlyphSite {
                index: i,
                x: run.x.unwrap_or(0.0) as f64,
                y: run.y.unwrap_or(0.0) as f64,
                x_end: (run.x.unwrap_or(0.0) + run.w.unwrap_or(0.0)) as f64,
                font_size: run.font_size.unwrap_or(10.0) as f64,
                text: run.text.clone(),
                font_name: run.font.clone(),
            });
        }

        let segments = self.page_line_segments(page_index).unwrap_or_default();
        let mut h_barriers = Vec::new();
        let mut v_barriers = Vec::new();
        for seg in &segments {
            if (seg.y0 - seg.y1).abs() < 1.0 {
                h_barriers.push(crate::tessellation::HBarrier {
                    y: seg.y0 as f64,
                    x_start: seg.x0.min(seg.x1) as f64,
                    x_end: seg.x0.max(seg.x1) as f64,
                });
            }
            if (seg.x0 - seg.x1).abs() < 1.0 {
                v_barriers.push(crate::tessellation::VBarrier {
                    x: seg.x0 as f64,
                    y_start: seg.y0.min(seg.y1) as f64,
                    y_end: seg.y0.max(seg.y1) as f64,
                });
            }
        }

        let constraints = crate::tessellation::PathConstraints {
            h_barriers,
            v_barriers,
            cells: Vec::new(),
        };

        let edges = crate::tessellation::build_adjacency(&sites, 5);
        let regions = crate::tessellation::converge(&sites, &edges, options, &constraints);

        let mut annotated_regions: Vec<crate::tessellation::AnnotatedRegion> = regions
            .into_iter()
            .map(|r| {
                let mut text = String::new();
                let mut last_y = -1.0;

                let mut indices = r.site_indices.clone();
                indices.sort_by(|&a, &b| {
                    let sy_diff = (sites[a].y - sites[b].y).abs();
                    if sy_diff > 2.0 {
                        sites[a].y.total_cmp(&sites[b].y)
                    } else {
                        sites[a].x.total_cmp(&sites[b].x)
                    }
                });

                for &idx in &indices {
                    let site = &sites[idx];
                    if last_y != -1.0 && (site.y - last_y).abs() > site.font_size * 0.5 {
                        text.push('\n');
                    } else if !text.is_empty() && !text.ends_with(' ') && !text.ends_with('\n') {
                        text.push(' ');
                    }
                    text.push_str(&site.text);
                    last_y = site.y;
                }

                let font = r
                    .site_indices
                    .first()
                    .and_then(|&idx| sites[idx].font_name.as_ref())
                    .map(|n| crate::tessellation::classify_font(n));

                crate::tessellation::AnnotatedRegion {
                    role: crate::tessellation::SemanticRole::Paragraph,
                    text,
                    bbox: crate::tessellation::BBox {
                        x: r.x_min,
                        y: r.y_min,
                        width: r.x_max - r.x_min,
                        height: r.y_max - r.y_min,
                    },
                    font_size: r.dominant_font_size,
                    font,
                    color: None,
                    confidence: 1.0,
                }
            })
            .collect();

        let (page_width, page_height) = page
            .page_box()
            .map(|b| ((b[2] - b[0]).abs() as f64, (b[3] - b[1]).abs() as f64))
            .unwrap_or((800.0, 1000.0));

        crate::tessellation::assign_roles(&mut annotated_regions, page_width, page_height);

        let mut extracted_tables = Vec::new();
        if let Ok(tables) = self.page_tables(page_index, 0.5) {
            for pt in tables {
                let mut rows: Vec<Vec<String>> =
                    vec![vec![String::new(); pt.col_count]; pt.row_count];
                for cell in &pt.cells {
                    let row_end = cell.row_end.min(pt.row_count);
                    let col_end = cell.col_end.min(pt.col_count);
                    for row in rows.iter_mut().take(row_end).skip(cell.row_start) {
                        for slot in row.iter_mut().take(col_end).skip(cell.col_start) {
                            *slot = cell.text.clone();
                        }
                    }
                }

                let headers = if pt.header_row_count > 0 && !rows.is_empty() {
                    rows.remove(0)
                } else {
                    Vec::new()
                };

                extracted_tables.push(crate::tessellation::ExtractedTable {
                    headers,
                    rows,
                    bbox: crate::tessellation::BBox {
                        x: pt.bbox[0] as f64,
                        y: pt.bbox[1] as f64,
                        width: (pt.bbox[2] - pt.bbox[0]).abs() as f64,
                        height: (pt.bbox[3] - pt.bbox[1]).abs() as f64,
                    },
                    has_explicit_borders: true,
                });

                for r in annotated_regions.iter_mut() {
                    let r_mid_y = r.bbox.y + r.bbox.height / 2.0;
                    if r_mid_y >= pt.bbox[1] as f64 && r_mid_y <= pt.bbox[3] as f64 {
                        r.role = crate::tessellation::SemanticRole::TableCell;
                    }
                }
            }
        }

        Ok(crate::tessellation::PageExtraction {
            page_number: page_index,
            width: page_width,
            height: page_height,
            regions: annotated_regions,
            tables: extracted_tables,
            charts: Vec::new(),
            images: Vec::new(),
            shapes: Vec::new(),
            metadata: crate::tessellation::PageMetadata {
                text_block_count: 0,
                table_count: 0,
                chart_count: 0,
                image_count: 0,
                total_characters: 0,
                dominant_font: None,
                dominant_font_size: 12.0,
                languages_detected: Vec::new(),
                has_selectable_text: true,
                extraction_warnings: Vec::new(),
            },
        })
    }

    /// Extract intelligently grouped layout blocks (Paragraphs, Headings, Tables)
    /// using Category-theoretic Spatial Graph Constraints (X/Y proximity edges).
    pub fn layout_blocks(&self) -> Vec<PdfTextBlock> {
        let mut blocks = Vec::new();
        for page in self.pages() {
            let mut runs = self.semantic_text_runs_for_page(&page);
            runs.retain(|r| !r.text.trim().is_empty());
            runs.sort_by(|a, b| a.y.unwrap_or(0.0).total_cmp(&b.y.unwrap_or(0.0)));

            let n = runs.len();
            let mut adj = vec![Vec::new(); n];
            for i in 0..n {
                for j in (i + 1)..n {
                    let y1 = runs[i].y.unwrap_or(0.0);
                    let y2 = runs[j].y.unwrap_or(0.0);
                    let x1 = runs[i].x.unwrap_or(0.0);
                    let x2 = runs[j].x.unwrap_or(0.0);
                    let fs1 = runs[i].font_size.unwrap_or(10.0).max(1.0);
                    let fs2 = runs[j].font_size.unwrap_or(10.0).max(1.0);
                    let max_fs = fs1.max(fs2);

                    let dy = (y1 - y2).abs();
                    let dx = (x1 - x2).abs();

                    // Rules for identical paragraph block:
                    // 1. Same line (horizontal edge): Y is very close, X is somewhat close
                    let horizontal = dy < (max_fs * 0.5) && dx < (max_fs * 3.0);
                    // 2. Consecutive lines (vertical edge): X is left-aligned, Y is consecutive
                    let vertical = dx < (max_fs * 1.5) && dy < (max_fs * 2.0);

                    if horizontal || vertical {
                        adj[i].push(j);
                        adj[j].push(i);
                    } else if dy >= (max_fs * 2.0) {
                        break;
                    }
                }
            }

            let mut visited = vec![false; n];
            let mut components = Vec::new();
            for i in 0..n {
                if !visited[i] {
                    let mut comp = Vec::new();
                    let mut queue = vec![i];
                    visited[i] = true;

                    while let Some(curr) = queue.pop() {
                        comp.push(curr);
                        for &neighbor in &adj[curr] {
                            if !visited[neighbor] {
                                visited[neighbor] = true;
                                queue.push(neighbor);
                            }
                        }
                    }
                    components.push(comp);
                }
            }

            for comp in components {
                let mut row_nodes = comp.clone();
                row_nodes.sort_by(|&a, &b| {
                    let ya = runs[a].y.unwrap_or(0.0);
                    let yb = runs[b].y.unwrap_or(0.0);
                    if (ya - yb).abs() < 5.0 {
                        let xa = runs[a].x.unwrap_or(0.0);
                        let xb = runs[b].x.unwrap_or(0.0);
                        xa.total_cmp(&xb)
                    } else {
                        yb.total_cmp(&ya)
                    }
                });

                let mut text = String::new();
                let mut min_x = f32::MAX;
                let mut min_y = f32::MAX;
                let mut max_x = f32::MIN;
                let mut max_y = f32::MIN;

                let mut last_y = f32::MAX;

                for &idx in &row_nodes {
                    let run = &runs[idx];
                    let x = run.x.unwrap_or(0.0);
                    let y = run.y.unwrap_or(0.0);
                    let w = run.w.unwrap_or(0.0);
                    let h = run.h.unwrap_or(5.0);

                    min_x = min_x.min(x);
                    min_y = min_y.min(y);
                    max_x = max_x.max(x + w);
                    max_y = max_y.max(y + h);

                    if last_y != f32::MAX && (y - last_y).abs() > 5.0 {
                        text.push('\n');
                    } else if !text.is_empty() && !text.ends_with(' ') && !text.ends_with('\n') {
                        text.push(' ');
                    }
                    text.push_str(run.text.trim());
                    last_y = y;
                }

                blocks.push(PdfTextBlock {
                    page_index: page.page_index,
                    text,
                    x: min_x,
                    y: min_y,
                    w: if max_x > min_x { max_x - min_x } else { 0.0 },
                    h: if max_y > min_y { max_y - min_y } else { 0.0 },
                });
            }
        }

        // Pages in order, then reading order within each page. The previous
        // single comparator mixed a 5pt band with a y comparison and was
        // therefore intransitive; see sort_into_reading_order.
        blocks.sort_by_key(|block| block.page_index);
        let mut start = 0;
        while start < blocks.len() {
            let page = blocks[start].page_index;
            let mut end = start + 1;
            while end < blocks.len() && blocks[end].page_index == page {
                end += 1;
            }
            sort_into_reading_order(&mut blocks[start..end], 5.0, |b| b.y, |b| b.x);
            start = end;
        }

        blocks
    }

    #[cfg(feature = "arrow")]
    fn page_multimodal_row(&self, page: &PdfPage) -> Result<PageMultimodalRow> {
        let mut runs = self.semantic_text_runs_for_page(page);
        sort_into_reading_order(
            &mut runs,
            4.0,
            |run| run.y.unwrap_or(0.0),
            |run| run.x.unwrap_or(0.0),
        );

        let input_text = runs
            .iter()
            .map(|run| run.text.trim())
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        let input_ids = input_text.chars().map(u32::from).collect::<Vec<_>>();
        let bbox_2d = normalize_page_bbox(page.page_box(), &runs);
        let (image_bytes, image_format) = if input_text.trim().is_empty() {
            #[cfg(feature = "rasterize")]
            {
                match self.rasterized_page_image_payload(page, 300.0)? {
                    Some(payload) => payload,
                    None => self.extract_page_image_payload(page)?,
                }
            }
            #[cfg(not(feature = "rasterize"))]
            {
                self.extract_page_image_payload(page)?
            }
        } else {
            self.extract_page_image_payload(page)?
        };

        Ok(PageMultimodalRow {
            page_index: page.page_index as u32,
            input_text,
            input_ids,
            bbox_2d,
            image_bytes,
            image_format,
        })
    }

    fn extract_page_line_segments(&self, page: &PdfPage) -> Result<Vec<LineSegment>> {
        let resources = page.resources.as_ref();
        let streams = self.page_content_streams_for_raster(page)?;
        if streams.is_empty() {
            return Ok(Vec::new());
        }
        let stream_refs = streams.iter().map(Vec::as_slice).collect::<Vec<_>>();
        Ok(extract_table_line_segments_and_images(self, &stream_refs, resources)?.0)
    }

    fn extract_page_image_placements(&self, page: &PdfPage) -> Result<Vec<PdfImagePlacement>> {
        let resources = page.resources.as_ref();
        let streams = self.page_content_streams_for_raster(page)?;
        if streams.is_empty() {
            return Ok(Vec::new());
        }
        let stream_refs = streams.iter().map(Vec::as_slice).collect::<Vec<_>>();
        Ok(extract_table_line_segments_and_images(self, &stream_refs, resources)?.1)
    }

    #[cfg(feature = "arrow")]
    fn page_table_cell_rows(&self, page_index: usize, epsilon: f32) -> Result<Vec<TableCellRow>> {
        let Some(layout) = self.page_table_layout(page_index, epsilon)? else {
            return Ok(Vec::new());
        };
        Ok(table_cell_rows_from_layout(page_index, layout))
    }

    #[cfg(feature = "arrow")]
    fn page_table_rows(&self, page_index: usize, epsilon: f32) -> Result<Vec<TableRow>> {
        Ok(table_rows_from_tables(
            page_index,
            self.page_tables(page_index, epsilon)?,
        ))
    }

    #[cfg(feature = "rasterize")]
    fn rasterized_page_image_payload(
        &self,
        page: &PdfPage,
        dpi: f32,
    ) -> Result<Option<PageImagePayload>> {
        let Some(crop_box) = page.page_box() else {
            return Ok(None);
        };
        let resources = page.resources.as_ref();
        let streams = self.page_content_streams_for_raster(page)?;
        if streams.is_empty() {
            return Ok(None);
        }
        let stream_refs = streams.iter().map(Vec::as_slice).collect::<Vec<_>>();
        let Some(png) = rasterize_page_vectors(self, &stream_refs, resources, crop_box, dpi)?
        else {
            return Ok(None);
        };
        Ok(Some((Some(png), Some("png".to_string()))))
    }

    /// Text-line boxes for a page, in PAGE space, derived from vector glyph
    /// geometry — a detector-free replacement for running a DB text detector.
    ///
    /// WHY: outlined-glyph PDFs (the DocuSign-signed contract class) carry the
    /// exact position of every glyph in the content stream. Running a neural
    /// text detector over a raster of that page re-discovers, approximately and
    /// at ~hundreds of ms/page, information we already hold exactly. Grouping
    /// glyph runs into lines here costs microseconds and is EXACT, so an OCR
    /// pipeline only needs the recogniser: crop each returned box, batch it
    /// through `rec`, done.
    ///
    /// Returns `(x0, y0, x1, y1)` boxes in page units (y is top-down, matching
    /// `GlyphRun::page_y`), ordered top-to-bottom then left-to-right. Runs whose
    /// baselines sit within `line_tolerance` page units of each other and that
    /// are horizontally adjacent merge into one line box.
    /// As [`Self::page_text_line_boxes`], but `split_columns` chooses the crop
    /// granularity. `true` splits a line at wide horizontal gaps, so each table
    /// cell is its own box (better when cell-level provenance matters). `false`
    /// keeps one box per visual line — far fewer, wider crops, which is what a
    /// text recogniser wants: recogniser cost is dominated by the number of
    /// crops, so whole lines cut OCR time several-fold at equal coverage.
    pub fn page_text_line_boxes_with(
        &self,
        page_index: usize,
        line_tolerance: f32,
        split_columns: bool,
    ) -> Result<Vec<[f32; 4]>> {
        self.text_line_boxes_impl(page_index, line_tolerance, split_columns)
    }

    /// Text-line boxes with column splitting enabled (one box per table cell).
    /// See [`Self::page_text_line_boxes_with`] to choose crop granularity.
    pub fn page_text_line_boxes(
        &self,
        page_index: usize,
        line_tolerance: f32,
    ) -> Result<Vec<[f32; 4]>> {
        self.text_line_boxes_impl(page_index, line_tolerance, true)
    }

    fn text_line_boxes_impl(
        &self,
        page_index: usize,
        line_tolerance: f32,
        split_columns: bool,
    ) -> Result<Vec<[f32; 4]>> {
        let runs = self.page_vector_text_runs(page_index)?;
        // Glyph ORIGINS in page space. Two things the run data cannot give us:
        // glyph width (contours are normalised to a 0..1 unit square for the
        // shape hash — lossy by design) and font size (`GlyphRun::scale` is
        // only recovered from rotation matrices, so ordinary text leaves it at
        // 1.0). Both are unnecessary: line GROUPING comes from baseline bands,
        // and the line HEIGHT is measured from the spacing between consecutive
        // bands — the document's own leading, which is exactly the quantity a
        // crop needs.
        let origins: Vec<(f32, f32, ())> = runs
            .iter()
            .flat_map(|run| {
                run.glyphs.iter().map(move |glyph| {
                    (
                        run.page_x + glyph.x_offset,
                        // per-glyph INK-TOP y: run origins scatter by ~5pt
                        // along one visual line (contour y-offsets compensate),
                        // which merged adjacent lines when banding on them
                        run.page_y + glyph.y_offset,
                        (),
                    )
                })
            })
            .collect();
        Ok(
            band_text_lines_public(origins, line_tolerance, split_columns)
                .into_iter()
                .map(|(bbox, _)| bbox)
                .collect(),
        )
    }

    /// Extract vector glyph runs for a single page by index. Errors when the
    /// `page_index` is out of bounds.
    pub fn page_vector_text_runs(&self, page_index: usize) -> Result<Vec<GlyphRun>> {
        let pages = self.pages();
        let page = pages
            .iter()
            .find(|page| page.page_index == page_index)
            .ok_or_else(|| anyhow!("page index {} out of bounds", page_index))?;
        vector_text::extract_page_vector_text_runs_checked(self, page)
    }

    /// Cluster every vector glyph in the document by SHAPE, with tolerance.
    ///
    /// `shape_hash` (exact FNV over a 1/255 grid) is too brittle for identity:
    /// a contour point sitting on a cell boundary flips the hash under float
    /// noise, and with ~40 points per glyph almost every instance lands in its
    /// own bucket (measured: 7.4k hashes for 15.7k glyphs on a real contract —
    /// ~2x dedup where ~50x is the truth). Identity needs tolerance, not
    /// exactness: two glyphs are the same shape iff they have the same contour
    /// structure (contour count, per-contour point counts and kinds) and every
    /// normalized point pair is within `eps`.
    ///
    /// Returns per-page `(page_x, page_y, cluster_id)` triples. Cluster ids are
    /// document-scoped and dense from 0. Candidate lookup is bucketed by the
    /// structural signature, so matching stays near-linear.
    /// Compatibility wrapper that returns an empty document if governed
    /// vector extraction fails. New callers should use
    /// [`Self::vector_glyph_clusters_checked`].
    pub fn vector_glyph_clusters(&self, eps: f32) -> Vec<Vec<(f32, f32, u32)>> {
        self.vector_glyph_clusters_checked(eps).unwrap_or_default()
    }

    /// Checked document-wide vector glyph clustering. Resource-budget and
    /// bounded Form traversal failures abort the request.
    pub fn vector_glyph_clusters_checked(&self, eps: f32) -> Result<Vec<Vec<(f32, f32, u32)>>> {
        use std::collections::HashMap;

        // (contour count, total points, kinds fingerprint) → candidate reps.
        // Size is compared as a RATIO during candidate verification (not a
        // bucketed key, which would flip at bucket boundaries): scale
        // invariance is not worth impurity — small glyphs flatten to so few
        // points that distinct digits become indistinguishable within `eps`,
        // and a digit merge is a fabricated value downstream. Same glyph at
        // another size just becomes another cluster, labeled independently.
        let mut buckets: HashMap<(u16, u16, u64), Vec<u32>> = HashMap::new();
        let mut reps: Vec<(f32, Vec<Vec<vector_text::BezPt>>)> = Vec::new();
        let mut out = Vec::new();

        for page in &self.pages() {
            let runs = vector_text::extract_page_vector_text_runs_checked(self, page)?;
            let mut page_out = Vec::new();
            for run in &runs {
                for glyph in &run.glyphs {
                    if glyph.contours.is_empty() {
                        continue;
                    }
                    let n_contours = glyph.contours.len().min(u16::MAX as usize) as u16;
                    let total_pts: usize = glyph.contours.iter().map(Vec::len).sum();
                    let mut kinds_fp = 14695981039346656037u64;
                    for contour in &glyph.contours {
                        for pt in contour {
                            kinds_fp ^= pt.kind as u64;
                            kinds_fp = kinds_fp.wrapping_mul(1099511628211);
                        }
                        kinds_fp ^= 0xff;
                        kinds_fp = kinds_fp.wrapping_mul(1099511628211);
                    }
                    let key = (
                        n_contours,
                        total_pts.min(u16::MAX as usize) as u16,
                        kinds_fp,
                    );
                    let candidates = buckets.entry(key).or_default();
                    let mut cluster = None;
                    'cand: for &rep_id in candidates.iter() {
                        let (rep_size, rep) = &reps[rep_id as usize];
                        // same shape at a clearly different size is a
                        // different cluster (see bucket comment above)
                        if glyph.size < rep_size * 0.9 || glyph.size > rep_size * 1.1 {
                            continue 'cand;
                        }
                        for (rc, gc) in rep.iter().zip(&glyph.contours) {
                            if rc.len() != gc.len() {
                                continue 'cand;
                            }
                            for (rp, gp) in rc.iter().zip(gc) {
                                if (rp.x - gp.x).abs() > eps || (rp.y - gp.y).abs() > eps {
                                    continue 'cand;
                                }
                            }
                        }
                        cluster = Some(rep_id);
                        break;
                    }
                    let id = cluster.unwrap_or_else(|| {
                        let id = reps.len() as u32;
                        reps.push((glyph.size, glyph.contours.clone()));
                        candidates.push(id);
                        id
                    });
                    page_out.push((run.page_x + glyph.x_offset, run.page_y + glyph.y_offset, id));
                }
            }
            out.push(page_out);
        }
        Ok(out)
    }

    /// Smooth per-glyph shape descriptors — the differentiable identity that
    /// replaces brittle exact matching. Each glyph's normalized contours are
    /// resampled to `k` arc-length-uniform points (allocated across contours
    /// proportionally to their length, contours in (min_y, min_x) order so
    /// concatenation is stable), centroid-centered and RMS-radius scaled.
    /// Identity then lives in a METRIC space: instance noise moves the
    /// descriptor slightly and the distance stays small, while genuinely
    /// different shapes stay far — no eps cliff, and a margin threshold gives
    /// a principled reject (send to the neural recognizer) instead of a
    /// silent wrong merge. Per page: `(page_x, page_y, size, [x0,y0,x1,y1,…])`.
    /// Compatibility wrapper that returns an empty document if governed
    /// vector extraction fails. New callers should use
    /// [`Self::vector_glyph_descriptors_checked`].
    pub fn vector_glyph_descriptors(&self, k: usize) -> Vec<Vec<GlyphGeom>> {
        self.vector_glyph_descriptors_checked(k).unwrap_or_default()
    }

    /// Checked document-wide vector glyph descriptors. Resource-budget and
    /// bounded Form traversal failures abort the request.
    pub fn vector_glyph_descriptors_checked(&self, k: usize) -> Result<Vec<Vec<GlyphGeom>>> {
        let k = k.max(8);
        let mut out = Vec::new();
        for page in &self.pages() {
            let runs = vector_text::extract_page_vector_text_runs_checked(self, page)?;
            let mut page_out = Vec::new();
            for run in &runs {
                for glyph in &run.glyphs {
                    if glyph.contours.is_empty() {
                        continue;
                    }
                    let mut contours: Vec<&Vec<vector_text::BezPt>> =
                        glyph.contours.iter().filter(|c| c.len() >= 2).collect();
                    if contours.is_empty() {
                        continue;
                    }
                    contours.sort_by(|a, b| {
                        let ka = a
                            .iter()
                            .map(|p| (p.y, p.x))
                            .fold((f32::MAX, f32::MAX), |acc, v| if v < acc { v } else { acc });
                        let kb = b
                            .iter()
                            .map(|p| (p.y, p.x))
                            .fold((f32::MAX, f32::MAX), |acc, v| if v < acc { v } else { acc });
                        // Lexicographic on (y, x) with a total order on each
                        // component: a tuple has no total_cmp, and comparing the
                        // pair with `<` is exactly the partial order that made
                        // this sort panic.
                        ka.0.total_cmp(&kb.0).then(ka.1.total_cmp(&kb.1))
                    });
                    let lens: Vec<f32> = contours
                        .iter()
                        .map(|c| {
                            c.windows(2)
                                .map(|w| {
                                    ((w[1].x - w[0].x).powi(2) + (w[1].y - w[0].y).powi(2)).sqrt()
                                })
                                .sum::<f32>()
                                .max(1e-4)
                        })
                        .collect();
                    let total: f32 = lens.iter().sum();
                    // Exact-k point allocation (largest-remainder) so every
                    // descriptor has the same dimensionality — required for
                    // vectorized distance in the metric space.
                    let mut alloc: Vec<usize> = lens
                        .iter()
                        .map(|l| ((l / total) * k as f32) as usize)
                        .collect();
                    for a in alloc.iter_mut() {
                        *a = (*a).max(2);
                    }
                    let mut assigned: usize = alloc.iter().sum();
                    let mut rema: Vec<(f32, usize)> = lens
                        .iter()
                        .enumerate()
                        .map(|(i, l)| ((l / total) * k as f32 - alloc[i] as f32, i))
                        .collect();
                    rema.sort_by(|a, b| b.0.total_cmp(&a.0));
                    let mut ri = 0;
                    while assigned < k && !rema.is_empty() {
                        alloc[rema[ri % rema.len()].1] += 1;
                        assigned += 1;
                        ri += 1;
                    }
                    let mut pts: Vec<(f32, f32)> = Vec::with_capacity(k);
                    for ((c, clen), &n) in contours.iter().zip(&lens).zip(&alloc) {
                        // arc-length-uniform resample of this contour polyline
                        let step = clen / (n as f32);
                        let mut target = 0.0f32;
                        let mut acc = 0.0f32;
                        let mut seg = 0usize;
                        for _ in 0..n {
                            while seg + 1 < c.len() {
                                let d = ((c[seg + 1].x - c[seg].x).powi(2)
                                    + (c[seg + 1].y - c[seg].y).powi(2))
                                .sqrt();
                                if acc + d >= target || seg + 2 >= c.len() {
                                    let t = if d > 1e-6 {
                                        ((target - acc) / d).clamp(0.0, 1.0)
                                    } else {
                                        0.0
                                    };
                                    pts.push((
                                        c[seg].x + t * (c[seg + 1].x - c[seg].x),
                                        c[seg].y + t * (c[seg + 1].y - c[seg].y),
                                    ));
                                    break;
                                }
                                acc += d;
                                seg += 1;
                            }
                            target += step;
                        }
                    }
                    if pts.is_empty() {
                        continue;
                    }
                    let (mut cx, mut cy) = (0.0f32, 0.0f32);
                    for (x, y) in &pts {
                        cx += x;
                        cy += y;
                    }
                    cx /= pts.len() as f32;
                    cy /= pts.len() as f32;
                    let rms = (pts
                        .iter()
                        .map(|(x, y)| (x - cx).powi(2) + (y - cy).powi(2))
                        .sum::<f32>()
                        / pts.len() as f32)
                        .sqrt()
                        .max(1e-4);
                    let mut desc = Vec::with_capacity(k * 2);
                    for (x, y) in pts.iter().take(k) {
                        desc.push((x - cx) / rms);
                        desc.push((y - cy) / rms);
                    }
                    // guard: many-contour glyphs can overflow k via the min-2
                    // floor; equal-length descriptors are non-negotiable, so
                    // pad deterministically with the last point
                    while desc.len() < k * 2 {
                        let n = desc.len();
                        desc.push(desc[n - 2]);
                        desc.push(desc[n - 1]);
                    }
                    page_out.push((
                        run.page_x + glyph.x_offset,
                        // per-glyph INK y: run origins scatter by several pt
                        // along one visual line while contour y-offsets
                        // compensate — banding on the run origin merges
                        // adjacent lines (measured: 5pt intra-line scatter vs
                        // 9pt real line steps on the DocuSign contract)
                        run.page_y + glyph.y_offset,
                        glyph.size,
                        desc,
                    ));
                }
            }
            out.push(page_out);
        }
        Ok(out)
    }

    /// Descriptors + line banding from ONE glyph enumeration, so the index
    /// spaces can never drift apart. Per page: the page's glyphs as
    /// `(page_x, page_y, size, descriptor)` plus visual lines as
    /// `(crop_box, [glyph_index, ...])` with members sorted by x.
    #[allow(clippy::type_complexity)]
    pub fn vector_glyph_descriptor_lines(
        &self,
        k: usize,
        line_tolerance: f32,
        split_columns: bool,
    ) -> Vec<(Vec<GlyphGeom>, Vec<([f32; 4], Vec<u32>)>)> {
        self.vector_glyph_descriptor_lines_checked(k, line_tolerance, split_columns)
            .unwrap_or_default()
    }

    /// Checked descriptor and line-banding surface. Governed extraction
    /// failures abort instead of becoming an empty document.
    #[allow(clippy::type_complexity)]
    pub fn vector_glyph_descriptor_lines_checked(
        &self,
        k: usize,
        line_tolerance: f32,
        split_columns: bool,
    ) -> Result<Vec<(Vec<GlyphGeom>, Vec<([f32; 4], Vec<u32>)>)>> {
        Ok(self
            .vector_glyph_descriptors_checked(k)?
            .into_iter()
            .map(|mut page| {
                // Character-scale ink only. Vector pages carry page frames,
                // table rules and seals as paths too; a page-sized "glyph"
                // (measured: size 1121 vs text sizes 4–9) both pollutes the
                // glyph stream and drags whole baseline bands off the text
                // (its y participates in the running-baseline update). Text
                // dominates by count, so the median size IS the font size —
                // gate relative to it rather than any absolute constant.
                let mut sizes: Vec<f32> = page.iter().map(|g| g.2).collect();
                sizes.sort_by(|a, b| a.total_cmp(b));
                if let Some(&median) = sizes.get(sizes.len() / 2) {
                    // low gate at 7% of the font size: a body-text period is
                    // ~1.2pt against a 7.2pt median — the 15% floor was
                    // silently eating dots, slashes and hyphens (measured:
                    // 18 glyphs vs 22 recognizer tokens on a CNPJ table row,
                    // the 4 missing being exactly the punctuation)
                    let (lo, hi) = (median * 0.07, median * 6.0);
                    page.retain(|g| g.2 >= lo && g.2 <= hi);
                }
                let items: Vec<(f32, f32, u32)> = page
                    .iter()
                    .enumerate()
                    .map(|(i, g)| (g.0, g.1, i as u32))
                    .collect();
                let lines = band_text_lines_public(items, line_tolerance, split_columns)
                    .into_iter()
                    .map(|(bbox, members)| (bbox, members.into_iter().map(|(_, i)| i).collect()))
                    .collect();
                (page, lines)
            })
            .collect())
    }

    /// TRUE-SCAN path: glyph geometry from the RASTER, not the content
    /// stream. Renders the page, binarizes, extracts connected-component ink
    /// glyphs with Moore-traced outlines (see [`raster_glyphs`]), and bands
    /// them with the same `band_text_lines` pass as the vector path. Output
    /// is shape-identical to [`Self::vector_glyph_descriptor_lines`] for one
    /// page, so the entire self-labelled decoding stack runs unchanged on
    /// scanned input.
    #[allow(clippy::type_complexity)]
    pub fn raster_glyph_descriptor_lines(
        &self,
        page_index: usize,
        dpi: f32,
        k: usize,
        threshold: u8,
        line_tolerance: f32,
        split_columns: bool,
    ) -> Result<Option<(Vec<GlyphGeom>, Vec<([f32; 4], Vec<u32>)>)>> {
        #[cfg(feature = "rasterize")]
        {
            self.raster_glyph_descriptor_lines_with(
                page_index,
                dpi,
                k,
                threshold,
                line_tolerance,
                split_columns,
                false,
            )
        }
        #[cfg(not(feature = "rasterize"))]
        {
            let _ = (page_index, dpi, k, threshold, line_tolerance, split_columns);
            Ok(None)
        }
    }

    /// [`Self::raster_glyph_descriptor_lines`] with an `interpreted` switch:
    /// when true, the page is rendered by the FULL interpreter (fonts
    /// painted) instead of the vectors-only fast path — required for
    /// born-digital font-text pages, where the fast path yields a textless
    /// rule-only image.
    #[cfg(feature = "rasterize")]
    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    pub fn raster_glyph_descriptor_lines_with(
        &self,
        page_index: usize,
        dpi: f32,
        k: usize,
        threshold: u8,
        line_tolerance: f32,
        split_columns: bool,
        interpreted: bool,
    ) -> Result<Option<(Vec<GlyphGeom>, Vec<([f32; 4], Vec<u32>)>)>> {
        let png = if interpreted {
            self.page_rasterized_png_interpreted_checked(page_index, dpi)?
        } else {
            self.page_rasterized_png(page_index, dpi)?
        };
        let Some(png) = png else {
            return Ok(None);
        };
        let img = image::load_from_memory(&png)
            .map_err(|e| anyhow!("raster decode: {e}"))?
            .to_luma8();
        let (w, h) = (img.width() as usize, img.height() as usize);
        let scale = dpi / 72.0;
        let min_px = (1.2 * scale).round().max(2.0) as usize;
        let max_px = (30.0 * scale).round() as usize;
        let raw = raster_glyphs::glyphs_from_luma(w, h, img.as_raw(), threshold, k, min_px, max_px);
        // pixel → page units
        let glyphs: Vec<GlyphGeom> = raw
            .into_iter()
            .map(|(x, y, s, d)| (x / scale, y / scale, s / scale, d))
            .collect();
        let items: Vec<(f32, f32, u32)> = glyphs
            .iter()
            .enumerate()
            .map(|(i, g)| (g.0, g.1, i as u32))
            .collect();
        // DENSITY-ADAPTIVE banding. The two regimes overlap geometrically —
        // intra-line ink-top spread (~2-3pt) matches dense-table row gaps —
        // but density separates them cleanly (measured: clean text ~2.6-3.9
        // glyphs per pt of y-range, dense price tables ~18/pt). Sparse pages
        // keep the proven gap-chaining; dense pages use fine projection
        // valleys, which chaining collapses to a single band.
        let y_lo = items.iter().map(|i| i.1).fold(f32::MAX, f32::min);
        let y_hi = items.iter().map(|i| i.1).fold(f32::MIN, f32::max);
        let density = items.len() as f32 / (y_hi - y_lo).max(1.0);
        let banded = if density > 7.0 {
            band_by_projection(items, (line_tolerance * 0.2).max(0.3), split_columns)
        } else {
            band_text_lines_public(items, line_tolerance, split_columns)
        };
        let lines = banded
            .into_iter()
            .map(|(bbox, members)| (bbox, members.into_iter().map(|(_, i)| i).collect()))
            .collect();
        Ok(Some((glyphs, lines)))
    }

    /// Visual text lines with per-glyph cluster membership — the substrate
    /// for self-labelled glyph decoding. For each page: `(crop_box,
    /// [(x, cluster_id), ...])` per line, glyphs sorted by x. The crop box and
    /// the membership come from the SAME banding pass (`band_text_lines`),
    /// so a recognised crop and its glyph sequence can never disagree about
    /// which glyphs are on the line.
    pub fn vector_glyph_cluster_lines(
        &self,
        eps: f32,
        line_tolerance: f32,
        split_columns: bool,
    ) -> Vec<Vec<TextLine<u32>>> {
        self.vector_glyph_cluster_lines_checked(eps, line_tolerance, split_columns)
            .unwrap_or_default()
    }

    /// Checked cluster and line-banding surface. Governed extraction failures
    /// abort instead of becoming an empty document.
    pub fn vector_glyph_cluster_lines_checked(
        &self,
        eps: f32,
        line_tolerance: f32,
        split_columns: bool,
    ) -> Result<Vec<Vec<TextLine<u32>>>> {
        Ok(self
            .vector_glyph_clusters_checked(eps)?
            .into_iter()
            .map(|page| band_text_lines_public(page, line_tolerance, split_columns))
            .collect())
    }

    /// Extract text from `page_index` using the glyph-shape catalog, bypassing
    /// the CMap/ToUnicode path entirely.  Useful as a fallback when
    /// `semantic_text_runs_for_page` returns a high `cmap_fallback_chars`
    /// ratio, indicating the font uses a custom encoding without a proper
    /// ToUnicode entry.  Returns `None` if the page has no vector glyph runs
    /// or if no glyph in the runs is known to the catalog.
    pub fn page_vector_text_as_string(&self, page_index: usize) -> Option<String> {
        self.page_vector_text_as_string_checked(page_index)
            .ok()
            .flatten()
    }

    /// Checked vector-text rendering. Governed extraction failures are
    /// returned instead of being indistinguishable from an empty page.
    pub fn page_vector_text_as_string_checked(&self, page_index: usize) -> Result<Option<String>> {
        let runs = self.page_vector_text_runs(page_index)?;
        if runs.is_empty() {
            return Ok(None);
        }
        let text = vector_text::runs_to_string(&runs);
        // Only return if at least some glyphs were resolved by the catalog.
        if text.trim().is_empty() || text.chars().all(|c| c == '_' || c == '\n' || c == ' ') {
            return Ok(None);
        }
        Ok(Some(text))
    }

    pub(crate) fn page_content_streams_for_raster(&self, page: &PdfPage) -> Result<Vec<Vec<u8>>> {
        let streams = page
            .content_streams
            .iter()
            .map(|stream_index| self.materialize_stream_bytes_for_raster(*stream_index))
            .collect::<Result<Vec<_>>>()?;
        Ok(streams.into_iter().flatten().collect())
    }

    fn materialize_stream_bytes_for_raster(&self, stream_index: usize) -> Result<Option<Vec<u8>>> {
        let Some(stream) = self.streams.get(stream_index) else {
            return Ok(None);
        };
        if let Some(decoded) = self.stream_decoded_bytes(stream_index) {
            self.decoded_byte_budget
                .charge_resource_bytes(decoded.len())?;
            return Ok(Some(decoded.to_vec()));
        }
        let Some(raw) = self.stream_raw_bytes(stream_index) else {
            return Ok(None);
        };
        if stream.filters.is_empty() || matches!(stream.decode_status, DecodeStatus::Raw) {
            self.decoded_byte_budget.charge_resource_bytes(raw.len())?;
            return Ok(Some(raw.to_vec()));
        }
        let decode_parms = self
            .object_by_id(stream.object_number, stream.generation)
            .and_then(|object| object.dictionary.as_ref())
            .and_then(|dict| dict.get("DecodeParms").or_else(|| dict.get("DP")));
        match self
            .decoded_byte_budget
            .decode_filters(raw, &stream.filters, decode_parms)
        {
            Ok(decoded) => Ok(Some(decoded)),
            Err(error) if is_decode_budget_exceeded(&error) => Err(error),
            Err(_) => Ok(None),
        }
    }

    fn pages_from_catalog(&self) -> Option<Vec<PdfPage>> {
        // The document is the one the newest trailer's `/Root` names. A
        // file saved with incremental updates holds a catalog per revision,
        // and the first in the file is the oldest. Without a usable
        // cross-reference chain, the catalog latest in the file is the best
        // guess for the same reason.
        let catalog = self
            .catalog
            .and_then(|(number, generation)| self.object_by_id(number, generation))
            .or_else(|| {
                // Latest in the file, by where each object's bytes are. An
                // object unpacked from an object stream has its container's
                // offset; its place in `objects` says nothing about order.
                self.objects
                    .iter()
                    .filter(|object| {
                        object
                            .dictionary
                            .as_ref()
                            .and_then(|dict| dict.get_name("Type"))
                            == Some("Catalog")
                    })
                    .max_by_key(|object| object.span.start)
            })?;
        let pages_ref = match catalog.dictionary.as_ref()?.get("Pages")? {
            PdfValue::Reference {
                object_number,
                generation,
            } => (*object_number, *generation),
            _ => return None,
        };
        let mut pages = Vec::new();
        self.collect_pages_from_node(pages_ref, &mut pages);
        if pages.is_empty() {
            None
        } else {
            Some(pages)
        }
    }

    /// Collect the leaf pages under `root`, in document order.
    ///
    /// The walk keeps its own stack and visits each node once. A page tree is
    /// a tree, but a malformed or hostile file can make `/Kids` point back at
    /// an ancestor, chain nodes thousands deep, or share one subtree between
    /// many parents; following those by recursion overflows the stack, and
    /// following shared subtrees repeatedly multiplies a kilobyte of input
    /// into billions of pages. Visiting each node once bounds the work by the
    /// number of objects in the file.
    fn collect_pages_from_node(&self, root: (u32, u16), pages: &mut Vec<PdfPage>) {
        let mut visited: HashSet<(u32, u16)> = HashSet::new();
        let mut pending: Vec<PendingPageNode> = vec![(root, None)];
        while let Some((node_ref, inherited_resources)) = pending.pop() {
            if !visited.insert(node_ref) {
                continue;
            }
            let Some(object) = self.object_by_id(node_ref.0, node_ref.1) else {
                continue;
            };
            let Some(dictionary) = object.dictionary.as_ref() else {
                continue;
            };

            let resources = dictionary
                .get("Resources")
                .and_then(|value| self.resolve_dictionary(value))
                .map(|resources| Rc::new(resources.clone()))
                .or(inherited_resources);

            match dictionary.get_name("Type") {
                Some("Page") => {
                    let page_index = pages.len();
                    let resources = resources.map(|resources| {
                        Rc::try_unwrap(resources).unwrap_or_else(|shared| (*shared).clone())
                    });
                    pages.push(self.page_from_object(object, page_index, resources));
                }
                Some("Pages") | None => {
                    if let Some(PdfValue::Array(kids)) = dictionary.get("Kids") {
                        // Reversed so the first kid is popped, and so visited,
                        // first: pages come out in document order.
                        for kid in kids.iter().rev() {
                            if let PdfValue::Reference {
                                object_number,
                                generation,
                            } = kid
                            {
                                pending.push(((*object_number, *generation), resources.clone()));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn page_from_object(
        &self,
        object: &PdfObject,
        page_index: usize,
        resources: Option<PdfDictionary>,
    ) -> PdfPage {
        let mut content_streams = Vec::new();
        let mut image_streams = Vec::new();

        if let Some(dictionary) = object.dictionary.as_ref() {
            if let Some(contents) = dictionary.get("Contents") {
                for (object_number, generation) in collect_references(contents) {
                    if let Some(target) = self.object_by_id(object_number, generation) {
                        if let Some(stream_index) = target.stream_index {
                            content_streams.push(stream_index);
                        }
                    }
                }
            }

            if let Some(resources) = resources
                .as_ref()
                .or_else(|| self.resolve_inherited_resources(dictionary))
            {
                if let Some(xobject_value) = resources.get("XObject") {
                    if let Some(xobjects) = self.resolve_dictionary(xobject_value) {
                        for entry in &xobjects.entries {
                            for (object_number, generation) in collect_references(&entry.value) {
                                if let Some(target) = self.object_by_id(object_number, generation) {
                                    if let Some(stream_index) = target.stream_index {
                                        let kind = self
                                            .streams
                                            .get(stream_index)
                                            .map(|stream| stream.kind)
                                            .unwrap_or(PdfStreamKind::Unknown);

                                        if kind == PdfStreamKind::Image {
                                            image_streams.push(stream_index);
                                        } else {
                                            // Assume it's a Form XObject. Its contents might contain text operators!
                                            content_streams.push(stream_index);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        content_streams.sort_unstable();
        content_streams.dedup();
        image_streams.sort_unstable();
        image_streams.dedup();

        PdfPage {
            page_index,
            object_number: object.object_number,
            generation: object.generation,
            content_streams,
            image_streams,
            crop_box: object
                .dictionary
                .as_ref()
                .and_then(|dictionary| self.resolve_inherited_crop_box(dictionary)),
            media_box: object
                .dictionary
                .as_ref()
                .and_then(|dictionary| self.resolve_inherited_media_box(dictionary)),
            rotate: object
                .dictionary
                .as_ref()
                .and_then(|dictionary| self.resolve_inherited_rotate(dictionary))
                .and_then(|raw| normalize_rotate(raw).ok())
                .unwrap_or(0),
            rotate_raw: object
                .dictionary
                .as_ref()
                .and_then(|dictionary| self.resolve_inherited_rotate(dictionary)),
            resources,
        }
    }

    fn resolve_inherited_resources<'a>(
        &'a self,
        dictionary: &'a PdfDictionary,
    ) -> Option<&'a PdfDictionary> {
        let mut current = dictionary;
        let mut depth = 0;
        loop {
            if depth > 128 {
                return None;
            }
            depth += 1;

            if let Some(resources) = current
                .get("Resources")
                .and_then(|value| self.resolve_dictionary(value))
            {
                return Some(resources);
            }
            let PdfValue::Reference {
                object_number,
                generation,
            } = current.get("Parent")?
            else {
                return None;
            };
            current = self
                .object_by_id(*object_number, *generation)?
                .dictionary
                .as_ref()?;
        }
    }

    fn resolve_inherited_media_box(&self, dictionary: &PdfDictionary) -> Option<[f32; 4]> {
        let mut current = Some(dictionary);
        let mut depth = 0;
        while let Some(dict) = current {
            if depth > 128 {
                return None;
            }
            depth += 1;

            if let Some(media_box) = dict.get("MediaBox").and_then(pdf_value_rect) {
                return Some(media_box);
            }
            current = match dict.get("Parent") {
                Some(PdfValue::Reference {
                    object_number,
                    generation,
                }) => self
                    .object_by_id(*object_number, *generation)
                    .and_then(|object| object.dictionary.as_ref()),
                _ => None,
            };
        }
        None
    }

    fn resolve_inherited_crop_box(&self, dictionary: &PdfDictionary) -> Option<[f32; 4]> {
        let mut current = Some(dictionary);
        let mut depth = 0;
        while let Some(dict) = current {
            if depth > 128 {
                return None;
            }
            depth += 1;

            if let Some(crop_box) = dict.get("CropBox").and_then(pdf_value_rect) {
                return Some(crop_box);
            }
            current = match dict.get("Parent") {
                Some(PdfValue::Reference {
                    object_number,
                    generation,
                }) => self
                    .object_by_id(*object_number, *generation)
                    .and_then(|object| object.dictionary.as_ref()),
                _ => None,
            };
        }
        None
    }

    /// Resolve the inherited raw `/Rotate` value, unnormalized.
    ///
    /// Kept raw so the renderer can refuse a value the format does not allow.
    /// Normalizing here would have to either round it (a page silently rotated
    /// the wrong way) or propagate an error through the recursive page walk,
    /// which drops a page it cannot read — worse than the bug. PDF 32000-1
    /// §7.7.3.3: `/Rotate` "shall be a multiple of 90".
    fn resolve_inherited_rotate(&self, dictionary: &PdfDictionary) -> Option<i64> {
        let mut current: Option<&PdfDictionary> = Some(dictionary);
        let mut depth = 0;
        while let Some(dict) = current {
            if depth > 128 {
                return None;
            }
            depth += 1;
            if let Some(rotate) = dict.get("Rotate").and_then(|v| match v {
                PdfValue::Integer(n) => Some(*n),
                _ => None,
            }) {
                return Some(rotate);
            }
            current = match dict.get("Parent") {
                Some(PdfValue::Reference {
                    object_number,
                    generation,
                }) => self
                    .object_by_id(*object_number, *generation)
                    .and_then(|object| object.dictionary.as_ref()),
                _ => None,
            };
        }
        None
    }

    fn populate_text_fragments(&mut self) {
        let mut fallback_bindings = HashMap::new();
        for object in &self.objects {
            let Some(dict) = object.dictionary.as_ref() else {
                continue;
            };
            if dict.get("Font").is_some() {
                fallback_bindings.extend(self.resolve_font_bindings(dict));
            }
            if let Some(resources) = dict
                .get("Resources")
                .and_then(|value| self.resolve_dictionary(value))
            {
                fallback_bindings.extend(self.resolve_font_bindings(resources));
            }
        }

        let mut stream_bindings: HashMap<usize, HashMap<String, FontBinding>> = HashMap::new();
        for page in self.pages() {
            let Some(object) = self.object_by_id(page.object_number, page.generation) else {
                continue;
            };
            let Some(dictionary) = object.dictionary.as_ref() else {
                continue;
            };
            let resources = page
                .resources
                .as_ref()
                .or_else(|| self.resolve_inherited_resources(dictionary));
            let Some(resources) = resources else {
                continue;
            };
            let page_bindings = self.resolve_font_bindings(resources);
            if page_bindings.is_empty() {
                continue;
            }
            for &stream_index in &page.content_streams {
                stream_bindings
                    .entry(stream_index)
                    .or_default()
                    .extend(page_bindings.clone());
            }
        }

        for (stream_index, stream) in self.streams.iter().enumerate() {
            if let Some(dict) = self
                .object_by_id(stream.object_number, stream.generation)
                .and_then(|object| object.dictionary.as_ref())
            {
                if let Some(resources) = dict
                    .get("Resources")
                    .and_then(|value| self.resolve_dictionary(value))
                {
                    let own_bindings = self.resolve_font_bindings(resources);
                    if !own_bindings.is_empty() {
                        stream_bindings
                            .entry(stream_index)
                            .or_default()
                            .extend(own_bindings);
                    }
                }
            }
        }

        let mut fragments = Vec::new();
        for (stream_index, stream) in self.streams.iter().enumerate() {
            if stream.kind != PdfStreamKind::Content {
                continue;
            }
            let decoded_bytes = self.stream_decoded_bytes(stream_index).or_else(|| {
                if stream.filters.is_empty() || matches!(stream.decode_status, DecodeStatus::Raw) {
                    self.stream_raw_bytes(stream_index)
                } else {
                    None
                }
            });
            let Some(decoded_bytes) = decoded_bytes else {
                continue;
            };
            if let Some(specific) = stream_bindings.get(&stream_index) {
                let mut combined = fallback_bindings.clone();
                combined.extend(specific.clone());
                fragments.extend(extract_text_fragments(
                    decoded_bytes,
                    stream.object_number,
                    stream.generation,
                    stream_index,
                    &combined,
                ));
            } else {
                fragments.extend(extract_text_fragments(
                    decoded_bytes,
                    stream.object_number,
                    stream.generation,
                    stream_index,
                    &fallback_bindings,
                ));
            }
        }
        self.text_fragments = fragments;
    }

    fn resolve_font_bindings(&self, resources: &PdfDictionary) -> HashMap<String, FontBinding> {
        let Some(font_value) = resources.get("Font") else {
            return HashMap::new();
        };
        let Some(fonts) = self.resolve_dictionary(font_value) else {
            return HashMap::new();
        };

        let mut bindings = HashMap::new();
        for entry in &fonts.entries {
            let Some((object_number, generation)) =
                collect_references(&entry.value).into_iter().next()
            else {
                continue;
            };
            let Some(font_object) = self.object_by_id(object_number, generation) else {
                continue;
            };
            let Some(font_dictionary) = font_object.dictionary.as_ref() else {
                continue;
            };
            let to_unicode = font_dictionary
                .get("ToUnicode")
                .and_then(|value| collect_references(value).into_iter().next())
                .and_then(|(object_number, generation)| {
                    self.cmaps
                        .iter()
                        .find(|cmap| {
                            cmap.object_number == object_number && cmap.generation == generation
                        })
                        .map(UnicodeMap::from_cmap)
                });

            let subtype = font_dictionary.get_name("Subtype").map(str::to_string);
            let base_font = font_dictionary.get_name("BaseFont").map(str::to_string);
            let (encoding, encoding_source) =
                self.resolve_font_encoding(font_dictionary, base_font.as_deref());

            bindings.insert(
                entry.key.clone(),
                FontBinding {
                    resource_name: entry.key.clone(),
                    object_number,
                    generation,
                    subtype,
                    to_unicode,
                    encoding,
                    encoding_source,
                },
            );
        }
        bindings
    }

    /// Build the encoding map for a font dictionary by resolving its
    /// `/Encoding` entry (named or indirect with `/Differences`) and applying
    /// implicit Standard-14 fallbacks when the entry is missing.
    fn resolve_font_encoding(
        &self,
        font_dictionary: &PdfDictionary,
        base_font: Option<&str>,
    ) -> (Option<crate::encoding::EncodingMap>, EncodingSource) {
        use crate::encoding::{BaseEncoding, EncodingMap};

        match font_dictionary.get("Encoding") {
            Some(PdfValue::Name(name)) => {
                if let Some(base) = BaseEncoding::from_name(name) {
                    return (Some(EncodingMap::new(base)), EncodingSource::Named);
                }
            }
            Some(value) => {
                let dict = self.resolve_dictionary(value);
                if let Some(dict) = dict {
                    let base = dict
                        .get_name("BaseEncoding")
                        .and_then(BaseEncoding::from_name)
                        .unwrap_or(BaseEncoding::Standard);
                    let mut map = EncodingMap::new(base);
                    if let Some(diffs_value) = dict.get("Differences") {
                        let diffs = crate::encoding::parse_differences(diffs_value);
                        if !diffs.is_empty() {
                            map.apply_differences(&diffs);
                            return (Some(map), EncodingSource::Differences);
                        }
                    }
                    // Encoding dict without /Differences — still useful as a
                    // base-name override.
                    return (Some(map), EncodingSource::Named);
                }
            }
            None => {}
        }

        // No /Encoding — Standard 14 fonts default to StandardEncoding.
        if let Some(base_font) = base_font {
            if let Some(base) = standard14_implicit_encoding(base_font) {
                return (
                    Some(EncodingMap::new(base)),
                    EncodingSource::StandardImplicit,
                );
            }
        }
        (None, EncodingSource::None)
    }

    fn extract_page_image_payload(&self, page: &PdfPage) -> Result<PageImagePayload> {
        let mut candidates = page
            .image_streams
            .iter()
            .filter_map(|stream_index| {
                self.streams
                    .get(*stream_index)
                    .map(|stream| (*stream_index, stream))
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|(_, stream)| std::cmp::Reverse(stream.raw_len));

        for (stream_index, stream) in candidates {
            let raw = match self.stream_raw_bytes(stream_index) {
                Some(raw) => raw,
                None => continue,
            };
            #[cfg(feature = "rasterize")]
            if stream
                .filters
                .iter()
                .any(|filter| is_ccitt_filter(filter.as_str()))
            {
                if let Some(image) = stream.image.as_ref() {
                    match decode_ccitt_image_payload(
                        raw,
                        &stream.filters,
                        image,
                        &self.decoded_byte_budget,
                    ) {
                        Ok(png) => return Ok((Some(png), Some("png".to_string()))),
                        Err(error) if is_decode_budget_exceeded(&error) => return Err(error),
                        Err(_) => {}
                    }
                }
            }
            if stream.filters.iter().all(|filter| {
                matches!(
                    filter.as_str(),
                    "DCTDecode" | "DCT" | "JPXDecode" | "JBIG2Decode"
                )
            }) {
                return Ok((
                    Some(raw.to_vec()),
                    stream
                        .image
                        .as_ref()
                        .and_then(|image| image.format_hint.clone())
                        .or_else(|| stream.filters.first().cloned()),
                ));
            }
            if stream
                .filters
                .iter()
                .any(|filter| matches!(filter.as_str(), "FlateDecode" | "Fl"))
            {
                let decode_parms = self
                    .object_by_id(stream.object_number, stream.generation)
                    .and_then(|object| object.dictionary.as_ref())
                    .and_then(|dict| dict.get("DecodeParms").or_else(|| dict.get("DP")));
                match self
                    .decoded_byte_budget
                    .decode_filters(raw, &stream.filters, decode_parms)
                {
                    Ok(decoded) => return Ok((Some(decoded), Some("raw".to_string()))),
                    Err(error) if is_decode_budget_exceeded(&error) => return Err(error),
                    Err(_) => {}
                }
            }
        }

        Ok((None, None))
    }

    #[cfg(feature = "rasterize")]
    fn extract_page_ccitt_luma_image(
        &self,
        page: &PdfPage,
    ) -> Result<Option<ccitt::CcittLumaImage>> {
        let mut candidates = page
            .image_streams
            .iter()
            .filter_map(|stream_index| {
                self.streams
                    .get(*stream_index)
                    .map(|stream| (*stream_index, stream))
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|(_, stream)| std::cmp::Reverse(stream.raw_len));

        for (stream_index, stream) in candidates {
            if !stream
                .filters
                .iter()
                .any(|filter| is_ccitt_filter(filter.as_str()))
            {
                continue;
            }
            let Some(image) = stream.image.as_ref() else {
                continue;
            };
            let Some(raw) = self.stream_raw_bytes(stream_index) else {
                continue;
            };
            match decode_ccitt_luma_payload(raw, &stream.filters, image, &self.decoded_byte_budget)
            {
                Ok(luma) => return Ok(Some(luma)),
                Err(error) if is_decode_budget_exceeded(&error) => return Err(error),
                Err(_) => {}
            }
        }

        Ok(None)
    }

    /// Resolve a resource name to a Type 3 font, or `None` for anything else.
    ///
    /// Type 3 is the one font subtype whose glyphs are content streams
    /// (`/CharProcs`), which makes its outlines reachable by a parser that can
    /// execute those streams. Every other subtype draws with a font program this
    /// crate does not interpret, so it returns `None` rather than carrying state
    /// it cannot use.
    ///
    /// Lives here rather than in `vector_text` because resolving dictionaries
    /// and materializing streams is this type's business; the extractor only
    /// consumes the result.
    /// Resolve a resource name to a font whose outlines this parser can reach.
    ///
    /// Two subtypes qualify, for different reasons. A Type 3 font *is* its
    /// outlines — each glyph is a content stream — so reaching them means
    /// executing them. An embedded TrueType program carries its outlines in
    /// `glyf`, so reaching them means reading that table. Every other subtype
    /// keeps its outlines in a form this crate does not read, and returning
    /// `None` is the honest answer: a plausible wrong shape is exactly what the
    /// catalog would then compare against.
    fn resolve_drawable_font(
        &self,
        resources: Option<&PdfDictionary>,
        name: &str,
    ) -> Option<crate::vector_text::DrawableFont> {
        let fonts = resources
            .and_then(|resources| resources.get("Font"))
            .and_then(|value| self.resolve_dictionary(value))?;
        let font = fonts
            .get(name)
            .and_then(|value| self.resolve_dictionary(value))?;
        match font.get_name("Subtype") {
            Some("Type3") => self
                .resolve_type3_font(font)
                .map(crate::vector_text::DrawableFont::Type3),
            // A TrueType subtype may still carry a bare CFF, and an OpenType
            // subtype names CFF explicitly; both routes end at the same reader.
            Some("TrueType") => self.resolve_true_type_font(font),
            Some("Type1") | Some("CIDFontType0") | Some("CIDFontType2") => {
                self.resolve_cff_font(font)
            }
            _ => None,
        }
    }

    /// The embedded TrueType program for a font dictionary, if it has one.
    ///
    /// `/FontDescriptor` may sit on the font or on a CIDFont descendant, which
    /// is why both are tried: a subset font is normally a Type0 whose program
    /// lives on the descendant, and those are the fonts real documents embed.
    fn resolve_true_type_font(
        &self,
        font: &PdfDictionary,
    ) -> Option<crate::vector_text::DrawableFont> {
        let descriptor = self.font_descriptor(font)?;
        let program = descriptor
            .get("FontFile2")
            .and_then(|value| self.resolve_stream_value_bytes(value))?;
        if let Some(parsed) = crate::truetype::TrueTypeFont::parse(&program) {
            return Some(crate::vector_text::DrawableFont::TrueType(parsed));
        }
        // A `/FontFile2` is normally a `glyf` program, but a producer may put a
        // bare CFF there; the sfnt header is what decides, and the glyph reader
        // refuses anything it cannot interpret.
        let encoding = self
            .resolve_font_encoding(font, font.get_name("BaseFont"))
            .0;
        crate::cff::CffFont::parse(&program, None)
            .map(|program| crate::vector_text::DrawableFont::Cff { program, encoding })
    }

    /// The embedded CFF program for a font dictionary, if it has one.
    ///
    /// `/FontFile3` carries CFF directly for a bare-CFF OpenType font, and an
    /// `OTTO` sfnt wraps it in a table. Both are read, because which one a
    /// producer writes depends on the tool rather than on the format.
    fn resolve_cff_font(&self, font: &PdfDictionary) -> Option<crate::vector_text::DrawableFont> {
        let descriptor = self.font_descriptor(font)?;
        let program = descriptor
            .get("FontFile3")
            .and_then(|value| self.resolve_stream_value_bytes(value))?;
        // An `OTTO` sfnt keeps its outlines in a `CFF ` table, and the em size
        // comes from `head` the same way a `glyf` program's does.
        if program.len() > 12 && &program[0..4] == b"OTTO" {
            let tables = u16::from_be_bytes([program[4], program[5]]) as usize;
            let mut cff = None;
            let mut units_per_em = None;
            for index in 0..tables {
                let record = 12 + index * 16;
                if record + 16 > program.len() {
                    break;
                }
                let offset = u32::from_be_bytes([
                    program[record + 8],
                    program[record + 9],
                    program[record + 10],
                    program[record + 11],
                ]) as usize;
                let length = u32::from_be_bytes([
                    program[record + 12],
                    program[record + 13],
                    program[record + 14],
                    program[record + 15],
                ]) as usize;
                let end = offset.saturating_add(length).min(program.len());
                if offset >= program.len() {
                    continue;
                }
                match &program[record..record + 4] {
                    b"CFF " => cff = Some(program[offset..end].to_vec()),
                    b"head" if offset + 20 <= program.len() => {
                        units_per_em =
                            Some(
                                u16::from_be_bytes([program[offset + 18], program[offset + 19]])
                                    as f32,
                            );
                    }
                    _ => {}
                }
            }
            let cff = cff?;
            let encoding = self
                .resolve_font_encoding(font, font.get_name("BaseFont"))
                .0;
            return crate::cff::CffFont::parse(&cff, units_per_em)
                .map(|program| crate::vector_text::DrawableFont::Cff { program, encoding });
        }
        let encoding = self
            .resolve_font_encoding(font, font.get_name("BaseFont"))
            .0;
        crate::cff::CffFont::parse(&program, None)
            .map(|program| crate::vector_text::DrawableFont::Cff { program, encoding })
    }

    /// The `/FontDescriptor` for a font, following `/DescendantFonts` once.
    fn font_descriptor<'a>(&'a self, font: &'a PdfDictionary) -> Option<&'a PdfDictionary> {
        if let Some(descriptor) = font
            .get("FontDescriptor")
            .and_then(|value| self.resolve_dictionary(value))
        {
            return Some(descriptor);
        }
        font.get("DescendantFonts")
            .and_then(|value| self.resolve_array(value))?
            .first()
            .and_then(|value| self.resolve_dictionary(value))?
            .get("FontDescriptor")
            .and_then(|value| self.resolve_dictionary(value))
    }

    /// Decoded bytes for an indirect stream value.
    fn resolve_stream_value_bytes(&self, value: &PdfValue) -> Option<Vec<u8>> {
        let (object_number, generation) = match value {
            PdfValue::Reference {
                object_number,
                generation,
            } => (*object_number, *generation),
            _ => return None,
        };
        let object = self.object_by_id(object_number, generation)?;
        let stream_index = object.stream_index?;
        self.materialize_stream_bytes_for_raster(stream_index)
            .ok()
            .flatten()
    }

    /// A Type 3 font's glyph programs, encoding, and widths.
    fn resolve_type3_font(&self, font: &PdfDictionary) -> Option<crate::vector_text::Type3Font> {
        let font_matrix = font
            .get("FontMatrix")
            .and_then(|value| self.resolve_array(value))
            .filter(|values| values.len() == 6)
            .map(|values| {
                let mut matrix = crate::vector_text::IDENTITY;
                for (slot, value) in matrix.iter_mut().zip(values) {
                    if let Some(number) = pdf_value_as_f32(value) {
                        *slot = number;
                    }
                }
                matrix
            })
            // A Type 3 font without a usable font matrix is malformed. The
            // 1000-unit em default is what the spec's own example uses, and
            // falling back to the identity would draw glyphs three orders of
            // magnitude too large — a visible wrong answer rather than a
            // contained one.
            .unwrap_or([0.001, 0.0, 0.0, 0.001, 0.0, 0.0]);

        let mut differences = HashMap::new();
        if let Some(array) = font
            .get("Encoding")
            .and_then(|value| self.resolve_dictionary(value))
            .and_then(|encoding| encoding.get("Differences"))
            .and_then(|value| self.resolve_array(value))
        {
            let mut code: u32 = 0;
            for value in array {
                match value {
                    PdfValue::Integer(n) => code = (*n).clamp(0, 255) as u32,
                    PdfValue::Name(glyph) => {
                        if code <= 255 {
                            differences.insert(code as u8, glyph.clone());
                        }
                        code += 1;
                    }
                    _ => {}
                }
            }
        }

        let mut char_procs = HashMap::new();
        if let Some(procs) = font
            .get("CharProcs")
            .and_then(|value| self.resolve_dictionary(value))
        {
            for entry in &procs.entries {
                if let Some(bytes) = self.resolve_stream_value_bytes(&entry.value) {
                    char_procs.insert(entry.key.clone(), bytes);
                }
            }
        }

        let first_char = font.get_integer("FirstChar").unwrap_or(0).clamp(0, 255) as u8;
        let widths: Vec<f32> = font
            .get("Widths")
            .and_then(|value| self.resolve_array(value))
            .map(|values| values.iter().filter_map(pdf_value_as_f32).collect())
            .unwrap_or_default();

        Some(crate::vector_text::Type3Font {
            differences,
            char_procs,
            font_matrix,
            first_char,
            widths,
        })
    }

    /// Resolve an array value, following one indirect reference hop.
    ///
    /// Mirrors [`Self::resolve_dictionary`]. An array is commonly written as an
    /// indirect object when it is long or shared — `/Widths` across subset fonts
    /// especially — so following the reference is not an edge case for the
    /// fonts this parser reads.
    fn resolve_array<'a>(&'a self, value: &'a PdfValue) -> Option<&'a Vec<PdfValue>> {
        match value {
            PdfValue::Array(values) => Some(values),
            PdfValue::Reference {
                object_number,
                generation,
            } => self
                .object_by_id(*object_number, *generation)?
                .array
                .as_ref(),
            _ => None,
        }
    }

    fn resolve_form_xobject(
        &self,
        resources: Option<&PdfDictionary>,
        name: &str,
        traversal_stack: &[(u32, u16)],
    ) -> Result<Option<FormXObject>> {
        let Some(resources) = resources else {
            return Ok(None);
        };
        let Some(xobject_value) = resources.get("XObject") else {
            return Ok(None);
        };
        let Some(xobjects) = self.resolve_dictionary(xobject_value) else {
            return Ok(None);
        };
        let Some(target) = xobjects.entries.iter().find(|entry| entry.key == name) else {
            return Ok(None);
        };
        let Some((object_number, generation)) =
            collect_references(&target.value).into_iter().next()
        else {
            return Ok(None);
        };
        let Some(object) = self.object_by_id(object_number, generation) else {
            return Ok(None);
        };
        let Some(dictionary) = object.dictionary.as_ref() else {
            return Ok(None);
        };
        if dictionary.get_name("Subtype") != Some("Form") {
            return Ok(None);
        }
        ensure_form_xobject_traversal(traversal_stack, (object_number, generation))?;
        let Some(stream_index) = object.stream_index else {
            return Ok(None);
        };
        let Some(bytes) = self.materialize_stream_bytes_for_raster(stream_index)? else {
            return Ok(None);
        };
        let nested_resources = dictionary
            .get("Resources")
            .and_then(|value| self.resolve_dictionary(value))
            .cloned()
            .or_else(|| Some(resources.clone()));
        let matrix = dictionary
            .get("Matrix")
            .and_then(pdf_value_matrix)
            .unwrap_or(IDENTITY_MATRIX);
        Ok(Some(FormXObject {
            object_id: (object_number, generation),
            bytes,
            resources: nested_resources,
            matrix,
        }))
    }

    fn resolve_image_xobject(&self, resources: Option<&PdfDictionary>, name: &str) -> Option<u32> {
        let resources = resources?;
        let xobjects = self.resolve_dictionary(resources.get("XObject")?)?;
        let target = xobjects.entries.iter().find(|entry| entry.key == name)?;
        let (object_number, generation) = collect_references(&target.value).into_iter().next()?;
        let object = self.object_by_id(object_number, generation)?;
        let dictionary = object.dictionary.as_ref()?;
        if dictionary.get_name("Subtype") != Some("Image") {
            return None;
        }
        Some(object_number)
    }
}

/// Low-level PDF object recovered from byte scanning.
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct PdfObject {
    /// Indirect object number.
    pub object_number: u32,
    /// Indirect object generation (almost always 0).
    pub generation: u16,
    /// Byte range covering the full `N G obj … endobj` declaration.
    pub span: ByteRange,
    /// Byte range of the object's dictionary, if any.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub dictionary_range: Option<ByteRange>,
    /// Parsed dictionary, when one was recovered.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub dictionary: Option<PdfDictionary>,
    /// Parsed array, when the object is one.
    ///
    /// Only dictionaries used to be kept, so an array written as an indirect
    /// object — `/Widths 12 0 R`, which a producer emitting many subset fonts
    /// will do — was unreachable: `resolve_array` could follow the reference and
    /// find nothing. The value parser has always handled arrays; the result was
    /// dropped one line after being produced.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub array: Option<Vec<PdfValue>>,
    /// Index into [`PdfDocument::streams`] when this object hosts a stream.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub stream_index: Option<usize>,
}

/// Page-level grouping derived from `/Type /Page`, `/Contents`, and `/Resources`.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct PdfPage {
    /// 0-based index of the page in the document.
    pub page_index: usize,
    /// PDF indirect object number for the page.
    pub object_number: u32,
    /// PDF indirect object generation.
    pub generation: u16,
    /// Indices into [`PdfDocument::streams`] for the page's content streams.
    pub content_streams: Vec<usize>,
    /// Indices into [`PdfDocument::streams`] for image XObjects referenced by the page.
    pub image_streams: Vec<usize>,
    /// `/CropBox` rectangle in PDF user space, when present.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub crop_box: Option<[f32; 4]>,
    /// `/MediaBox` rectangle in PDF user space, when present.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub media_box: Option<[f32; 4]>,
    /// `/Resources` dictionary for the page, when present.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub resources: Option<PdfDictionary>,
    /// Page rotation in degrees from the PDF /Rotate key (0, 90, 180, 270).
    /// Inherited from parent page tree nodes if not set on the page itself.
    ///
    /// This is the **normalized** rotation: a multiple of 90, with out-of-range
    /// multiples folded (`-90` → `270`, `450` → `90`). The raw value the file
    /// declared is kept in [`Self::rotate_raw`] so a renderer can refuse one
    /// that is not a multiple of 90 rather than picking a rounding.
    pub rotate: u32,
    /// The `/Rotate` value exactly as the file declared it, before folding.
    ///
    /// `None` when the page tree declares none. Equal to `rotate` for every
    /// well-formed file; differs when the file is malformed, which is the case
    /// [`PdfDocument::page_rasterized_png`] reports.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub rotate_raw: Option<i64>,
}

impl PdfPage {
    /// Effective page rectangle: prefers `/CropBox`, falls back to `/MediaBox`.
    pub fn page_box(&self) -> Option<[f32; 4]> {
        self.crop_box.or(self.media_box)
    }
}

/// Shallow PDF dictionary.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct PdfDictionary {
    /// Key/value pairs in declaration order.
    pub entries: Vec<PdfDictionaryEntry>,
}

impl PdfDictionary {
    /// Look up a key; returns `None` if absent.
    pub fn get(&self, key: &str) -> Option<&PdfValue> {
        self.entries
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| &entry.value)
    }

    /// Look up a key and return its value as a name, or `None` if absent or
    /// not a name.
    pub fn get_name(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            PdfValue::Name(value) => Some(value.as_str()),
            _ => None,
        }
    }

    /// Look up a key and return its value as an integer, or `None` if absent
    /// or not an integer.
    pub fn get_integer(&self, key: &str) -> Option<i64> {
        match self.get(key)? {
            PdfValue::Integer(value) => Some(*value),
            _ => None,
        }
    }

    /// Look up a key and return its value as a boolean, or `None` if absent
    /// or not a boolean.
    pub fn get_bool(&self, key: &str) -> Option<bool> {
        match self.get(key)? {
            PdfValue::Bool(value) => Some(*value),
            _ => None,
        }
    }

    /// Look up a key and return any names contained in its value.
    ///
    /// Accepts either a single `Name` value or an `Array` of names; returns an
    /// empty vector for anything else.
    pub fn get_name_array(&self, key: &str) -> Vec<String> {
        match self.get(key) {
            Some(PdfValue::Array(values)) => values
                .iter()
                .filter_map(|value| match value {
                    PdfValue::Name(name) => Some(name.clone()),
                    _ => None,
                })
                .collect(),
            Some(PdfValue::Name(name)) => vec![name.clone()],
            _ => Vec::new(),
        }
    }
}

/// One entry in a [`PdfDictionary`].
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct PdfDictionaryEntry {
    /// Entry key.
    pub key: String,
    /// Entry value.
    pub value: PdfValue,
}

/// An array or dictionary part-way through being copied by
/// [`PdfValue::clone`].
enum CopiedContainer<'a> {
    Array {
        rest: std::slice::Iter<'a, PdfValue>,
        copied: Vec<PdfValue>,
    },
    Dictionary {
        rest: std::slice::Iter<'a, PdfDictionaryEntry>,
        copied: Vec<PdfDictionaryEntry>,
        /// Key of the entry whose value is being copied.
        key: &'a str,
    },
}

/// Copies a value without recursing once per level of nesting.
///
/// The derived implementation calls itself for every array and dictionary
/// level, and a value nested to the parser's limit then needs more stack to
/// copy than to parse: page resources are copied on the way to every reader.
/// Containers being copied are kept in a list instead.
impl Clone for PdfValue {
    fn clone(&self) -> Self {
        let mut open: Vec<CopiedContainer<'_>> = Vec::new();
        let mut source = self;
        loop {
            let mut finished = match source {
                Self::Null => Some(Self::Null),
                Self::Bool(value) => Some(Self::Bool(*value)),
                Self::Integer(value) => Some(Self::Integer(*value)),
                Self::Real(value) => Some(Self::Real(*value)),
                Self::Name(value) => Some(Self::Name(value.clone())),
                Self::String(value) => Some(Self::String(value.clone())),
                Self::HexString(value) => Some(Self::HexString(value.clone())),
                Self::Reference {
                    object_number,
                    generation,
                } => Some(Self::Reference {
                    object_number: *object_number,
                    generation: *generation,
                }),
                Self::Raw(value) => Some(Self::Raw(value.clone())),
                Self::Array(values) => {
                    open.push(CopiedContainer::Array {
                        rest: values.iter(),
                        copied: Vec::with_capacity(values.len()),
                    });
                    None
                }
                Self::Dictionary(dictionary) => {
                    open.push(CopiedContainer::Dictionary {
                        rest: dictionary.entries.iter(),
                        copied: Vec::with_capacity(dictionary.entries.len()),
                        key: "",
                    });
                    None
                }
            };
            // Hand a finished copy to the container it belongs in, and keep
            // closing containers until one has another child to copy.
            loop {
                let Some(top) = open.last_mut() else {
                    // Nothing is open, so the copy is the whole value: a
                    // container that was just opened is still in the list.
                    return finished.unwrap_or(Self::Null);
                };
                match top {
                    CopiedContainer::Array { rest, copied } => {
                        copied.extend(finished.take());
                        if let Some(next) = rest.next() {
                            source = next;
                            break;
                        }
                    }
                    CopiedContainer::Dictionary { rest, copied, key } => {
                        if let Some(value) = finished.take() {
                            copied.push(PdfDictionaryEntry {
                                key: (*key).to_owned(),
                                value,
                            });
                        }
                        if let Some(entry) = rest.next() {
                            *key = &entry.key;
                            source = &entry.value;
                            break;
                        }
                    }
                }
                finished = open.pop().map(|container| match container {
                    CopiedContainer::Array { copied, .. } => Self::Array(copied),
                    CopiedContainer::Dictionary { copied, .. } => {
                        Self::Dictionary(PdfDictionary { entries: copied })
                    }
                });
            }
        }
    }
}

/// A PDF value as recovered from a dictionary or array.
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(Serialize))]
#[cfg_attr(feature = "serde", serde(tag = "type", content = "value"))]
pub enum PdfValue {
    /// `null`.
    Null,
    /// `true` / `false`.
    Bool(bool),
    /// Integer literal.
    Integer(i64),
    /// Real-number literal.
    Real(f64),
    /// `/Name` token.
    Name(String),
    /// Literal `(string)`.
    String(String),
    /// Hex-encoded `<string>`.
    HexString(String),
    /// Array of values.
    Array(Vec<PdfValue>),
    /// Inline dictionary.
    Dictionary(PdfDictionary),
    /// Indirect-object reference (`N G R`).
    Reference {
        /// Referenced object number.
        object_number: u32,
        /// Referenced generation.
        generation: u16,
    },
    /// Anything else preserved as raw text (recovery fallback).
    Raw(String),
}

/// Decoded stream metadata.
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct PdfStream {
    /// Owning object number.
    pub object_number: u32,
    /// Owning object generation.
    pub generation: u16,
    /// Byte range of the raw (filtered) stream payload.
    pub raw_range: ByteRange,
    /// Length in bytes of the raw payload.
    pub raw_len: usize,
    /// Length in bytes of the decoded payload, when decoding succeeded.
    pub decoded_len: Option<usize>,
    /// Filter chain declared on the stream, in application order.
    pub filters: Vec<String>,
    /// Stream classification (content, image, font, …).
    pub kind: PdfStreamKind,
    /// Result of attempting to decode the stream.
    pub decode_status: DecodeStatus,
    /// Image metadata, when `kind == Image`.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub image: Option<PdfImageInfo>,
    /// Font-program metadata, when `kind == FontProgram`.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub font_program: Option<PdfFontProgramInfo>,
    /// Short text preview of the decoded payload for diagnostics.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub decoded_preview: Option<String>,
    #[cfg_attr(feature = "serde", serde(skip))]
    decoded: Option<Vec<u8>>,
}

/// Coarse classification of a [`PdfStream`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub enum PdfStreamKind {
    /// Page or form content stream.
    Content,
    /// `/ToUnicode` or other CMap stream.
    CMap,
    /// Image XObject payload.
    Image,
    /// Embedded font program (Type 1, TrueType, CFF, …).
    FontProgram,
    /// Document metadata stream.
    Metadata,
    /// Compressed object stream (`/Type /ObjStm`).
    ObjectStream,
    /// Cross-reference stream.
    XRef,
    /// Unknown / unclassified.
    Unknown,
}

/// Result of attempting to decode a [`PdfStream`].
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
#[cfg_attr(feature = "serde", serde(tag = "state", content = "detail"))]
pub enum DecodeStatus {
    /// No decoding requested or no filter was applied.
    Raw,
    /// Decoding succeeded.
    Decoded,
    /// Decoding was deliberately skipped (with a human-readable reason).
    Skipped(String),
    /// Decoding failed (with a human-readable reason).
    Failed(String),
}

/// Image-XObject metadata extracted from a stream's dictionary.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct PdfImageInfo {
    /// `/Width` in pixels.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub width: Option<i64>,
    /// `/Height` in pixels.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub height: Option<i64>,
    /// `/BitsPerComponent`.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub bits_per_component: Option<i64>,
    /// `/ColorSpace` name.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub color_space: Option<String>,
    /// Hinted source format (e.g. `jpeg`, `jbig2`) when known.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub format_hint: Option<String>,
    /// Decode parameters associated with the image stream's terminal filter.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub decode_parms: Option<PdfDictionary>,
}

/// Font-program stream metadata.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct PdfFontProgramInfo {
    /// Font subtype (`/Type1`, `/TrueType`, `/CIDFontType0`, …).
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub subtype: Option<String>,
    /// Hinted underlying format (e.g. `cff`, `otf`) when known.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub format_hint: Option<String>,
}

/// A single text-operator emission from a content stream (raw, pre-CMap).
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct PdfTextFragment {
    /// Owning object number.
    pub object_number: u32,
    /// Owning object generation.
    pub generation: u16,
    /// Index into [`PdfDocument::streams`] for the source stream.
    pub stream_index: usize,
    /// PDF text operator that emitted this fragment (`Tj`, `TJ`, `'`, `"`).
    pub operator: String,
    /// Raw decoded text bytes (codepoints not yet resolved through CMap).
    pub text: String,
    /// Active font name when the fragment was emitted.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub font: Option<String>,
}

/// A semantically-resolved text run with full styling and geometry context.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct PdfTextRun {
    /// 0-based page index.
    pub page_index: usize,
    /// Index into [`PdfDocument::streams`] for the source stream.
    pub stream_index: usize,
    /// PDF text operator that emitted the run (`Tj`, `TJ`, `'`, `"`).
    pub operator: String,
    /// Decoded text, after `/ToUnicode` mapping when available.
    pub text: String,
    /// Number of characters that could not be resolved via ToUnicode CMap and
    /// fell back to raw byte-as-char decoding.  A high ratio of this count vs
    /// `text.chars().count()` indicates that the font uses a custom encoding
    /// without a proper ToUnicode entry — the text is likely garbled.
    pub cmap_fallback_chars: u32,
    /// How the run was decoded.  See [`DecodingPath`] for the priority order
    /// (`ToUnicodeCMap` is the most accurate, `RawByteFallback` is a heuristic
    /// last resort).
    pub decoding_path: DecodingPath,
    /// Active font name when the run was emitted.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub font: Option<String>,
    /// Object number of the resolved font, when known.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub font_object: Option<u32>,
    /// Generation number of the resolved font, when known.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub font_generation: Option<u16>,
    /// Effective font size in user-space units.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub font_size: Option<f32>,
    /// Run origin x in page space.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub x: Option<f32>,
    /// Run origin y (baseline) in page space.
    pub y: Option<f32>,
    /// Run advance width in page units.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub w: Option<f32>,
    /// Run height (ascent + descent) in page units.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub h: Option<f32>,
    /// Fill colour applied at the time of emission.
    pub fill_color: crate::tessellation::PdfColor,
    /// Stroke colour applied at the time of emission.
    pub stroke_color: crate::tessellation::PdfColor,
    /// PDF text-rendering mode (`Tr` operand).
    pub render_mode: crate::tessellation::TextRenderMode,
    /// `Tc` character spacing operand.
    pub char_spacing: f32,
    /// `Tw` word spacing operand.
    pub word_spacing: f32,
    /// `Tz` horizontal scaling operand (percent).
    pub horizontal_scaling: f32,
    /// `Ts` text rise operand.
    pub text_rise: f32,
}

/// A coarse text block: concatenated text in a single page-space bbox.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct PdfTextBlock {
    /// 0-based page index.
    pub page_index: usize,
    /// Concatenated block text.
    pub text: String,
    /// Block left edge in page space.
    pub x: f32,
    /// Block top edge in page space.
    pub y: f32,
    /// Block width in page units.
    pub w: f32,
    /// Block height in page units.
    pub h: f32,
}

/// Detect column gutters: vertical strips of width ≥ ~3% of the page where
/// no glyph site lives. Place a `VBarrier` at the middle of each gutter,
/// with the y-range spanning every glyph site so any cross-gutter merge
/// hits an infinite penalty.
///
/// Conservative: returns an empty vector for single-column pages.
/// One vector glyph as geometry: `(page_x, ink_top_y, size, descriptor)`.
/// The descriptor is `2k` floats of arc-length-resampled normalized outline —
/// the smooth identity used for metric matching (see
/// [`PdfDocument::vector_glyph_descriptors`]).
pub type GlyphGeom = (f32, f32, f32, Vec<f32>);
/// A banded visual text line: crop box plus members `(x, payload)` in x order.
pub type TextLine<T> = ([f32; 4], Vec<(f32, T)>);

/// Projection-profile banding: ink-count histogram over y, bands = contiguous
/// above-threshold intervals between valleys. The gap-chaining variant below
/// depends on inter-row gaps surviving in the sorted-y sequence; on DENSE
/// pages (a 9k-glyph price table) intra-row scatter bridges every row and
/// chaining collapses the page into one band (measured). A projection valley
/// exists wherever rows separate, regardless of density.
pub fn band_by_projection<T: Copy>(
    items: Vec<(f32, f32, T)>,
    bin: f32,
    split_columns: bool,
) -> Vec<TextLine<T>> {
    if items.is_empty() {
        return Vec::new();
    }
    let bin = bin.max(0.1);
    let y_min = items.iter().map(|i| i.1).fold(f32::MAX, f32::min);
    let y_max = items.iter().map(|i| i.1).fold(f32::MIN, f32::max);
    let n_bins = (((y_max - y_min) / bin) as usize + 2).max(1);
    let mut raw = vec![0u32; n_bins];
    for it in &items {
        raw[((it.1 - y_min) / bin) as usize] += 1;
    }
    // window-3 smoothing: 1–2-bin valleys INSIDE a line (ascender vs
    // x-height ink-tops) close, real inter-row gaps (>= 3 empty bins)
    // survive — a fixed merge threshold could not serve both a sparse text
    // page and a dense table (measured: one collapsed whichever the other
    // needed)
    let hist: Vec<u32> = (0..n_bins)
        .map(|i| {
            let a = if i > 0 { raw[i - 1] } else { 0 };
            let c = if i + 1 < n_bins { raw[i + 1] } else { 0 };
            a + raw[i] + c
        })
        .collect();
    // contiguous non-empty runs of bins = candidate bands
    let mut intervals: Vec<(f32, f32)> = Vec::new();
    let mut start: Option<usize> = None;
    for (i, &c) in hist.iter().enumerate() {
        if c > 0 {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(s0) = start.take() {
            intervals.push((y_min + s0 as f32 * bin, y_min + i as f32 * bin));
        }
    }
    if let Some(s0) = start {
        intervals.push((y_min + s0 as f32 * bin, y_min + n_bins as f32 * bin));
    }
    // median band height stands in for the leading in the box formula
    let mut heights: Vec<f32> = intervals.iter().map(|(a, b)| b - a).collect();
    heights.sort_by(|a, b| a.total_cmp(b));
    let lead = heights
        .get(heights.len() / 2)
        .copied()
        .unwrap_or(10.0)
        .clamp(4.0, 40.0);
    let gutter = if split_columns {
        lead * 2.0
    } else {
        f32::INFINITY
    };
    let mut lines: Vec<TextLine<T>> = Vec::new();
    for (a, b) in intervals {
        let mut band: Vec<(f32, f32, T)> = items
            .iter()
            .filter(|i| i.1 >= a && i.1 < b)
            .copied()
            .collect();
        if band.is_empty() {
            continue;
        }
        band.sort_by(|p, q| p.0.total_cmp(&q.0));
        let top = band.iter().map(|i| i.1).fold(f32::MAX, f32::min);
        let mut members: Vec<(f32, T)> = Vec::new();
        let (mut x0, mut x1) = (band[0].0, band[0].0);
        for &(x, _, payload) in &band {
            if x - x1 > gutter {
                lines.push((
                    [x0, top - lead * 0.2, x1 + lead * 0.6, top + lead * 1.1],
                    std::mem::take(&mut members),
                ));
                x0 = x;
            }
            x1 = x1.max(x);
            members.push((x, payload));
        }
        lines.push((
            [x0, top - lead * 0.2, x1 + lead * 0.6, top + lead * 1.1],
            members,
        ));
    }
    lines.sort_by(|a, b| a.0[1].total_cmp(&b.0[1]).then(a.0[0].total_cmp(&b.0[0])));
    lines
}

/// Band items carrying `(x, baseline_y, payload)` into visual text lines.
///
/// The one line-grouping algorithm in this crate — both `text_line_boxes`
/// (crop boxes for a recognizer) and `vector_glyph_cluster_lines` (glyph
/// membership for self-labelled decoding) run through here, so a glyph can
/// never fall on a different line than the box built around it.
///
/// 1. Measure the document's LEADING before banding anything. Baselines on
///    one visual line scatter by ~0.7pt (kerning / subpixel placement) while
///    true line steps are ~11pt, so the line-break signal is the population
///    of LARGE gaps: median of gaps above a floor, banding relative to that.
/// 2. Band by baseline against the band's RUNNING tail so a slowly drifting
///    line does not split mid-way.
/// 3. Within a band, optionally split on gutters (gaps > ~2 line-heights) so
///    table cells stay individual crops.
pub fn band_text_lines_public<T: Copy>(
    mut items: Vec<(f32, f32, T)>,
    line_tolerance: f32,
    split_columns: bool,
) -> Vec<TextLine<T>> {
    if items.is_empty() {
        return Vec::new();
    }
    items.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.total_cmp(&b.0)));

    let mut distinct: Vec<f32> = items.iter().map(|o| o.1).collect();
    distinct.dedup_by(|a, b| (*a - *b).abs() < 0.05);
    let mut big_gaps: Vec<f32> = distinct
        .windows(2)
        .map(|w| w[1] - w[0])
        .filter(|g| *g > 4.0 && *g < 60.0)
        .collect();
    big_gaps.sort_by(|a, b| a.total_cmp(b));
    let leading = big_gaps
        .get(big_gaps.len() / 2)
        .copied()
        .unwrap_or(12.0)
        .clamp(6.0, 40.0);
    // Same visual line when within ~40% of the leading — comfortably above
    // the intra-line scatter, comfortably below a real line step.
    let band_tol = (leading * 0.4).max(line_tolerance);
    // y values are ink-TOP positions: the box needs a small margin above the
    // top and a full glyph body (plus descender room) below it
    let ascent = leading * 0.2;
    let descent = leading * 1.1;

    let mut bands: Vec<Vec<(f32, f32, T)>> = Vec::new();
    for o in items {
        match bands.last_mut() {
            Some(band) if (o.1 - band[band.len() - 1].1).abs() <= band_tol => band.push(o),
            _ => bands.push(vec![o]),
        }
    }

    let gutter = if split_columns {
        leading * 2.0
    } else {
        f32::INFINITY
    };
    let mut lines: Vec<TextLine<T>> = Vec::new();
    for mut band in bands {
        // Sort by x: origins arrive in baseline order, and a band spans
        // several baselines, so x is not monotonic here (left unsorted this
        // produced negative-width boxes).
        band.sort_by(|a, b| a.0.total_cmp(&b.0));
        // Baseline of the band = the median, robust to the intra-line drift.
        let baseline = band[band.len() / 2].1;
        let mut members: Vec<(f32, T)> = Vec::new();
        let (mut x0, mut x1) = (band[0].0, band[0].0);
        for &(x, _, payload) in &band {
            if x - x1 > gutter {
                lines.push((
                    [
                        x0,
                        baseline - ascent,
                        x1 + leading * 0.6,
                        baseline + descent,
                    ],
                    std::mem::take(&mut members),
                ));
                x0 = x;
            }
            x1 = x1.max(x);
            members.push((x, payload));
        }
        lines.push((
            [
                x0,
                baseline - ascent,
                x1 + leading * 0.6,
                baseline + descent,
            ],
            members,
        ));
    }

    lines.sort_by(|a, b| a.0[1].total_cmp(&b.0[1]).then(a.0[0].total_cmp(&b.0[0])));
    lines
}

fn detect_column_gutters(
    sites: &[crate::tessellation::GlyphSite],
    page_width: f64,
) -> Vec<crate::tessellation::VBarrier> {
    if sites.len() < 16 || page_width < 100.0 {
        return Vec::new();
    }

    // 1pt bins across the whole page. `occupied[i] = true` iff any site
    // overlaps column [i, i+1).
    let bins = page_width.ceil() as usize;
    let mut occupied = vec![false; bins];
    let mut y_min = f64::INFINITY;
    let mut y_max = f64::NEG_INFINITY;
    for s in sites {
        let lo = s.x.floor().max(0.0) as usize;
        let hi = (s.x_end.ceil() as usize).min(bins);
        if lo < hi {
            occupied[lo..hi].fill(true);
        }
        if s.y < y_min {
            y_min = s.y;
        }
        if s.y > y_max {
            y_max = s.y;
        }
    }
    if !y_min.is_finite() || !y_max.is_finite() || y_max <= y_min {
        return Vec::new();
    }

    // Find leftmost and rightmost occupied bins so we only consider gutters
    // INSIDE the body text region (ignore page margins).
    let body_lo = match occupied.iter().position(|&o| o) {
        Some(i) => i,
        None => return Vec::new(),
    };
    let body_hi = match occupied.iter().rposition(|&o| o) {
        Some(i) => i,
        None => return Vec::new(),
    };
    if body_hi <= body_lo + 100 {
        return Vec::new(); // body is too narrow to have columns
    }

    // A real gutter must be wide enough to clearly separate columns. 3% of
    // page width or 18 pt, whichever is larger, keeps us conservative on
    // narrow PDFs without missing legitimate ½-inch gutters on US Letter.
    let min_gutter = (page_width * 0.03).max(18.0).ceil() as usize;

    let mut barriers = Vec::new();
    let mut run_start: Option<usize> = None;
    for (i, is_occupied) in occupied.iter().enumerate().take(body_hi + 1).skip(body_lo) {
        if !*is_occupied {
            run_start.get_or_insert(i);
        } else if let Some(start) = run_start.take() {
            let width = i - start;
            if width >= min_gutter {
                let x_mid = (start + i) as f64 / 2.0;
                barriers.push(crate::tessellation::VBarrier {
                    x: x_mid,
                    y_start: y_min - 1.0,
                    y_end: y_max + 1.0,
                });
            }
        }
    }
    barriers
}

/// One block in reading order. Returned by [`PdfDocument::reading_order_blocks`].
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct ReadingOrderBlock {
    /// Indices into the result of [`PdfDocument::semantic_text_runs_for_page`]
    /// for the runs this block covers. Within the block they remain in PDF
    /// emission order.
    pub run_indices: Vec<usize>,
    /// Block left edge in page space.
    pub x_min: f32,
    /// Block bottom edge in page space.
    pub y_min: f32,
    /// Block right edge in page space.
    pub x_max: f32,
    /// Block top edge in page space.
    pub y_max: f32,
}

/// Sort tessellation blocks in reading order: full-page-width banners
/// (titles, banners) first by descending y, then columns left-to-right
/// where each column's contents are sorted top-to-bottom.
fn sort_blocks_reading_order(blocks: &mut Vec<ReadingOrderBlock>, page_width: f32) {
    if blocks.len() < 2 || page_width < 1.0 {
        return;
    }

    // 1. Separate full-width banners from column-bound blocks.
    let banner_threshold = page_width * 0.80;
    let (mut banners, mut body): (Vec<_>, Vec<_>) = blocks
        .drain(..)
        .partition(|b| (b.x_max - b.x_min) >= banner_threshold);

    // 2. Cluster body blocks into columns by x_min. We use a one-pass
    //    sweep over x_min-sorted blocks: a column break is any gap larger
    //    than `column_gap` between adjacent x_min values.
    let column_gap = (page_width * 0.10).max(30.0);
    body.sort_by(|a, b| a.x_min.total_cmp(&b.x_min));

    let mut columns: Vec<Vec<ReadingOrderBlock>> = Vec::new();
    let mut current: Vec<ReadingOrderBlock> = Vec::new();
    let mut last_x: Option<f32> = None;
    for b in body {
        let is_new_column = match last_x {
            Some(lx) => b.x_min - lx > column_gap,
            None => false,
        };
        if is_new_column && !current.is_empty() {
            columns.push(std::mem::take(&mut current));
        }
        last_x = Some(b.x_min);
        current.push(b);
    }
    if !current.is_empty() {
        columns.push(current);
    }

    // 3. Banners sorted top-to-bottom (descending y_max).
    banners.sort_by(|a, b| b.y_max.total_cmp(&a.y_max));

    // 4. Each column sorted top-to-bottom.
    for col in columns.iter_mut() {
        col.sort_by(|a, b| b.y_max.total_cmp(&a.y_max));
    }

    // 5. Stitch: banners first, then columns left-to-right.
    let mut out: Vec<ReadingOrderBlock> = banners;
    for col in columns {
        out.extend(col);
    }
    *blocks = out;
}

/// A geometric line segment recovered from path operators.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct LineSegment {
    /// Start x in page space.
    pub x0: f32,
    /// Start y in page space.
    pub y0: f32,
    /// End x in page space.
    pub x1: f32,
    /// End y in page space.
    pub y1: f32,
}

/// Snapped horizontal/vertical line grid inferred for table reconstruction.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct TableGrid {
    /// Canonical column edges in page-space x.
    pub canonical_x: Vec<f32>,
    /// Canonical row edges in page-space y.
    pub canonical_y: Vec<f32>,
    /// Horizontal edge presence: `h_edges[row][col]`.
    pub h_edges: Vec<Vec<bool>>,
    /// Vertical edge presence: `v_edges[row][col]`.
    pub v_edges: Vec<Vec<bool>>,
}

/// A piece of text assigned to a specific table cell during reconstruction.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct TableTextAssignment {
    /// Row index in the canonical grid.
    pub row: usize,
    /// Column index in the canonical grid.
    pub col: usize,
    /// Assigned text content.
    pub text: String,
    /// X coordinate of the originating run in page space.
    pub x: f32,
    /// Y coordinate of the originating run in page space.
    pub y: f32,
    /// Font size of the originating run, when known.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub font_size: Option<f32>,
}

/// One OCR-derived text run in pixel space (top-down).
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct OcrTextRun {
    /// Recognised text content.
    pub text: String,
    /// Run left edge in raster pixels.
    pub px_x: f32,
    /// Run baseline y in raster pixels (top-down).
    pub px_y: f32,
    /// Run height in raster pixels.
    pub px_height: f32,
}

/// Convert OCR pixel-space coordinates back to PDF user space.
///
/// Returns `(pdf_x, pdf_y, pdf_font_size)`. Shared by [`OcrTextRun`] and the
/// columnar Arrow OCR path so both stay bit-identical.
pub fn ocr_px_to_pdf_space(
    px_x: f32,
    px_y: f32,
    px_height: f32,
    crop_box: [f32; 4],
    dpi: f32,
) -> (f32, f32, f32) {
    let scale = (dpi / 72.0).max(f32::EPSILON);
    let [x_min, _y_min, _x_max, y_max] = crop_box;
    let pdf_x = (px_x / scale) + x_min;
    let pdf_y = y_max - (px_y / scale);
    let pdf_font_size = px_height / scale;
    (pdf_x, pdf_y, pdf_font_size)
}

impl OcrTextRun {
    /// Convert pixel-space coordinates back to PDF user space given a
    /// `crop_box` and the DPI at which the page was rasterised.
    ///
    /// Returns `(pdf_x, pdf_y, pdf_font_size)`.
    pub fn to_pdf_space(&self, crop_box: [f32; 4], dpi: f32) -> (f32, f32, f32) {
        ocr_px_to_pdf_space(self.px_x, self.px_y, self.px_height, crop_box, dpi)
    }
}

/// One reconstructed table cell, possibly spanning multiple rows or columns.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct TableCell {
    /// First row this cell occupies.
    pub row_start: usize,
    /// One-past-last row this cell occupies.
    pub row_end: usize,
    /// First column this cell occupies.
    pub col_start: usize,
    /// One-past-last column this cell occupies.
    pub col_end: usize,
    /// Cell bounding box `[x_min, y_min, x_max, y_max]` in page space.
    pub bbox: [f32; 4],
    /// Concatenated cell text.
    pub text: String,
    /// Source text assignments that contributed to the cell.
    pub assignments: Vec<TableTextAssignment>,
}

/// Reconstructed table layout: canonical grid + flattened cells.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct TableLayout {
    /// Canonical column edges in page-space x.
    pub canonical_x: Vec<f32>,
    /// Canonical row edges in page-space y.
    pub canonical_y: Vec<f32>,
    /// Flattened cells (no implicit ordering — read via `bbox`/`row_start`).
    pub cells: Vec<TableCell>,
}

/// Final exported PDF table with positional bounds and rendered markdown.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct PdfTable {
    /// 0-based page index.
    pub page_index: usize,
    /// 0-based table index within the page.
    pub table_index: usize,
    /// First row index in the page's canonical grid.
    pub row_start: usize,
    /// One-past-last row index in the page's canonical grid.
    pub row_end: usize,
    /// First column index in the page's canonical grid.
    pub col_start: usize,
    /// One-past-last column index in the page's canonical grid.
    pub col_end: usize,
    /// Number of rows in the table.
    pub row_count: usize,
    /// Number of columns in the table.
    pub col_count: usize,
    /// Number of cells (after merging spans).
    pub cell_count: usize,
    /// Number of leading rows treated as headers.
    pub header_row_count: usize,
    /// Row index where the body starts, when distinct from `header_row_count`.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub body_row_start: Option<usize>,
    /// Table bounding box `[x_min, y_min, x_max, y_max]` in page space.
    pub bbox: [f32; 4],
    /// Markdown rendering of the table.
    pub markdown: String,
    /// Flattened cells.
    pub cells: Vec<TableCell>,
}

/// `/ToUnicode` CMap recovered from a font dictionary.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct PdfCMap {
    /// Owning font-object number.
    pub object_number: u32,
    /// Owning font-object generation.
    pub generation: u16,
    /// CMap name (`/CMapName`) when present.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub name: Option<String>,
    /// `begincodespacerange` ranges defining valid input codes.
    pub code_space_ranges: Vec<PdfCMapCodeSpaceRange>,
    /// Decoded `bfchar` / `bfrange` entries.
    pub entries: Vec<PdfCMapEntry>,
}

/// One `begincodespacerange` entry from a CMap (hex byte strings).
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct PdfCMapCodeSpaceRange {
    /// Inclusive low end of the codespace, as the raw hex string.
    pub start: String,
    /// Inclusive high end of the codespace, as the raw hex string.
    pub end: String,
}

/// One mapping entry inside a CMap: a single char, a range, or a range with
/// an explicit `dsts` array.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
#[cfg_attr(feature = "serde", serde(tag = "kind"))]
pub enum PdfCMapEntry {
    /// Single-character `bfchar` mapping.
    Char {
        /// Source code (hex bytes).
        src: String,
        /// Destination Unicode (hex bytes).
        dst: String,
    },
    /// Linear `bfrange src_start src_end dst_start` mapping.
    Range {
        /// Inclusive start of the source range.
        start: String,
        /// Inclusive end of the source range.
        end: String,
        /// Destination Unicode for the start of the range; subsequent codes
        /// increment the trailing byte.
        dst_start: String,
    },
    /// `bfrange src_start src_end [dst1 dst2 …]` mapping.
    RangeArray {
        /// Inclusive start of the source range.
        start: String,
        /// Inclusive end of the source range.
        end: String,
        /// One destination per source code in `[start..=end]`.
        dsts: Vec<String>,
    },
}

/// Non-fatal warning emitted by the parser during recovery.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct PdfWarning {
    /// Object number the warning is associated with, when available.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub object_number: Option<u32>,
    /// Generation number the warning is associated with, when available.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub generation: Option<u16>,
    /// Human-readable warning message.
    pub message: String,
}

type ContentMatrix = [f32; 6];

const IDENTITY_MATRIX: ContentMatrix = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

use crate::tessellation::{ColorSpace, PdfColor, TextRenderMode};

#[derive(Debug, Clone)]
struct ContentState {
    ctm: ContentMatrix,
    graphics_stack: Vec<ContentMatrix>,
    color_stack: Vec<(PdfColor, PdfColor)>, // (fill_color, stroke_color)
    /// `q` operators seen past [`MAX_GRAPHICS_STATE_DEPTH`], awaiting their `Q`.
    unsaved_states: usize,
    /// The matrix and colors at the first `q` past the limit, restored by
    /// every `Q` past it.
    overflow_state: Option<(ContentMatrix, PdfColor, PdfColor)>,
    text_matrix: ContentMatrix,
    line_matrix: ContentMatrix,
    leading: f32,
    font_size: Option<f32>,
    font_name: Option<String>,
    fill_color: PdfColor,
    stroke_color: PdfColor,
    render_mode: TextRenderMode,
    char_spacing: f32,
    word_spacing: f32,
    horizontal_scaling: f32,
    text_rise: f32,
}

impl Default for ContentState {
    fn default() -> Self {
        Self {
            ctm: IDENTITY_MATRIX,
            graphics_stack: Vec::new(),
            color_stack: Vec::new(),
            unsaved_states: 0,
            overflow_state: None,
            text_matrix: IDENTITY_MATRIX,
            line_matrix: IDENTITY_MATRIX,
            leading: 0.0,
            font_size: None,
            font_name: None,
            fill_color: PdfColor {
                space: ColorSpace::DeviceGray,
                components: vec![0.0],
            },
            stroke_color: PdfColor {
                space: ColorSpace::DeviceGray,
                components: vec![0.0],
            },
            render_mode: TextRenderMode::Fill,
            char_spacing: 0.0,
            word_spacing: 0.0,
            horizontal_scaling: 100.0,
            text_rise: 0.0,
        }
    }
}

/// How a `FontBinding` resolves a byte string to characters.
///
/// Higher tiers (earlier variants) are preferred and produce zero `cmap_fallback_chars`;
/// lower tiers either map approximately or fall through to a Latin-1 byte cast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum DecodingPath {
    /// `/ToUnicode` CMap embedded in the font (most accurate).
    ToUnicodeCMap,
    /// Type0 / CID font without an explicit CMap — text is UTF-16 BE.
    Type0Utf16Be,
    /// `/Encoding` dictionary with `/Differences` overrides applied.
    EncodingDifferences,
    /// `/Encoding` is a base name (`StandardEncoding`, `WinAnsiEncoding`, …).
    EncodingNamed,
    /// Standard Type 1 font (Helvetica/Times/Courier) with implicit
    /// `StandardEncoding` mapping.
    StandardImplicit,
    /// No encoding information available — bytes were treated as Latin-1.
    RawByteFallback,
}

/// Priority order for [`DecodingPath`] — lower values are preferred.  Used
/// when summarising a run that mixes paths (e.g. a `TJ` array spanning a
/// font switch) into a single representative path.
fn decoding_path_priority(path: &DecodingPath) -> u8 {
    match path {
        DecodingPath::ToUnicodeCMap => 0,
        DecodingPath::Type0Utf16Be => 1,
        DecodingPath::EncodingDifferences => 2,
        DecodingPath::EncodingNamed => 3,
        DecodingPath::StandardImplicit => 4,
        DecodingPath::RawByteFallback => 5,
    }
}

#[derive(Debug, Clone)]
struct FontBinding {
    resource_name: String,
    object_number: u32,
    generation: u16,
    subtype: Option<String>,
    to_unicode: Option<UnicodeMap>,
    encoding: Option<crate::encoding::EncodingMap>,
    encoding_source: EncodingSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EncodingSource {
    /// No `/Encoding` entry was available.
    None,
    /// `/Encoding` was a name (e.g. `WinAnsiEncoding`).
    Named,
    /// `/Encoding` was a dictionary with a `/Differences` array.
    Differences,
    /// Synthesised implicit encoding for a Standard 14 Type 1 font.
    StandardImplicit,
}

impl FontBinding {
    /// Decode `bytes` to a string, returning the produced text along with the
    /// number of characters that fell through to the raw-byte (Latin-1) path.
    ///
    /// Resolution order:
    /// 1. `/ToUnicode` CMap (highest fidelity).
    /// 2. Type0 / CIDFont — interpret as UTF-16 BE.
    /// 3. `/Encoding` (Named, Differences, or Standard-implicit).
    /// 4. Raw-byte Latin-1 fallback (returns the full character count as the
    ///    fallback budget so callers can detect heuristic decoding).
    fn decode_counting(&self, bytes: &[u8]) -> (String, u32, DecodingPath) {
        if let Some(map) = &self.to_unicode {
            let (text, fallback) = map.decode_counting(bytes);
            return (text, fallback, DecodingPath::ToUnicodeCMap);
        }
        if matches!(
            self.subtype.as_deref(),
            Some("Type0" | "CIDFontType0" | "CIDFontType2")
        ) {
            return (decode_utf16_be(bytes), 0, DecodingPath::Type0Utf16Be);
        }
        if let Some(encoding) = &self.encoding {
            let (text, fallback) = encoding.decode(bytes);
            let path = match self.encoding_source {
                EncodingSource::Differences => DecodingPath::EncodingDifferences,
                EncodingSource::Named => DecodingPath::EncodingNamed,
                EncodingSource::StandardImplicit => DecodingPath::StandardImplicit,
                EncodingSource::None => DecodingPath::RawByteFallback,
            };
            return (text, fallback, path);
        }
        let text = decode_pdf_text(bytes);
        let fallback = text.chars().count() as u32;
        (text, fallback, DecodingPath::RawByteFallback)
    }
}

/// Standard 14 PDF Type 1 fonts.  These ship with implicit encodings even
/// when neither `/Encoding` nor `/ToUnicode` is present in the PDF — Helvetica,
/// Times, and Courier use `StandardEncoding`; Symbol and ZapfDingbats use
/// font-specific encodings that are not yet decoded by this crate.
fn standard14_implicit_encoding(base_font: &str) -> Option<crate::encoding::BaseEncoding> {
    // Strip 14-byte font subset prefix ("ABCDEF+") if present.
    let name = base_font
        .split_once('+')
        .map(|(_, n)| n)
        .unwrap_or(base_font);
    match name {
        "Helvetica"
        | "Helvetica-Bold"
        | "Helvetica-Oblique"
        | "Helvetica-BoldOblique"
        | "Times-Roman"
        | "Times-Bold"
        | "Times-Italic"
        | "Times-BoldItalic"
        | "Courier"
        | "Courier-Bold"
        | "Courier-Oblique"
        | "Courier-BoldOblique" => Some(crate::encoding::BaseEncoding::Standard),
        _ => None,
    }
}

#[derive(Debug, Clone, Default)]
struct UnicodeMap {
    code_lengths: Vec<usize>,
    chars: HashMap<Vec<u8>, String>,
    ranges: Vec<UnicodeRange>,
    range_arrays: Vec<UnicodeRangeArray>,
}

#[derive(Debug, Clone)]
struct UnicodeRange {
    byte_len: usize,
    start: u32,
    end: u32,
    dst_start: u32,
}

#[derive(Debug, Clone)]
struct UnicodeRangeArray {
    byte_len: usize,
    start: u32,
    end: u32,
    dsts: Vec<String>,
}

#[cfg(feature = "arrow")]
#[derive(Debug)]
struct ParallelTextRow {
    run: PdfTextRun,
    unicode_scalars: Vec<u32>,
}

#[cfg(feature = "arrow")]
impl ParallelTextRow {
    fn from_run(run: PdfTextRun) -> Self {
        let unicode_scalars = run.text.chars().map(u32::from).collect();
        Self {
            run,
            unicode_scalars,
        }
    }
}

#[cfg(feature = "arrow")]
#[derive(Debug)]
struct ThreadLocalTextBatch {
    arena: Bump,
    rows: Vec<ParallelTextRow>,
}

#[cfg(feature = "arrow")]
impl ThreadLocalTextBatch {
    fn new() -> Self {
        Self {
            arena: Bump::new(),
            rows: Vec::new(),
        }
    }

    fn push_run(&mut self, run: PdfTextRun) {
        let char_count = run.text.chars().count();
        let scratch = self.arena.alloc_slice_fill_copy(char_count, 0u32);
        let mut len = 0usize;
        for scalar in run.text.chars().map(u32::from) {
            scratch[len] = scalar;
            len += 1;
        }
        self.rows.push(ParallelTextRow {
            run,
            unicode_scalars: scratch[..len].to_vec(),
        });
    }
}

#[cfg(feature = "arrow")]
#[derive(Debug)]
struct PageMultimodalRow {
    page_index: u32,
    input_text: String,
    input_ids: Vec<u32>,
    bbox_2d: [i32; 4],
    image_bytes: Option<Vec<u8>>,
    image_format: Option<String>,
}

#[cfg(feature = "arrow")]
#[derive(Debug)]
struct TableCellRow {
    page_index: u32,
    table_index: u32,
    row_start: u32,
    row_end: u32,
    col_start: u32,
    col_end: u32,
    row_span: u32,
    col_span: u32,
    bbox: [f32; 4],
    text: String,
}

#[cfg(feature = "arrow")]
#[derive(Debug)]
struct TableRow {
    page_index: u32,
    table_index: u32,
    row_start: u32,
    row_end: u32,
    col_start: u32,
    col_end: u32,
    row_count: u32,
    col_count: u32,
    cell_count: u32,
    header_row_count: u32,
    body_row_start: Option<u32>,
    bbox: [f32; 4],
    markdown: String,
}

#[cfg(feature = "rasterize")]
#[derive(Clone)]
struct GraphicsState {
    ctm: ContentMatrix,
    fill_paint: Paint<'static>,
    stroke_paint: Paint<'static>,
    stroke: Stroke,
    /// Shared between a state and the copies `q` saves of it. A mask is one
    /// byte per surface pixel, so copying it per saved state would make
    /// nesting cost memory; it is copied only when a nested clip changes it.
    clip_mask: Option<Rc<Mask>>,
}

#[cfg(feature = "rasterize")]
impl Default for GraphicsState {
    fn default() -> Self {
        let mut fill_paint = Paint::default();
        fill_paint.set_color(Color::BLACK);
        let mut stroke_paint = Paint::default();
        stroke_paint.set_color(Color::BLACK);
        Self {
            ctm: IDENTITY_MATRIX,
            fill_paint,
            stroke_paint,
            stroke: Stroke::default(),
            clip_mask: None,
        }
    }
}

#[cfg(feature = "rasterize")]
struct PageRasterizer<'a> {
    doc: &'a PdfDocument,
    base_ctm: ContentMatrix,
    pixmap: Pixmap,
    px_width: u32,
    px_height: u32,
    state_stack: Vec<GraphicsState>,
    /// `q` operators seen past [`MAX_GRAPHICS_STATE_DEPTH`], awaiting their `Q`.
    unsaved_states: usize,
    /// The state at the first `q` past the limit, restored by every `Q` past
    /// it. It shares its clip mask with the state it was copied from.
    overflow_state: Option<GraphicsState>,
    path_builder: PathBuilder,
    pending_clip: Option<FillRule>,
    form_stack: Vec<(u32, u16)>,
    painted: bool,
}

struct FormXObject {
    object_id: (u32, u16),
    bytes: Vec<u8>,
    resources: Option<PdfDictionary>,
    matrix: ContentMatrix,
}

#[derive(Clone, Copy)]
struct PrimitiveState {
    ctm: ContentMatrix,
}

impl Default for PrimitiveState {
    fn default() -> Self {
        Self {
            ctm: IDENTITY_MATRIX,
        }
    }
}

/// A placed image XObject on the page (image plus page-space bbox).
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct PdfImagePlacement {
    /// `/Resources` name of the image XObject (e.g. `Im0`).
    pub name: String,
    /// PDF indirect object number of the image.
    pub object_number: u32,
    /// Image left edge in page space.
    pub x: f32,
    /// Image top edge in page space.
    pub y: f32,
    /// Image width in page units.
    pub w: f32,
    /// Image height in page units.
    pub h: f32,
}

struct TablePrimitiveExtractor<'a> {
    doc: &'a PdfDocument,
    state_stack: Vec<PrimitiveState>,
    /// `q` operators seen past [`MAX_GRAPHICS_STATE_DEPTH`], awaiting their `Q`.
    unsaved_states: usize,
    /// The state at the first `q` past the limit, restored by every `Q` past
    /// it.
    overflow_state: Option<PrimitiveState>,
    path_segments: Vec<LineSegment>,
    current_point: Option<(f32, f32)>,
    subpath_start: Option<(f32, f32)>,
    segments: Vec<LineSegment>,
    form_stack: Vec<(u32, u16)>,
    pub placed_images: Vec<PdfImagePlacement>,
}

impl UnicodeMap {
    fn from_cmap(cmap: &PdfCMap) -> Self {
        let mut map = Self::default();
        map.code_lengths = cmap
            .code_space_ranges
            .iter()
            .filter_map(|range| hex_token_bytes(&range.start).ok().map(|bytes| bytes.len()))
            .collect::<Vec<_>>();

        for entry in &cmap.entries {
            match entry {
                PdfCMapEntry::Char { src, dst } => {
                    if let (Ok(src), Some(dst)) = (hex_token_bytes(src), unicode_hex_token(dst)) {
                        map.code_lengths.push(src.len());
                        map.chars.insert(src, dst);
                    }
                }
                PdfCMapEntry::Range {
                    start,
                    end,
                    dst_start,
                } => {
                    if let (Some(byte_len), Some(start_val), Some(end_val)) = (
                        hex_token_bytes(start).ok().map(|bytes| bytes.len()),
                        hex_token_u32(start),
                        hex_token_u32(end),
                    ) {
                        map.code_lengths.push(byte_len);
                        if start_val == end_val {
                            if let (Ok(src), Some(dst)) =
                                (hex_token_bytes(start), unicode_hex_token(dst_start))
                            {
                                map.chars.insert(src, dst);
                                continue;
                            }
                        }
                        if let Some(dst_start) = unicode_hex_scalar(dst_start) {
                            map.ranges.push(UnicodeRange {
                                byte_len,
                                start: start_val,
                                end: end_val,
                                dst_start,
                            });
                        }
                    }
                }
                PdfCMapEntry::RangeArray { start, end, dsts } => {
                    if let (Some(byte_len), Some(start), Some(end)) = (
                        hex_token_bytes(start).ok().map(|bytes| bytes.len()),
                        hex_token_u32(start),
                        hex_token_u32(end),
                    ) {
                        map.code_lengths.push(byte_len);
                        map.range_arrays.push(UnicodeRangeArray {
                            byte_len,
                            start,
                            end,
                            dsts: dsts
                                .iter()
                                .filter_map(|dst| unicode_hex_token(dst))
                                .collect(),
                        });
                    }
                }
            }
        }

        // An empty source code (`<>`) names no bytes. Kept as a code length it
        // would match at every position without consuming input, so decoding
        // would never advance.
        map.code_lengths.retain(|len| *len > 0);
        map.chars.remove(&[][..]);
        map.code_lengths.sort_unstable();
        map.code_lengths.dedup();
        if map.code_lengths.is_empty() {
            map.code_lengths.extend([1, 2]);
        }

        // Some Type1 ToUnicode CMaps (e.g. Adobe InDesign) emit 1-byte bfchar
        // keys for glyphs (`<2E>`, `<41>`) alongside a 2-byte `<0020>` entry
        // for space while declaring `<0000> <FFFF>` as the codespace range.
        // When 1-byte codes are active, mirror `<00XX>` entries onto `[XX]`
        // without overwriting explicit 1-byte mappings.
        if map.code_lengths.contains(&1) {
            let zero_prefixed: Vec<(u8, String)> = map
                .chars
                .iter()
                .filter_map(|(k, v)| {
                    if k.len() == 2 && k[0] == 0 {
                        Some((k[1], v.clone()))
                    } else {
                        None
                    }
                })
                .collect();
            for (low_byte, dst) in zero_prefixed {
                map.chars.entry(vec![low_byte]).or_insert(dst);
            }
        }

        map
    }

    /// Decode `bytes` via this CMap, returning `(text, fallback_count)` where
    /// `fallback_count` is the number of characters resolved by the raw-byte
    /// fallback (i.e. not present in the CMap's bfchar/bfrange tables).
    fn decode_counting(&self, bytes: &[u8]) -> (String, u32) {
        let mut out = String::new();
        let mut fallback = 0u32;
        let mut cursor = 0usize;
        while cursor < bytes.len() {
            let mut matched = false;
            for &len in self.code_lengths.iter().rev() {
                if cursor + len > bytes.len() {
                    continue;
                }
                if let Some(text) = self.decode_code(&bytes[cursor..cursor + len]) {
                    out.push_str(&text);
                    cursor += len;
                    matched = true;
                    break;
                }
            }
            if !matched {
                out.push(bytes[cursor] as char);
                fallback += 1;
                cursor += 1;
            }
        }
        (out, fallback)
    }

    fn decode_code(&self, code: &[u8]) -> Option<String> {
        if let Some(text) = self.chars.get(code) {
            return Some(text.clone());
        }
        let value = bytes_to_u32(code);
        for range in &self.ranges {
            if range.byte_len == code.len() && value >= range.start && value <= range.end {
                // The destination start comes from the file; a range that runs
                // past `u32::MAX` names no scalar value.
                let scalar = range.dst_start.checked_add(value - range.start)?;
                return char::from_u32(scalar).map(|ch| ch.to_string());
            }
        }
        for range in &self.range_arrays {
            if range.byte_len != code.len() || value < range.start || value > range.end {
                continue;
            }
            let index = (value - range.start) as usize;
            if let Some(text) = range.dsts.get(index) {
                return Some(text.clone());
            }
        }
        None
    }
}

#[cfg(feature = "arrow")]
fn build_text_run_record_batch(rows: Vec<ParallelTextRow>) -> Result<RecordBatch> {
    let mut page_builder = UInt32Builder::new();
    let mut stream_builder = UInt32Builder::new();
    let mut operator_builder = StringBuilder::new();
    let mut font_builder = StringBuilder::new();
    let mut font_object_builder = UInt32Builder::new();
    let mut x_builder = Float32Builder::new();
    let mut y_builder = Float32Builder::new();
    let mut font_size_builder = Float32Builder::new();
    let mut text_builder = StringBuilder::new();
    let mut token_builder = ListBuilder::new(UInt32Builder::new());

    for row in rows {
        let run = row.run;
        page_builder.append_value(run.page_index as u32);
        stream_builder.append_value(run.stream_index as u32);
        operator_builder.append_value(run.operator.as_str());
        if let Some(font) = run.font.as_deref() {
            font_builder.append_value(font);
        } else {
            font_builder.append_null();
        }
        if let Some(font_object) = run.font_object {
            font_object_builder.append_value(font_object);
        } else {
            font_object_builder.append_null();
        }
        if let Some(x) = run.x {
            x_builder.append_value(x);
        } else {
            x_builder.append_null();
        }
        if let Some(y) = run.y {
            y_builder.append_value(y);
        } else {
            y_builder.append_null();
        }
        if let Some(font_size) = run.font_size {
            font_size_builder.append_value(font_size);
        } else {
            font_size_builder.append_null();
        }
        text_builder.append_value(run.text.as_str());
        {
            let values = token_builder.values();
            for scalar in row.unicode_scalars {
                values.append_value(scalar);
            }
        }
        token_builder.append(true);
    }

    let schema = Arc::new(Schema::new(vec![
        Field::new("page_index", DataType::UInt32, false),
        Field::new("stream_index", DataType::UInt32, false),
        Field::new("operator", DataType::Utf8, false),
        Field::new("font", DataType::Utf8, true),
        Field::new("font_object", DataType::UInt32, true),
        Field::new("x", DataType::Float32, true),
        Field::new("y", DataType::Float32, true),
        Field::new("font_size", DataType::Float32, true),
        Field::new("text", DataType::Utf8, false),
        Field::new(
            "unicode_scalars",
            DataType::List(Arc::new(Field::new("item", DataType::UInt32, true))),
            false,
        ),
    ]));

    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(page_builder.finish()),
            Arc::new(stream_builder.finish()),
            Arc::new(operator_builder.finish()),
            Arc::new(font_builder.finish()),
            Arc::new(font_object_builder.finish()),
            Arc::new(x_builder.finish()),
            Arc::new(y_builder.finish()),
            Arc::new(font_size_builder.finish()),
            Arc::new(text_builder.finish()),
            Arc::new(token_builder.finish()),
        ],
    )
    .map_err(Into::into)
}

#[cfg(feature = "arrow")]
fn build_page_multimodal_record_batch(rows: Vec<PageMultimodalRow>) -> Result<RecordBatch> {
    let mut page_builder = UInt32Builder::new();
    let mut text_builder = StringBuilder::new();
    let mut token_builder = ListBuilder::new(UInt32Builder::new());
    let mut bbox_builder = FixedSizeListBuilder::new(Int32Builder::new(), 4);
    let mut image_builder = BinaryBuilder::new();
    let mut image_format_builder = StringBuilder::new();

    for row in rows {
        page_builder.append_value(row.page_index);
        text_builder.append_value(row.input_text.as_str());
        {
            let values = token_builder.values();
            for token in row.input_ids {
                values.append_value(token);
            }
        }
        token_builder.append(true);
        {
            let values = bbox_builder.values();
            for value in row.bbox_2d {
                values.append_value(value);
            }
        }
        bbox_builder.append(true);
        if let Some(bytes) = row.image_bytes.as_deref() {
            image_builder.append_value(bytes);
        } else {
            image_builder.append_null();
        }
        if let Some(format) = row.image_format.as_deref() {
            image_format_builder.append_value(format);
        } else {
            image_format_builder.append_null();
        }
    }

    let schema = Arc::new(Schema::new(vec![
        Field::new("page_index", DataType::UInt32, false),
        Field::new("input_text", DataType::Utf8, false),
        Field::new(
            "input_ids",
            DataType::List(Arc::new(Field::new("item", DataType::UInt32, true))),
            false,
        ),
        Field::new(
            "bbox_2d",
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Int32, true)), 4),
            false,
        ),
        Field::new("image_bytes", DataType::Binary, true),
        Field::new("image_format", DataType::Utf8, true),
    ]));

    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(page_builder.finish()),
            Arc::new(text_builder.finish()),
            Arc::new(token_builder.finish()),
            Arc::new(bbox_builder.finish()),
            Arc::new(image_builder.finish()),
            Arc::new(image_format_builder.finish()),
        ],
    )
    .map_err(Into::into)
}

#[cfg(feature = "arrow")]
fn build_table_layout_record_batch(rows: Vec<TableCellRow>) -> Result<RecordBatch> {
    let mut page_builder = UInt32Builder::new();
    let mut table_builder = UInt32Builder::new();
    let mut row_start_builder = UInt32Builder::new();
    let mut row_end_builder = UInt32Builder::new();
    let mut col_start_builder = UInt32Builder::new();
    let mut col_end_builder = UInt32Builder::new();
    let mut row_span_builder = UInt32Builder::new();
    let mut col_span_builder = UInt32Builder::new();
    let mut bbox_builder = FixedSizeListBuilder::new(Float32Builder::new(), 4);
    let mut text_builder = StringBuilder::new();

    for row in rows {
        page_builder.append_value(row.page_index);
        table_builder.append_value(row.table_index);
        row_start_builder.append_value(row.row_start);
        row_end_builder.append_value(row.row_end);
        col_start_builder.append_value(row.col_start);
        col_end_builder.append_value(row.col_end);
        row_span_builder.append_value(row.row_span);
        col_span_builder.append_value(row.col_span);
        {
            let values = bbox_builder.values();
            for value in row.bbox {
                values.append_value(value);
            }
        }
        bbox_builder.append(true);
        text_builder.append_value(row.text.as_str());
    }

    let schema = Arc::new(Schema::new(vec![
        Field::new("page_index", DataType::UInt32, false),
        Field::new("table_index", DataType::UInt32, false),
        Field::new("row_start", DataType::UInt32, false),
        Field::new("row_end", DataType::UInt32, false),
        Field::new("col_start", DataType::UInt32, false),
        Field::new("col_end", DataType::UInt32, false),
        Field::new("row_span", DataType::UInt32, false),
        Field::new("col_span", DataType::UInt32, false),
        Field::new(
            "bbox",
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float32, true)), 4),
            false,
        ),
        Field::new("text", DataType::Utf8, false),
    ]));

    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(page_builder.finish()),
            Arc::new(table_builder.finish()),
            Arc::new(row_start_builder.finish()),
            Arc::new(row_end_builder.finish()),
            Arc::new(col_start_builder.finish()),
            Arc::new(col_end_builder.finish()),
            Arc::new(row_span_builder.finish()),
            Arc::new(col_span_builder.finish()),
            Arc::new(bbox_builder.finish()),
            Arc::new(text_builder.finish()),
        ],
    )
    .map_err(Into::into)
}

#[cfg(feature = "arrow")]
fn build_table_record_batch(rows: Vec<TableRow>) -> Result<RecordBatch> {
    let mut page_builder = UInt32Builder::new();
    let mut table_builder = UInt32Builder::new();
    let mut row_start_builder = UInt32Builder::new();
    let mut row_end_builder = UInt32Builder::new();
    let mut col_start_builder = UInt32Builder::new();
    let mut col_end_builder = UInt32Builder::new();
    let mut row_count_builder = UInt32Builder::new();
    let mut col_count_builder = UInt32Builder::new();
    let mut cell_count_builder = UInt32Builder::new();
    let mut header_row_count_builder = UInt32Builder::new();
    let mut body_row_start_builder = UInt32Builder::new();
    let mut bbox_builder = FixedSizeListBuilder::new(Float32Builder::new(), 4);
    let mut markdown_builder = StringBuilder::new();

    for row in rows {
        page_builder.append_value(row.page_index);
        table_builder.append_value(row.table_index);
        row_start_builder.append_value(row.row_start);
        row_end_builder.append_value(row.row_end);
        col_start_builder.append_value(row.col_start);
        col_end_builder.append_value(row.col_end);
        row_count_builder.append_value(row.row_count);
        col_count_builder.append_value(row.col_count);
        cell_count_builder.append_value(row.cell_count);
        header_row_count_builder.append_value(row.header_row_count);
        if let Some(body_row_start) = row.body_row_start {
            body_row_start_builder.append_value(body_row_start);
        } else {
            body_row_start_builder.append_null();
        }
        {
            let values = bbox_builder.values();
            for value in row.bbox {
                values.append_value(value);
            }
        }
        bbox_builder.append(true);
        markdown_builder.append_value(row.markdown.as_str());
    }

    let schema = Arc::new(Schema::new(vec![
        Field::new("page_index", DataType::UInt32, false),
        Field::new("table_index", DataType::UInt32, false),
        Field::new("row_start", DataType::UInt32, false),
        Field::new("row_end", DataType::UInt32, false),
        Field::new("col_start", DataType::UInt32, false),
        Field::new("col_end", DataType::UInt32, false),
        Field::new("row_count", DataType::UInt32, false),
        Field::new("col_count", DataType::UInt32, false),
        Field::new("cell_count", DataType::UInt32, false),
        Field::new("header_row_count", DataType::UInt32, false),
        Field::new("body_row_start", DataType::UInt32, true),
        Field::new(
            "bbox",
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float32, true)), 4),
            false,
        ),
        Field::new("markdown", DataType::Utf8, false),
    ]));

    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(page_builder.finish()),
            Arc::new(table_builder.finish()),
            Arc::new(row_start_builder.finish()),
            Arc::new(row_end_builder.finish()),
            Arc::new(col_start_builder.finish()),
            Arc::new(col_end_builder.finish()),
            Arc::new(row_count_builder.finish()),
            Arc::new(col_count_builder.finish()),
            Arc::new(cell_count_builder.finish()),
            Arc::new(header_row_count_builder.finish()),
            Arc::new(body_row_start_builder.finish()),
            Arc::new(bbox_builder.finish()),
            Arc::new(markdown_builder.finish()),
        ],
    )
    .map_err(Into::into)
}

/// Order items into reading order: lines top to bottom, left to right within a
/// line.
///
/// A banded comparator — "within N points compare by x, otherwise by y" — is
/// **not a total order**, and this is not a NaN edge case. For y values 0, 3
/// and 6 under a 4-point band, a/b and b/c compare by x while a/c compares by
/// y, so `a < b < c < a` is reachable with ordinary finite coordinates. Rust's
/// stable sort detects the violated contract and panics
/// ("user-provided comparison function does not correctly implement a total
/// order"), which crosses the pyo3 boundary as a PanicException. Whether it
/// trips depends on the input length, because only some paths in the sort
/// check, so such a comparator can pass every small test and panic in
/// production.
///
/// Grouping first removes the contradiction instead of hiding it: each item is
/// assigned to a line by a single pass, and the ordering within a line is a
/// plain comparison on x. Both steps are transitive, so the result is a total
/// order by construction rather than by luck.
fn sort_into_reading_order<T>(
    items: &mut [T],
    tolerance: f32,
    y_of: impl Fn(&T) -> f32,
    x_of: impl Fn(&T) -> f32,
) {
    // Top to bottom. total_cmp so a NaN coordinate orders deterministically
    // instead of poisoning the comparison.
    items.sort_by(|a, b| y_of(b).total_cmp(&y_of(a)));

    // Greedy lines, anchored on the first item of each line so the band cannot
    // drift arbitrarily far from where it started.
    let mut start = 0;
    while start < items.len() {
        let anchor = y_of(&items[start]);
        let mut end = start + 1;
        while end < items.len() && (anchor - y_of(&items[end])).abs() <= tolerance {
            end += 1;
        }
        items[start..end].sort_by(|a, b| x_of(a).total_cmp(&x_of(b)));
        start = end;
    }
}

fn contains_text_operators(bytes: &[u8]) -> bool {
    memmem::find(bytes, b"BT").is_some()
        || memmem::find(bytes, b"Tj").is_some()
        || memmem::find(bytes, b"TJ").is_some()
        || memmem::find(bytes, b"Tf").is_some()
}

#[cfg(feature = "rasterize")]
fn raster_surface_layout(crop_box: [f32; 4], dpi: f32) -> Option<(u32, u32, usize)> {
    let [x_min, y_min, x_max, y_max] = crop_box;
    let scale = dpi / 72.0;
    let px_width = ((x_max - x_min).abs() * scale).ceil() as u32;
    let px_height = ((y_max - y_min).abs() * scale).ceil() as u32;
    if px_width == 0 || px_height == 0 {
        return None;
    }
    let pixel_count = u64::from(px_width).checked_mul(u64::from(px_height))?;
    if pixel_count > 100_000_000 {
        return None;
    }
    let rgba_bytes = usize::try_from(pixel_count.checked_mul(4)?).ok()?;
    Some((px_width, px_height, rgba_bytes))
}

#[cfg(feature = "rasterize")]
fn rasterize_page_vectors(
    doc: &PdfDocument,
    content_streams: &[&[u8]],
    resources: Option<&PdfDictionary>,
    crop_box: [f32; 4],
    dpi: f32,
) -> Result<Option<Vec<u8>>> {
    let Some(mut rasterizer) = PageRasterizer::new(doc, crop_box, dpi)? else {
        return Ok(None);
    };
    for stream in content_streams {
        rasterizer.interpret_stream(stream, resources)?;
    }
    rasterizer.finish_png()
}

fn extract_table_line_segments_and_images(
    doc: &PdfDocument,
    content_streams: &[&[u8]],
    resources: Option<&PdfDictionary>,
) -> Result<(Vec<LineSegment>, Vec<PdfImagePlacement>)> {
    let mut extractor = TablePrimitiveExtractor::new(doc);
    for stream in content_streams {
        extractor.interpret_stream(stream, resources)?;
    }
    Ok(extractor.finish())
}

impl TableGrid {
    /// Snap a set of line segments to a canonical row/column grid.
    ///
    /// Endpoints are quantised within `epsilon` page units; remaining
    /// segments project onto horizontal / vertical edge masks of the inferred
    /// grid.
    pub fn build(segments: &[LineSegment], epsilon: f32) -> Self {
        let mut raw_x = Vec::with_capacity(segments.len() * 2);
        let mut raw_y = Vec::with_capacity(segments.len() * 2);
        for segment in segments {
            raw_x.push(segment.x0);
            raw_x.push(segment.x1);
            raw_y.push(segment.y0);
            raw_y.push(segment.y1);
        }

        let canonical_x = quantize_axis(raw_x, epsilon);
        let canonical_y = quantize_axis(raw_y, epsilon);
        let cols = canonical_x.len().saturating_sub(1);
        let rows = canonical_y.len().saturating_sub(1);

        let mut grid = Self {
            canonical_x,
            canonical_y,
            h_edges: vec![vec![false; cols]; rows + 1],
            v_edges: vec![vec![false; rows]; cols + 1],
        };
        grid.project_edges(segments, epsilon);
        grid
    }

    /// True when the grid has at least one row and one column.
    pub fn has_cells(&self) -> bool {
        self.canonical_x.len() >= 2 && self.canonical_y.len() >= 2
    }

    /// Number of rows in the canonical grid.
    pub fn row_count(&self) -> usize {
        self.canonical_y.len().saturating_sub(1)
    }

    /// Number of columns in the canonical grid.
    pub fn col_count(&self) -> usize {
        self.canonical_x.len().saturating_sub(1)
    }

    /// Locate the `(row, col)` cell containing the given page-space point, or
    /// `None` if the point lies outside the grid. Row indices are top-down.
    pub fn route_point(&self, x: f32, y: f32) -> Option<(usize, usize)> {
        if !self.has_cells() {
            return None;
        }
        let min_x = *self.canonical_x.first()?;
        let max_x = *self.canonical_x.last()?;
        let min_y = *self.canonical_y.first()?;
        let max_y = *self.canonical_y.last()?;
        if x < min_x || x > max_x || y < min_y || y > max_y {
            return None;
        }

        let col = self
            .canonical_x
            .partition_point(|value| *value <= x)
            .saturating_sub(1);
        let row_from_bottom = self
            .canonical_y
            .partition_point(|value| *value <= y)
            .saturating_sub(1);
        let row_count = self.canonical_y.len().saturating_sub(1);
        if col >= self.canonical_x.len().saturating_sub(1) || row_from_bottom >= row_count {
            return None;
        }
        Some((row_count - 1 - row_from_bottom, col))
    }

    /// Resolve text assignments into a [`TableLayout`] by walking merged-cell
    /// components from the snapped grid.
    pub fn to_layout(&self, assignments: &[TableTextAssignment]) -> TableLayout {
        let components = self.merged_components();
        let mut cells = Vec::new();
        for component in components {
            let mut cell_assignments = assignments
                .iter()
                .filter(|assignment| {
                    assignment.row >= component.row_start
                        && assignment.row <= component.row_end
                        && assignment.col >= component.col_start
                        && assignment.col <= component.col_end
                })
                .cloned()
                .collect::<Vec<_>>();
            cell_assignments.sort_by(|left, right| {
                left.row
                    .cmp(&right.row)
                    .then(left.col.cmp(&right.col))
                    .then_with(|| right.y.total_cmp(&left.y))
                    .then_with(|| left.x.total_cmp(&right.x))
            });
            let text = cell_assignments
                .iter()
                .map(|assignment| assignment.text.trim())
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            cells.push(TableCell {
                row_start: component.row_start,
                row_end: component.row_end,
                col_start: component.col_start,
                col_end: component.col_end,
                bbox: self.component_bbox(
                    component.row_start,
                    component.row_end,
                    component.col_start,
                    component.col_end,
                ),
                text,
                assignments: cell_assignments,
            });
        }
        cells.sort_by(|left, right| {
            left.row_start
                .cmp(&right.row_start)
                .then(left.col_start.cmp(&right.col_start))
                .then(left.row_end.cmp(&right.row_end))
                .then(left.col_end.cmp(&right.col_end))
        });
        TableLayout {
            canonical_x: self.canonical_x.clone(),
            canonical_y: self.canonical_y.clone(),
            cells,
        }
    }

    fn project_edges(&mut self, segments: &[LineSegment], epsilon: f32) {
        if !self.has_cells() {
            return;
        }

        for segment in segments {
            let is_horizontal = (segment.y0 - segment.y1).abs() <= epsilon;
            let is_vertical = (segment.x0 - segment.x1).abs() <= epsilon;
            if is_horizontal {
                let y_idx = find_nearest_axis(&self.canonical_y, segment.y0);
                let x_start = find_nearest_axis(&self.canonical_x, segment.x0.min(segment.x1));
                let x_end = find_nearest_axis(&self.canonical_x, segment.x0.max(segment.x1));
                if let Some(row) = self.h_edges.get_mut(y_idx) {
                    for col in x_start..x_end.min(row.len()) {
                        row[col] = true;
                    }
                }
            } else if is_vertical {
                let x_idx = find_nearest_axis(&self.canonical_x, segment.x0);
                let y_start = find_nearest_axis(&self.canonical_y, segment.y0.min(segment.y1));
                let y_end = find_nearest_axis(&self.canonical_y, segment.y0.max(segment.y1));
                if let Some(col) = self.v_edges.get_mut(x_idx) {
                    for row in y_start..y_end.min(col.len()) {
                        col[row] = true;
                    }
                }
            }
        }
    }

    fn merged_components(&self) -> Vec<TableComponent> {
        let rows = self.row_count();
        let cols = self.col_count();
        if rows == 0 || cols == 0 {
            return Vec::new();
        }
        let mut visited = vec![vec![false; cols]; rows];
        let mut components = Vec::new();

        for row in 0..rows {
            for col in 0..cols {
                if visited[row][col] {
                    continue;
                }
                let mut best: Option<TableComponent> = None;
                for row_end in row..rows {
                    for col_end in col..cols {
                        if self.rectangle_has_visited(&visited, row, row_end, col, col_end) {
                            continue;
                        }
                        if !self.rectangle_forms_cell(row, row_end, col, col_end) {
                            continue;
                        }
                        let candidate = TableComponent {
                            row_start: row,
                            row_end,
                            col_start: col,
                            col_end,
                        };
                        let better = best.is_none_or(|current| {
                            candidate.area() > current.area()
                                || (candidate.area() == current.area()
                                    && candidate.width() > current.width())
                                || (candidate.area() == current.area()
                                    && candidate.width() == current.width()
                                    && candidate.height() > current.height())
                        });
                        if better {
                            best = Some(candidate);
                        }
                    }
                }

                if let Some(component) = best {
                    for row in visited
                        .iter_mut()
                        .take(component.row_end + 1)
                        .skip(component.row_start)
                    {
                        for cell in row
                            .iter_mut()
                            .take(component.col_end + 1)
                            .skip(component.col_start)
                        {
                            *cell = true;
                        }
                    }
                    components.push(component);
                }
            }
        }

        components
    }

    fn rectangle_has_visited(
        &self,
        visited: &[Vec<bool>],
        row_start: usize,
        row_end: usize,
        col_start: usize,
        col_end: usize,
    ) -> bool {
        (row_start..=row_end).any(|row| (col_start..=col_end).any(|col| visited[row][col]))
    }

    fn rectangle_forms_cell(
        &self,
        row_start: usize,
        row_end: usize,
        col_start: usize,
        col_end: usize,
    ) -> bool {
        self.outer_perimeter_present(row_start, row_end, col_start, col_end)
            && self.internal_boundaries_absent(row_start, row_end, col_start, col_end)
    }

    fn outer_perimeter_present(
        &self,
        row_start: usize,
        row_end: usize,
        col_start: usize,
        col_end: usize,
    ) -> bool {
        if row_start > row_end || col_start > col_end {
            return false;
        }
        let top_axis = self.row_count().saturating_sub(row_start);
        let bottom_axis = self.row_count().saturating_sub(row_end + 1);

        (col_start..=col_end).all(|col| {
            self.h_edges
                .get(top_axis)
                .and_then(|row| row.get(col))
                .copied()
                .unwrap_or(false)
                && self
                    .h_edges
                    .get(bottom_axis)
                    .and_then(|row| row.get(col))
                    .copied()
                    .unwrap_or(false)
        }) && (row_start..=row_end).all(|row| {
            let bottom = self.bottom_row_index(row);
            self.v_edges
                .get(col_start)
                .and_then(|col| col.get(bottom))
                .copied()
                .unwrap_or(false)
                && self
                    .v_edges
                    .get(col_end + 1)
                    .and_then(|col| col.get(bottom))
                    .copied()
                    .unwrap_or(false)
        })
    }

    fn internal_boundaries_absent(
        &self,
        row_start: usize,
        row_end: usize,
        col_start: usize,
        col_end: usize,
    ) -> bool {
        (col_start + 1..=col_end).all(|internal_col| {
            (row_start..=row_end).all(|row| {
                let bottom = self.bottom_row_index(row);
                !self
                    .v_edges
                    .get(internal_col)
                    .and_then(|col| col.get(bottom))
                    .copied()
                    .unwrap_or(false)
            })
        }) && (row_start + 1..=row_end).all(|internal_row| {
            let shared_axis = self.row_count().saturating_sub(internal_row);
            (col_start..=col_end).all(|col| {
                !self
                    .h_edges
                    .get(shared_axis)
                    .and_then(|row| row.get(col))
                    .copied()
                    .unwrap_or(false)
            })
        })
    }

    fn bottom_row_index(&self, row: usize) -> usize {
        self.row_count().saturating_sub(1).saturating_sub(row)
    }

    fn component_bbox(
        &self,
        row_start: usize,
        row_end: usize,
        col_start: usize,
        col_end: usize,
    ) -> [f32; 4] {
        let top_axis = self.row_count().saturating_sub(row_start);
        let bottom_axis = self.row_count().saturating_sub(row_end + 1);
        [
            self.canonical_x.get(col_start).copied().unwrap_or_default(),
            self.canonical_y
                .get(bottom_axis)
                .copied()
                .unwrap_or_default(),
            self.canonical_x
                .get(col_end + 1)
                .copied()
                .unwrap_or_default(),
            self.canonical_y.get(top_axis).copied().unwrap_or_default(),
        ]
    }
}

impl TableLayout {
    /// Split a layout into one [`PdfTable`] per connected cell group on
    /// `page_index`. Each table includes a rendered markdown view.
    pub fn into_tables(self, page_index: usize) -> Vec<PdfTable> {
        let mut tables = Vec::new();
        let groups = adjacent_cell_groups(&self.cells);

        for group in groups {
            let mut cells = group
                .into_iter()
                .map(|index| self.cells[index].clone())
                .collect::<Vec<_>>();
            cells.sort_by(|left, right| {
                left.row_start
                    .cmp(&right.row_start)
                    .then(left.col_start.cmp(&right.col_start))
                    .then(left.row_end.cmp(&right.row_end))
                    .then(left.col_end.cmp(&right.col_end))
            });

            let row_start = cells
                .iter()
                .map(|cell| cell.row_start)
                .min()
                .unwrap_or_default();
            let row_end = cells
                .iter()
                .map(|cell| cell.row_end)
                .max()
                .unwrap_or_default();
            let col_start = cells
                .iter()
                .map(|cell| cell.col_start)
                .min()
                .unwrap_or_default();
            let col_end = cells
                .iter()
                .map(|cell| cell.col_end)
                .max()
                .unwrap_or_default();
            let bbox =
                cells
                    .iter()
                    .fold([f32::MAX, f32::MAX, f32::MIN, f32::MIN], |mut acc, cell| {
                        acc[0] = acc[0].min(cell.bbox[0]);
                        acc[1] = acc[1].min(cell.bbox[1]);
                        acc[2] = acc[2].max(cell.bbox[2]);
                        acc[3] = acc[3].max(cell.bbox[3]);
                        acc
                    });
            let header_row_count =
                infer_table_header_row_count(&cells, row_start, row_end, col_start, col_end);
            let markdown = table_markdown(
                &cells,
                row_start,
                row_end,
                col_start,
                col_end,
                header_row_count,
            );
            let row_count = row_end.saturating_sub(row_start) + 1;
            tables.push(PdfTable {
                page_index,
                table_index: tables.len(),
                row_start,
                row_end,
                col_start,
                col_end,
                row_count,
                col_count: col_end.saturating_sub(col_start) + 1,
                cell_count: cells.len(),
                header_row_count,
                body_row_start: if header_row_count < row_count {
                    Some(row_start + header_row_count)
                } else {
                    None
                },
                bbox,
                markdown,
                cells,
            });
        }

        tables
    }
}

#[derive(Debug, Clone, Copy)]
struct TableComponent {
    row_start: usize,
    row_end: usize,
    col_start: usize,
    col_end: usize,
}

impl TableComponent {
    fn width(self) -> usize {
        self.col_end.saturating_sub(self.col_start) + 1
    }

    fn height(self) -> usize {
        self.row_end.saturating_sub(self.row_start) + 1
    }

    fn area(self) -> usize {
        self.width() * self.height()
    }
}

fn quantize_axis(mut coords: Vec<f32>, epsilon: f32) -> Vec<f32> {
    if coords.is_empty() {
        return Vec::new();
    }
    coords.sort_unstable_by(|left, right| left.total_cmp(right));
    let mut canonical = vec![coords[0]];
    for coord in coords.into_iter().skip(1) {
        let last = *canonical.last().unwrap_or(&coord);
        if (coord - last).abs() > epsilon {
            canonical.push(coord);
        }
    }
    canonical
}

fn find_nearest_axis(axis: &[f32], value: f32) -> usize {
    match axis.binary_search_by(|probe| probe.total_cmp(&value)) {
        Ok(index) => index,
        Err(index) => {
            if index == 0 {
                0
            } else if index >= axis.len() {
                axis.len().saturating_sub(1)
            } else {
                let left = (value - axis[index - 1]).abs();
                let right = (axis[index] - value).abs();
                if left <= right {
                    index - 1
                } else {
                    index
                }
            }
        }
    }
}

fn text_run_anchor(run: &PdfTextRun) -> Option<(f32, f32)> {
    let x = run.x?;
    let y = run.y?;
    let font_size = run.font_size.unwrap_or(12.0).max(1.0);
    Some((x, y - (font_size * 0.5)))
}

fn table_text_assignments_for_page(
    doc: &PdfDocument,
    page: &PdfPage,
    grid: &TableGrid,
) -> Vec<TableTextAssignment> {
    let mut assignments = Vec::new();
    for run in doc.semantic_text_runs_for_page(page) {
        let Some((x, y)) = text_run_anchor(&run) else {
            continue;
        };
        let Some((row, col)) = grid.route_point(x, y) else {
            continue;
        };
        if run.text.trim().is_empty() {
            continue;
        }
        assignments.push(TableTextAssignment {
            row,
            col,
            text: run.text,
            x,
            y,
            font_size: run.font_size,
        });
    }
    assignments
}

fn table_text_assignments_from_ocr(
    grid: &TableGrid,
    crop_box: [f32; 4],
    dpi: f32,
    ocr_runs: &[OcrTextRun],
) -> Vec<TableTextAssignment> {
    let mut assignments = Vec::new();
    for run in ocr_runs {
        if run.text.trim().is_empty() {
            continue;
        }
        let (pdf_x, pdf_y, pdf_font_size) = run.to_pdf_space(crop_box, dpi);
        let Some((row, col)) = grid.route_point(pdf_x, pdf_y) else {
            continue;
        };
        assignments.push(TableTextAssignment {
            row,
            col,
            text: run.text.clone(),
            x: pdf_x,
            y: pdf_y,
            font_size: Some(pdf_font_size),
        });
    }
    assignments
}

/// Columnar OCR → cell assignments. Walks Arrow columns without building
/// `Vec<OcrTextRun>`; still clones text into each routed assignment.
#[cfg(feature = "arrow")]
fn table_text_assignments_from_ocr_batch(
    grid: &TableGrid,
    crop_box: [f32; 4],
    dpi: f32,
    batch: &RecordBatch,
) -> Result<Vec<TableTextAssignment>> {
    let text_col = ocr_batch_required_column(batch, "text")?;
    let px_x_col = ocr_batch_required_column(batch, "px_x")?;
    let px_y_col = ocr_batch_required_column(batch, "px_y")?;
    let px_height_col = ocr_batch_required_column(batch, "px_height")?;

    let n = batch.num_rows();
    let mut assignments = Vec::new();
    for i in 0..n {
        if text_col.is_null(i)
            || px_x_col.is_null(i)
            || px_y_col.is_null(i)
            || px_height_col.is_null(i)
        {
            return Err(anyhow!(
                "ocr_runs Arrow row {i} has null in a required column \
                 (text/px_x/px_y/px_height)"
            ));
        }
        let text = ocr_batch_utf8_value(text_col, i)?;
        if text.trim().is_empty() {
            continue;
        }
        let px_x = ocr_batch_f32_value(px_x_col, i, "px_x")?;
        let px_y = ocr_batch_f32_value(px_y_col, i, "px_y")?;
        let px_height = ocr_batch_f32_value(px_height_col, i, "px_height")?;
        let (pdf_x, pdf_y, pdf_font_size) =
            ocr_px_to_pdf_space(px_x, px_y, px_height, crop_box, dpi);
        let Some((row, col)) = grid.route_point(pdf_x, pdf_y) else {
            continue;
        };
        assignments.push(TableTextAssignment {
            row,
            col,
            text,
            x: pdf_x,
            y: pdf_y,
            font_size: Some(pdf_font_size),
        });
    }
    Ok(assignments)
}

#[cfg(feature = "arrow")]
fn ocr_batch_required_column<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a Arc<dyn Array>> {
    batch.column_by_name(name).ok_or_else(|| {
        anyhow!(
            "ocr_runs Arrow batch missing required column '{name}' \
             (expected text, px_x, px_y, px_height)"
        )
    })
}

#[cfg(feature = "arrow")]
fn ocr_batch_utf8_value(array: &Arc<dyn Array>, index: usize) -> Result<String> {
    if let Some(arr) = array.as_any().downcast_ref::<StringArray>() {
        return Ok(arr.value(index).to_owned());
    }
    if let Some(arr) = array.as_any().downcast_ref::<LargeStringArray>() {
        return Ok(arr.value(index).to_owned());
    }
    Err(anyhow!(
        "ocr_runs column 'text' must be Utf8 or LargeUtf8, got {:?}",
        array.data_type()
    ))
}

#[cfg(feature = "arrow")]
fn ocr_batch_f32_value(array: &Arc<dyn Array>, index: usize, name: &str) -> Result<f32> {
    if let Some(arr) = array.as_any().downcast_ref::<Float32Array>() {
        return Ok(arr.value(index));
    }
    if let Some(arr) = array.as_any().downcast_ref::<Float64Array>() {
        return Ok(arr.value(index) as f32);
    }
    Err(anyhow!(
        "ocr_runs column '{name}' must be Float32 or Float64, got {:?}",
        array.data_type()
    ))
}

fn filter_table_rule_segments(segments: &[LineSegment], epsilon: f32) -> Vec<LineSegment> {
    if segments.is_empty() {
        return Vec::new();
    }

    let mut min_x = f32::MAX;
    let mut max_x = f32::MIN;
    let mut min_y = f32::MAX;
    let mut max_y = f32::MIN;
    for segment in segments {
        min_x = min_x.min(segment.x0.min(segment.x1));
        max_x = max_x.max(segment.x0.max(segment.x1));
        min_y = min_y.min(segment.y0.min(segment.y1));
        max_y = max_y.max(segment.y0.max(segment.y1));
    }
    let short_dim = (max_x - min_x).abs().min((max_y - min_y).abs()).max(1.0);
    let min_rule_length = (epsilon * 3.0).max(short_dim * 0.01);

    let mut horizontal = Vec::new();
    let mut vertical = Vec::new();
    for segment in segments {
        let dx = segment.x1 - segment.x0;
        let dy = segment.y1 - segment.y0;
        if dy.abs() <= epsilon {
            let x0 = segment.x0.min(segment.x1);
            let x1 = segment.x0.max(segment.x1);
            if (x1 - x0) >= min_rule_length {
                horizontal.push(LineSegment {
                    x0,
                    y0: (segment.y0 + segment.y1) * 0.5,
                    x1,
                    y1: (segment.y0 + segment.y1) * 0.5,
                });
            }
        } else if dx.abs() <= epsilon {
            let y0 = segment.y0.min(segment.y1);
            let y1 = segment.y0.max(segment.y1);
            if (y1 - y0) >= min_rule_length {
                vertical.push(LineSegment {
                    x0: (segment.x0 + segment.x1) * 0.5,
                    y0,
                    x1: (segment.x0 + segment.x1) * 0.5,
                    y1,
                });
            }
        }
    }

    let mut merged = merge_horizontal_rule_segments(horizontal, epsilon);
    merged.extend(merge_vertical_rule_segments(vertical, epsilon));
    merged
}

fn merge_horizontal_rule_segments(
    mut segments: Vec<LineSegment>,
    epsilon: f32,
) -> Vec<LineSegment> {
    if segments.is_empty() {
        return segments;
    }
    segments.sort_unstable_by(|left, right| {
        left.y0
            .total_cmp(&right.y0)
            .then(left.x0.total_cmp(&right.x0))
    });

    let mut merged: Vec<LineSegment> = Vec::with_capacity(segments.len());
    for segment in segments {
        if let Some(last) = merged.last_mut() {
            if (segment.y0 - last.y0).abs() <= epsilon && segment.x0 <= last.x1 + epsilon {
                last.x1 = last.x1.max(segment.x1);
                last.y0 = (last.y0 + segment.y0) * 0.5;
                last.y1 = last.y0;
                continue;
            }
        }
        merged.push(segment);
    }
    merged
}

fn merge_vertical_rule_segments(mut segments: Vec<LineSegment>, epsilon: f32) -> Vec<LineSegment> {
    if segments.is_empty() {
        return segments;
    }
    segments.sort_unstable_by(|left, right| {
        left.x0
            .total_cmp(&right.x0)
            .then(left.y0.total_cmp(&right.y0))
    });

    let mut merged: Vec<LineSegment> = Vec::with_capacity(segments.len());
    for segment in segments {
        if let Some(last) = merged.last_mut() {
            if (segment.x0 - last.x0).abs() <= epsilon && segment.y0 <= last.y1 + epsilon {
                last.y1 = last.y1.max(segment.y1);
                last.x0 = (last.x0 + segment.x0) * 0.5;
                last.x1 = last.x0;
                continue;
            }
        }
        merged.push(segment);
    }
    merged
}

fn adjacent_cell_groups(cells: &[TableCell]) -> Vec<Vec<usize>> {
    if cells.is_empty() {
        return Vec::new();
    }

    let mut by_col_start: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut by_col_end_next: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut by_row_start: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut by_row_end_next: HashMap<usize, Vec<usize>> = HashMap::new();

    for (index, cell) in cells.iter().enumerate() {
        by_col_start.entry(cell.col_start).or_default().push(index);
        by_col_end_next
            .entry(cell.col_end.saturating_add(1))
            .or_default()
            .push(index);
        by_row_start.entry(cell.row_start).or_default().push(index);
        by_row_end_next
            .entry(cell.row_end.saturating_add(1))
            .or_default()
            .push(index);
    }

    let mut visited = vec![false; cells.len()];
    let mut groups = Vec::new();

    for start_index in 0..cells.len() {
        if visited[start_index] {
            continue;
        }
        let mut stack = vec![start_index];
        let mut group = Vec::new();
        visited[start_index] = true;

        while let Some(current) = stack.pop() {
            group.push(current);
            let cell = &cells[current];

            if let Some(candidates) = by_col_start.get(&cell.col_end.saturating_add(1)) {
                push_adjacent_cells(cells, &mut visited, &mut stack, current, candidates);
            }
            if let Some(candidates) = by_col_end_next.get(&cell.col_start) {
                push_adjacent_cells(cells, &mut visited, &mut stack, current, candidates);
            }
            if let Some(candidates) = by_row_start.get(&cell.row_end.saturating_add(1)) {
                push_adjacent_cells(cells, &mut visited, &mut stack, current, candidates);
            }
            if let Some(candidates) = by_row_end_next.get(&cell.row_start) {
                push_adjacent_cells(cells, &mut visited, &mut stack, current, candidates);
            }
        }

        groups.push(group);
    }

    groups
}

fn push_adjacent_cells(
    cells: &[TableCell],
    visited: &mut [bool],
    stack: &mut Vec<usize>,
    current: usize,
    candidates: &[usize],
) {
    for &candidate in candidates {
        if visited[candidate] || candidate == current {
            continue;
        }
        if table_cells_adjacent(&cells[current], &cells[candidate]) {
            visited[candidate] = true;
            stack.push(candidate);
        }
    }
}

#[cfg(feature = "arrow")]
fn layout_table_indices(cells: &[TableCell]) -> Vec<usize> {
    let mut table_indices = vec![0; cells.len()];
    for (table_index, group) in adjacent_cell_groups(cells).into_iter().enumerate() {
        for index in group {
            table_indices[index] = table_index;
        }
    }
    table_indices
}

#[cfg(feature = "arrow")]
fn table_cell_rows_from_layout(page_index: usize, layout: TableLayout) -> Vec<TableCellRow> {
    let table_indices = layout_table_indices(&layout.cells);
    layout
        .cells
        .into_iter()
        .enumerate()
        .map(|(index, cell)| TableCellRow {
            page_index: page_index as u32,
            table_index: table_indices.get(index).copied().unwrap_or_default() as u32,
            row_start: cell.row_start as u32,
            row_end: cell.row_end as u32,
            col_start: cell.col_start as u32,
            col_end: cell.col_end as u32,
            row_span: (cell.row_end.saturating_sub(cell.row_start) + 1) as u32,
            col_span: (cell.col_end.saturating_sub(cell.col_start) + 1) as u32,
            bbox: cell.bbox,
            text: cell.text,
        })
        .collect()
}

#[cfg(feature = "arrow")]
fn table_rows_from_tables(page_index: usize, tables: Vec<PdfTable>) -> Vec<TableRow> {
    tables
        .into_iter()
        .map(|table| TableRow {
            page_index: page_index as u32,
            table_index: table.table_index as u32,
            row_start: table.row_start as u32,
            row_end: table.row_end as u32,
            col_start: table.col_start as u32,
            col_end: table.col_end as u32,
            row_count: table.row_count as u32,
            col_count: table.col_count as u32,
            cell_count: table.cell_count as u32,
            header_row_count: table.header_row_count as u32,
            body_row_start: table.body_row_start.map(|value| value as u32),
            bbox: table.bbox,
            markdown: table.markdown,
        })
        .collect()
}

fn table_cells_adjacent(left: &TableCell, right: &TableCell) -> bool {
    let row_overlap = left.row_start.max(right.row_start) <= left.row_end.min(right.row_end);
    let col_overlap = left.col_start.max(right.col_start) <= left.col_end.min(right.col_end);
    (left.col_end + 1 == right.col_start || right.col_end + 1 == left.col_start) && row_overlap
        || (left.row_end + 1 == right.row_start || right.row_end + 1 == left.row_start)
            && col_overlap
}

fn infer_table_header_row_count(
    cells: &[TableCell],
    table_row_start: usize,
    table_row_end: usize,
    table_col_start: usize,
    table_col_end: usize,
) -> usize {
    let top_row_cells = table_cells_for_row(cells, table_row_start);
    if top_row_cells.is_empty() {
        return 0;
    }
    if !table_row_has_text(&top_row_cells)
        || !table_row_covers_all_cols(&top_row_cells, table_col_start, table_col_end)
    {
        0
    } else {
        let mut header_row_count = 1;
        let mut current_row = table_row_start;
        let mut current_row_has_span = table_row_has_span(&top_row_cells);

        while current_row < table_row_end && current_row_has_span {
            let next_row = current_row + 1;
            if next_row >= table_row_end {
                break;
            }
            let next_row_cells = table_cells_for_row(cells, next_row);
            if next_row_cells.is_empty()
                || !table_row_has_text(&next_row_cells)
                || !table_row_covers_all_cols(&next_row_cells, table_col_start, table_col_end)
            {
                break;
            }
            header_row_count += 1;
            current_row = next_row;
            current_row_has_span = table_row_has_span(&next_row_cells);
        }

        header_row_count
    }
}

fn table_cells_for_row(cells: &[TableCell], row: usize) -> Vec<&TableCell> {
    cells
        .iter()
        .filter(|cell| cell.row_start <= row && cell.row_end >= row)
        .collect()
}

fn table_row_has_text(cells: &[&TableCell]) -> bool {
    cells.iter().any(|cell| !cell.text.trim().is_empty())
}

fn table_row_has_span(cells: &[&TableCell]) -> bool {
    cells
        .iter()
        .any(|cell| cell.col_end > cell.col_start || cell.row_end > cell.row_start)
}

fn table_row_covers_all_cols(
    cells: &[&TableCell],
    table_col_start: usize,
    table_col_end: usize,
) -> bool {
    spans_cover_range(
        &cells
            .iter()
            .map(|cell| (cell.col_start, cell.col_end))
            .collect::<Vec<_>>(),
        table_col_start,
        table_col_end,
    )
}

fn spans_cover_range(spans: &[(usize, usize)], start: usize, end: usize) -> bool {
    let mut covered = start;
    let mut ordered = spans.to_vec();
    ordered.sort_unstable_by_key(|span| (span.0, span.1));
    for (span_start, span_end) in ordered {
        if span_end < covered {
            continue;
        }
        if span_start > covered {
            return false;
        }
        covered = covered.max(span_end.saturating_add(1));
        if covered > end {
            return true;
        }
    }
    covered > end
}

fn table_markdown(
    cells: &[TableCell],
    row_start: usize,
    row_end: usize,
    col_start: usize,
    col_end: usize,
    header_row_count: usize,
) -> String {
    let row_count = row_end.saturating_sub(row_start) + 1;
    let col_count = col_end.saturating_sub(col_start) + 1;
    if row_count == 0 || col_count == 0 {
        return String::new();
    }

    let mut matrix = vec![vec![String::new(); col_count]; row_count];
    for cell in cells {
        let row = cell.row_start.saturating_sub(row_start);
        let col = cell.col_start.saturating_sub(col_start);
        if row < row_count && col < col_count {
            matrix[row][col] = markdown_cell_text(&cell.text);
        }
    }

    let header = if header_row_count > 0 {
        flatten_table_header(cells, row_start, col_start, col_count, header_row_count)
    } else {
        (0..col_count)
            .map(|index| format!("col_{}", index + 1))
            .collect::<Vec<_>>()
    };

    let mut lines = Vec::new();
    lines.push(format!("| {} |", header.join(" | ")));
    lines.push(format!(
        "| {} |",
        std::iter::repeat_n("---", col_count)
            .collect::<Vec<_>>()
            .join(" | ")
    ));

    let body_start = if header_row_count > 0 {
        header_row_count
    } else {
        0
    };
    for row in matrix.into_iter().skip(body_start) {
        lines.push(format!("| {} |", row.join(" | ")));
    }
    lines.join("\n")
}

fn flatten_table_header(
    cells: &[TableCell],
    row_start: usize,
    col_start: usize,
    col_count: usize,
    header_row_count: usize,
) -> Vec<String> {
    let row_end = row_start + header_row_count.saturating_sub(1);
    (0..col_count)
        .map(|offset| {
            let col = col_start + offset;
            let mut parts = Vec::new();
            for row in row_start..=row_end {
                if let Some(cell) = cells.iter().find(|cell| {
                    cell.row_start <= row
                        && cell.row_end >= row
                        && cell.col_start <= col
                        && cell.col_end >= col
                }) {
                    let text = markdown_cell_text(&cell.text);
                    if !text.is_empty() && parts.last() != Some(&text) {
                        parts.push(text);
                    }
                }
            }
            if parts.is_empty() {
                format!("col_{}", offset + 1)
            } else {
                parts.join(" / ")
            }
        })
        .collect()
}

fn markdown_cell_text(text: &str) -> String {
    text.replace('\n', " ")
        .replace('|', "\\|")
        .trim()
        .to_string()
}

#[cfg(feature = "arrow")]
fn normalize_page_bbox(media_box: Option<[f32; 4]>, runs: &[PdfTextRun]) -> [i32; 4] {
    let media_box = media_box.unwrap_or([0.0, 0.0, 1000.0, 1000.0]);
    let page_left = media_box[0];
    let page_bottom = media_box[1];
    let page_right = media_box[2];
    let page_top = media_box[3];
    let page_width = (page_right - page_left).abs().max(1.0);
    let page_height = (page_top - page_bottom).abs().max(1.0);

    let mut min_x = f32::MAX;
    let mut min_y = f32::MAX;
    let mut max_x = f32::MIN;
    let mut max_y = f32::MIN;

    for run in runs {
        let x = run.x.unwrap_or(0.0);
        let y = run.y.unwrap_or(0.0);
        let font_size = run.font_size.unwrap_or(12.0).max(1.0);
        let width = font_size * run.text.chars().count() as f32 * 0.6;
        min_x = min_x.min(x);
        min_y = min_y.min(y - font_size);
        max_x = max_x.max(x + width);
        max_y = max_y.max(y);
    }

    if !min_x.is_finite() || !min_y.is_finite() || !max_x.is_finite() || !max_y.is_finite() {
        return [0, 0, 0, 0];
    }

    [
        normalize_x(min_x, page_left, page_width),
        normalize_y(max_y, page_top, page_height),
        normalize_x(max_x, page_left, page_width),
        normalize_y(min_y, page_top, page_height),
    ]
}

#[cfg(feature = "arrow")]
fn normalize_x(value: f32, page_left: f32, page_width: f32) -> i32 {
    (((value - page_left) / page_width) * 1000.0)
        .floor()
        .clamp(0.0, 1000.0) as i32
}

#[cfg(feature = "arrow")]
fn normalize_y(value: f32, page_top: f32, page_height: f32) -> i32 {
    (((page_top - value) / page_height) * 1000.0)
        .floor()
        .clamp(0.0, 1000.0) as i32
}

#[cfg(feature = "rasterize")]
fn pdf_color(red: f32, green: f32, blue: f32) -> Color {
    Color::from_rgba(
        red.clamp(0.0, 1.0),
        green.clamp(0.0, 1.0),
        blue.clamp(0.0, 1.0),
        1.0,
    )
    .unwrap_or(Color::BLACK)
}

fn pdf_value_rect(value: &PdfValue) -> Option<[f32; 4]> {
    let PdfValue::Array(values) = value else {
        return None;
    };
    let numbers = values
        .iter()
        .filter_map(pdf_value_number)
        .collect::<Vec<_>>();
    let slice = numbers.get(0..4)?;
    Some([slice[0], slice[1], slice[2], slice[3]])
}

fn pdf_value_matrix(value: &PdfValue) -> Option<ContentMatrix> {
    let PdfValue::Array(values) = value else {
        return None;
    };
    if values.len() != 6 {
        return None;
    }
    let mut matrix = [0.0f32; 6];
    for (index, value) in values.iter().enumerate() {
        matrix[index] = pdf_value_number(value)?;
    }
    Some(matrix)
}

fn pdf_value_number(value: &PdfValue) -> Option<f32> {
    match value {
        PdfValue::Integer(value) => Some(*value as f32),
        PdfValue::Real(value) => Some(*value as f32),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy)]
struct ObjectHeaderMatch {
    start: usize,
    header_end: usize,
    object_number: u32,
    generation: u16,
}

// Per-document scan state is threaded explicitly so parsing stays allocation-
// conscious and does not hide mutable global state behind a context object.
#[allow(clippy::too_many_arguments)]
fn scan_object(
    bytes: &[u8],
    header: &ObjectHeaderMatch,
    options: ParseOptions,
    streams: &mut Vec<PdfStream>,
    text_fragments: &mut Vec<PdfTextFragment>,
    cmaps: &mut Vec<PdfCMap>,
    warnings: &mut Vec<PdfWarning>,
    indirect_integers: &HashMap<(u32, u16), i64>,
    decoded_byte_budget: &DecodedByteBudget,
) -> Result<PdfObject> {
    let (dictionary_range, dictionary) = find_dictionary(bytes, header.header_end)
        .map(|(range, dict)| (Some(range), Some(dict)))
        .unwrap_or((None, None));
    // Only one of the two can be present for a well-formed object, and a
    // dictionary with a stream after it is the common case; the array is read
    // only when there is no dictionary to read.
    let array = if dictionary.is_none() {
        find_array(bytes, header.header_end).map(|(_, values)| values)
    } else {
        None
    };

    let (stream_index, span_end) = if let Some((stream, endobj_end)) = scan_stream(
        bytes,
        header,
        dictionary.as_ref(),
        dictionary_range,
        options,
        streams.len(),
        text_fragments,
        cmaps,
        warnings,
        indirect_integers,
        decoded_byte_budget,
    )? {
        streams.push(stream);
        (Some(streams.len() - 1), endobj_end)
    } else {
        (
            None,
            find_endobj(bytes, header.header_end).unwrap_or(bytes.len()),
        )
    };

    Ok(PdfObject {
        object_number: header.object_number,
        generation: header.generation,
        span: ByteRange {
            start: header.start,
            end: span_end,
        },
        dictionary_range,
        dictionary,
        array,
        stream_index,
    })
}

// Low-level PDF stream boundary + decode routine; the argument list mirrors
// the parser's per-object scan state (bytes, header, dict, options, sinks).
#[allow(clippy::too_many_arguments)]
fn scan_stream(
    bytes: &[u8],
    header: &ObjectHeaderMatch,
    dictionary: Option<&PdfDictionary>,
    dictionary_range: Option<ByteRange>,
    options: ParseOptions,
    _stream_index: usize,
    _text_fragments: &mut Vec<PdfTextFragment>,
    cmaps: &mut Vec<PdfCMap>,
    warnings: &mut Vec<PdfWarning>,
    indirect_integers: &HashMap<(u32, u16), i64>,
    decoded_byte_budget: &DecodedByteBudget,
) -> Result<Option<(PdfStream, usize)>> {
    let Some(dict_range) = dictionary_range else {
        return Ok(None);
    };
    let mut cursor = dict_range.end;
    skip_whitespace_and_comments(bytes, &mut cursor);
    if !starts_with_keyword(bytes, cursor, b"stream") {
        return Ok(None);
    }
    cursor += "stream".len();
    let raw_start = skip_stream_eol(bytes, cursor);
    let length_hint = dictionary.and_then(|dict| resolve_stream_length(dict, indirect_integers));
    let Some((raw_end, endstream_end, resynced, ended_at_endobj)) =
        find_stream_end(bytes, raw_start, length_hint)
    else {
        warnings.push(PdfWarning {
            object_number: Some(header.object_number),
            generation: Some(header.generation),
            message: "stream started but endstream was not found".to_string(),
        });
        return Ok(None);
    };
    if resynced {
        warnings.push(PdfWarning {
            object_number: Some(header.object_number),
            generation: Some(header.generation),
            message: "stream boundaries recovered via sentinel resync".to_string(),
        });
    }
    let endobj_end = if ended_at_endobj {
        endstream_end
    } else {
        find_endobj(bytes, endstream_end).unwrap_or(endstream_end)
    };
    let raw_range = ByteRange {
        start: raw_start,
        end: raw_end,
    };
    let raw = &bytes[raw_start..raw_end];
    let filters = dictionary
        .map(|dict| dict.get_name_array("Filter"))
        .unwrap_or_default();
    let mut kind = classify_stream_kind_from_dictionary(dictionary);
    let mut decoded: Option<Vec<u8>> = None;
    let decode_status = if filters.is_empty() || is_passthrough_filter_chain(&filters) {
        // No decode work: empty filter chain or an identity/passthrough chain.
        DecodeStatus::Raw
    } else {
        let decode_parms =
            dictionary.and_then(|dict| dict.get("DecodeParms").or_else(|| dict.get("DP")));
        match decoded_byte_budget.decode_filters(raw, &filters, decode_parms) {
            Ok(bytes) => {
                if options.keep_decoded_streams
                    || matches!(
                        kind,
                        PdfStreamKind::Content | PdfStreamKind::CMap | PdfStreamKind::ObjectStream
                    )
                {
                    // Retain decoded bytes: caller asked to keep all, or this is
                    // a structurally-required stream kind.
                    decoded = Some(bytes);
                } else if kind == PdfStreamKind::Unknown {
                    let sniffed = classify_stream_kind(dictionary, &bytes);
                    if sniffed != PdfStreamKind::Unknown {
                        kind = sniffed;
                    }
                    if matches!(kind, PdfStreamKind::Content | PdfStreamKind::CMap) {
                        decoded = Some(bytes);
                    }
                }
                DecodeStatus::Decoded
            }
            Err(err) if is_decode_budget_exceeded(&err) => return Err(err),
            Err(err) => DecodeStatus::Failed(err.to_string()),
        }
    };

    let decoded_ref = match decoded.as_deref() {
        Some(bytes) => Some(bytes),
        None if filters.is_empty() => Some(raw),
        _ => None,
    };

    if let Some(decoded_bytes) = decoded_ref {
        if matches!(kind, PdfStreamKind::Unknown | PdfStreamKind::Metadata) {
            let decoded_kind = classify_stream_kind(dictionary, decoded_bytes);
            if decoded_kind != PdfStreamKind::Unknown {
                kind = decoded_kind;
            }
        }
        if (options.extract_cmaps || options.extract_text) && kind == PdfStreamKind::CMap {
            if let Some(cmap) = parse_cmap(decoded_bytes, header.object_number, header.generation) {
                cmaps.push(cmap);
            }
        }
    }

    let image = classify_image_info(dictionary, raw);
    let font_program = classify_font_program_info(dictionary, raw);
    let decoded_len = decoded_ref.map(|bytes| bytes.len());
    let decoded_preview = decoded_ref
        .filter(|bytes| is_textish(bytes))
        .map(|bytes| preview_text(bytes, 240));
    let retained = if options.keep_decoded_streams
        || matches!(
            kind,
            PdfStreamKind::Content | PdfStreamKind::CMap | PdfStreamKind::ObjectStream
        ) {
        decoded
    } else {
        None
    };

    Ok(Some((
        PdfStream {
            object_number: header.object_number,
            generation: header.generation,
            raw_range,
            raw_len: raw_range.len(),
            decoded_len,
            filters,
            kind,
            decode_status,
            image,
            font_program,
            decoded_preview,
            decoded: retained,
        },
        endobj_end,
    )))
}

fn classify_stream_kind(dictionary: Option<&PdfDictionary>, raw: &[u8]) -> PdfStreamKind {
    let kind = classify_stream_kind_from_dictionary(dictionary);
    if kind != PdfStreamKind::Unknown {
        return kind;
    }
    sniff_stream_kind(raw)
}

fn classify_stream_kind_from_dictionary(dictionary: Option<&PdfDictionary>) -> PdfStreamKind {
    if let Some(dict) = dictionary {
        match (dict.get_name("Type"), dict.get_name("Subtype")) {
            (Some("XRef"), _) => return PdfStreamKind::XRef,
            (Some("ObjStm"), _) => return PdfStreamKind::ObjectStream,
            (_, Some("Image")) => return PdfStreamKind::Image,
            (Some("Metadata"), _) => return PdfStreamKind::Metadata,
            (Some("CMap"), _) => return PdfStreamKind::CMap,
            _ => {}
        }
        if matches!(
            dict.get_name("Subtype"),
            Some("Type1C" | "CIDFontType0C" | "OpenType")
        ) || dict.get("FontFile").is_some()
            || dict.get("FontFile2").is_some()
            || dict.get("FontFile3").is_some()
        {
            return PdfStreamKind::FontProgram;
        }
    }
    PdfStreamKind::Unknown
}

fn sniff_stream_kind(raw: &[u8]) -> PdfStreamKind {
    // We cannot restrict this to 8KB.
    // Vector-heavy documents (like McKinsey PDFs) routinely have 500KB of pure vector paths
    // followed by `BT ... Tj` text instructions at the very end of the stream!
    let sample = raw;
    if memmem::find(sample, b"begincmap").is_some()
        || memmem::find(sample, b"begincodespacerange").is_some()
        || memmem::find(sample, b"/CMapName").is_some()
    {
        return PdfStreamKind::CMap;
    }
    if memmem::find(sample, b"BT").is_some()
        && (memmem::find(sample, b"Tj").is_some()
            || memmem::find(sample, b"TJ").is_some()
            || memmem::find(sample, b"Tf").is_some())
    {
        return PdfStreamKind::Content;
    }
    PdfStreamKind::Unknown
}

fn classify_image_info(dictionary: Option<&PdfDictionary>, raw: &[u8]) -> Option<PdfImageInfo> {
    let dict = dictionary?;
    if dict.get_name("Subtype") != Some("Image") {
        return None;
    }
    let filters = dict.get_name_array("Filter");
    Some(PdfImageInfo {
        width: dict.get_integer("Width"),
        height: dict.get_integer("Height"),
        bits_per_component: dict.get_integer("BitsPerComponent"),
        color_space: dict.get_name("ColorSpace").map(str::to_string),
        format_hint: image_magic(raw)
            .map(str::to_string)
            .or_else(|| image_filter_format_hint(&filters).map(str::to_string)),
        decode_parms: image_decode_parms(dict, &filters),
    })
}

fn image_filter_format_hint(filters: &[String]) -> Option<&'static str> {
    filters.iter().find_map(|filter| match filter.as_str() {
        "DCTDecode" | "DCT" => Some("jpeg"),
        "JPXDecode" => Some("jp2"),
        "JBIG2Decode" => Some("jbig2"),
        "CCITTFaxDecode" | "CCF" => Some("ccitt"),
        _ => None,
    })
}

fn image_decode_parms(dict: &PdfDictionary, filters: &[String]) -> Option<PdfDictionary> {
    let parms = dict.get("DecodeParms").or_else(|| dict.get("DP"))?;
    match parms {
        PdfValue::Dictionary(dict) => Some(dict.clone()),
        PdfValue::Array(values) => {
            let ccitt_index = filters
                .iter()
                .position(|filter| is_ccitt_filter(filter.as_str()));
            ccitt_index
                .and_then(|index| values.get(index))
                .and_then(pdf_value_as_dictionary)
                .cloned()
                .or_else(|| values.iter().find_map(pdf_value_as_dictionary).cloned())
        }
        _ => None,
    }
}

fn pdf_value_as_dictionary(value: &PdfValue) -> Option<&PdfDictionary> {
    match value {
        PdfValue::Dictionary(dict) => Some(dict),
        _ => None,
    }
}

#[cfg(feature = "rasterize")]
fn decode_ccitt_image_payload(
    raw: &[u8],
    filters: &[String],
    image: &PdfImageInfo,
    decoded_byte_budget: &DecodedByteBudget,
) -> Result<Vec<u8>> {
    let Some(ccitt_index) = filters
        .iter()
        .position(|filter| is_ccitt_filter(filter.as_str()))
    else {
        return Err(anyhow!("CCITT filter not present"));
    };
    if ccitt_index + 1 != filters.len() {
        return Err(anyhow!("CCITT filter must be terminal"));
    }
    let input = if ccitt_index == 0 {
        std::borrow::Cow::Borrowed(raw)
    } else {
        std::borrow::Cow::Owned(decoded_byte_budget.decode_filters(
            raw,
            &filters[..ccitt_index],
            None,
        )?)
    };
    ccitt::decode_ccitt_to_png(input.as_ref(), image)
}

#[cfg(feature = "rasterize")]
fn decode_ccitt_luma_payload(
    raw: &[u8],
    filters: &[String],
    image: &PdfImageInfo,
    decoded_byte_budget: &DecodedByteBudget,
) -> Result<ccitt::CcittLumaImage> {
    let Some(ccitt_index) = filters
        .iter()
        .position(|filter| is_ccitt_filter(filter.as_str()))
    else {
        return Err(anyhow!("CCITT filter not present"));
    };
    if ccitt_index + 1 != filters.len() {
        return Err(anyhow!("CCITT filter must be terminal"));
    }
    let input = if ccitt_index == 0 {
        std::borrow::Cow::Borrowed(raw)
    } else {
        std::borrow::Cow::Owned(decoded_byte_budget.decode_filters(
            raw,
            &filters[..ccitt_index],
            None,
        )?)
    };
    ccitt::decode_ccitt_to_luma(input.as_ref(), image)
}

fn classify_font_program_info(
    dictionary: Option<&PdfDictionary>,
    raw: &[u8],
) -> Option<PdfFontProgramInfo> {
    let dict = dictionary?;
    let subtype = dict.get_name("Subtype").map(str::to_string);
    let hinted = subtype
        .clone()
        .or_else(|| font_magic(raw).map(str::to_string));
    if hinted.is_none()
        && dict.get("FontFile").is_none()
        && dict.get("FontFile2").is_none()
        && dict.get("FontFile3").is_none()
    {
        return None;
    }
    Some(PdfFontProgramInfo {
        subtype,
        format_hint: font_magic(raw).map(str::to_string),
    })
}

fn find_pdf_header(bytes: &[u8]) -> Option<String> {
    let index = memmem::find(bytes, b"%PDF-")?;
    let end = bytes[index..]
        .iter()
        .position(|byte| *byte == b'\n' || *byte == b'\r')
        .map(|offset| index + offset)
        .unwrap_or(bytes.len());
    String::from_utf8(bytes[index..end].to_vec()).ok()
}

fn collect_indirect_integers(
    bytes: &[u8],
    headers: &[ObjectHeaderMatch],
) -> HashMap<(u32, u16), i64> {
    headers
        .iter()
        .filter_map(|header| {
            // Only an integer object is wanted, so anything that does not
            // start like a number is skipped without parsing it. Parsing a
            // whole array or dictionary here, once per object header, made
            // the scan quadratic in the document size.
            let mut cursor = header.header_end;
            skip_whitespace_and_comments(bytes, &mut cursor);
            if !matches!(bytes.get(cursor), Some(b'+' | b'-' | b'.' | b'0'..=b'9')) {
                return None;
            }
            let mut parser = ValueParser::new(bytes, cursor);
            match parser.parse_value().ok()? {
                PdfValue::Integer(value) => {
                    Some(((header.object_number, header.generation), value))
                }
                _ => None,
            }
        })
        .collect()
}

/// Normalize a page `/Rotate` to one of `0`, `90`, `180`, `270`.
///
/// PDF 32000-1 §7.7.3.3 requires a multiple of 90. Out-of-range multiples are
/// folded (`-90` → `270`, `450` → `90`, `360` → `0`), which every viewer does
/// and which is unambiguous. A value that is not a multiple of 90 has no
/// correct reading, so it is refused: the alternative is to round it, and a
/// page silently rotated the wrong way is a plausible-looking wrong answer
/// rather than a visible failure.
fn normalize_rotate(rotate: i64) -> Result<u32> {
    if rotate % 90 != 0 {
        return Err(anyhow!(
            "unsupported PDF page rotation {rotate}: /Rotate must be a multiple of 90"
        ));
    }
    Ok((rotate.rem_euclid(360) as u32 / 90) * 90)
}

/// A PDF number as `f32`.
///
/// Integers and reals arrive as distinct variants; anything else — a name, a
/// string, a nested array — is not a number and yields `None` so the caller can
/// decide what a missing value means.
fn pdf_value_as_f32(value: &PdfValue) -> Option<f32> {
    match value {
        PdfValue::Integer(n) => Some(*n as f32),
        PdfValue::Real(n) => Some(*n as f32),
        _ => None,
    }
}

fn resolve_stream_length(
    dictionary: &PdfDictionary,
    indirect_integers: &HashMap<(u32, u16), i64>,
) -> Option<i64> {
    match dictionary.get("Length")? {
        PdfValue::Integer(value) => Some(*value),
        PdfValue::Reference {
            object_number,
            generation,
        } => indirect_integers
            .get(&(*object_number, *generation))
            .copied(),
        _ => None,
    }
}

fn find_all_object_headers(bytes: &[u8], max_objects: usize) -> Vec<ObjectHeaderMatch> {
    #[cfg(feature = "parallel")]
    let mut headers = find_obj_markers_simd(bytes)
        .into_par_iter()
        .filter_map(|obj_start| parse_object_header_from_obj_marker(bytes, obj_start))
        .collect::<Vec<_>>();

    #[cfg(not(feature = "parallel"))]
    let mut headers = find_obj_markers_simd(bytes)
        .into_iter()
        .filter_map(|obj_start| parse_object_header_from_obj_marker(bytes, obj_start))
        .collect::<Vec<_>>();

    headers.sort_unstable_by_key(|header| header.start);
    headers.dedup_by_key(|header| header.start);
    if headers.len() > max_objects {
        headers.truncate(max_objects);
    }
    headers
}

/// Find byte offsets of `N G obj` markers using a SIMD scan on supported
/// targets, falling back to a scalar loop elsewhere.
pub fn find_obj_markers_simd(bytes: &[u8]) -> Vec<usize> {
    let mut offsets = Vec::new();
    #[cfg(target_arch = "x86_64")]
    let mut cursor = find_obj_markers_simd_x86_64(bytes, &mut offsets);

    #[cfg(all(not(target_arch = "x86_64"), target_arch = "aarch64"))]
    let mut cursor = find_obj_markers_simd_aarch64(bytes, &mut offsets);

    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    let mut cursor = 0usize;

    let finder = memmem::Finder::new(b"obj");
    while cursor < bytes.len() {
        let Some(relative) = finder.find(&bytes[cursor..]) else {
            break;
        };
        let pos = cursor + relative;
        offsets.push(pos);
        cursor = pos + 3;
    }

    offsets.sort_unstable();
    offsets.dedup();
    offsets
}

fn parse_object_header_from_obj_marker(
    bytes: &[u8],
    obj_start: usize,
) -> Option<ObjectHeaderMatch> {
    let after = obj_start + 3;
    if after < bytes.len() && !is_delimiter(bytes[after]) {
        return None;
    }

    let (generation, generation_start) = reverse_parse_unsigned_u16(bytes, obj_start)?;
    let object_gap = generation_start.checked_sub(1)?;
    if !bytes[object_gap].is_ascii_whitespace() {
        return None;
    }
    let (object_number, object_start) = reverse_parse_unsigned_u32(bytes, object_gap)?;
    if object_start > 0 && !is_delimiter(bytes[object_start - 1]) {
        return None;
    }

    Some(ObjectHeaderMatch {
        start: object_start,
        header_end: after,
        object_number,
        generation,
    })
}

#[cfg(target_arch = "x86_64")]
fn find_obj_markers_simd_x86_64(bytes: &[u8], offsets: &mut Vec<usize>) -> usize {
    use std::arch::x86_64::{
        __m128i, _mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8, _mm_set1_epi8,
    };

    let mut cursor = 0usize;
    // SAFETY: the intrinsic needs SSE2, which is part of the x86_64 baseline
    // this function is compiled for. It touches no memory.
    let target = unsafe { _mm_set1_epi8(b'o' as i8) };
    while cursor + 16 <= bytes.len() {
        // SAFETY: SSE2 is baseline on x86_64. The loop condition
        // `cursor + 16 <= bytes.len()` keeps the 16 bytes read from
        // `bytes[cursor..]` inside the slice, and `_mm_loadu_si128` is the
        // unaligned load, so the cast pointer needs no alignment.
        let chunk = unsafe { _mm_loadu_si128(bytes[cursor..].as_ptr() as *const __m128i) };
        // SAFETY: SSE2 is baseline on x86_64; register-only, no memory access.
        let matches = unsafe { _mm_cmpeq_epi8(chunk, target) };
        // SAFETY: SSE2 is baseline on x86_64; register-only, no memory access.
        let mut bitmask = unsafe { _mm_movemask_epi8(matches) } as u32;
        while bitmask != 0 {
            let trailing = bitmask.trailing_zeros() as usize;
            let pos = cursor + trailing;
            if pos + 2 < bytes.len() && &bytes[pos..pos + 3] == b"obj" {
                offsets.push(pos);
            }
            bitmask &= bitmask - 1;
        }
        cursor += 16;
    }
    cursor
}

#[cfg(target_arch = "aarch64")]
fn find_obj_markers_simd_aarch64(bytes: &[u8], offsets: &mut Vec<usize>) -> usize {
    use std::arch::aarch64::{uint8x16_t, vceqq_u8, vdupq_n_u8, vld1q_u8, vst1q_u8};

    let mut cursor = 0usize;
    // SAFETY: the intrinsic needs NEON, which is part of the aarch64 baseline
    // this function is compiled for. It touches no memory.
    let target = unsafe { vdupq_n_u8(b'o') };
    while cursor + 16 <= bytes.len() {
        // SAFETY: NEON is baseline on aarch64. The loop condition
        // `cursor + 16 <= bytes.len()` keeps the 16 bytes read from
        // `bytes[cursor..]` inside the slice; `vld1q_u8` has no alignment
        // requirement.
        let chunk = unsafe { vld1q_u8(bytes[cursor..].as_ptr()) };
        // SAFETY: NEON is baseline on aarch64; register-only, no memory access.
        let matches: uint8x16_t = unsafe { vceqq_u8(chunk, target) };
        let mut lanes = [0u8; 16];
        // SAFETY: NEON is baseline on aarch64. `lanes` is a live, writable
        // `[u8; 16]`, exactly the 16 bytes `vst1q_u8` stores, and the store
        // has no alignment requirement.
        unsafe { vst1q_u8(lanes.as_mut_ptr(), matches) };
        let mut bitmask = 0u32;
        for (index, lane) in lanes.iter().enumerate() {
            if *lane != 0 {
                bitmask |= 1 << index;
            }
        }
        while bitmask != 0 {
            let trailing = bitmask.trailing_zeros() as usize;
            let pos = cursor + trailing;
            if pos + 2 < bytes.len() && &bytes[pos..pos + 3] == b"obj" {
                offsets.push(pos);
            }
            bitmask &= bitmask - 1;
        }
        cursor += 16;
    }
    cursor
}

fn find_dictionary(bytes: &[u8], start: usize) -> Option<(ByteRange, PdfDictionary)> {
    let mut cursor = start;
    skip_whitespace_and_comments(bytes, &mut cursor);
    if bytes.get(cursor..cursor + 2)? != b"<<" {
        return None;
    }
    let mut parser = ValueParser::new(bytes, cursor);
    match parser.parse_value().ok()? {
        PdfValue::Dictionary(dictionary) => Some((
            ByteRange {
                start: cursor,
                end: parser.pos,
            },
            dictionary,
        )),
        _ => None,
    }
}

/// Parse a leading array, when the indirect object is one.
///
/// An indirect object is normally a dictionary, but a producer that shares or
/// sizes a long array writes the array itself as the object — `/Widths` across
/// subset fonts is the case this parser reads. Only dictionaries used to be
/// kept, so such a reference resolved to nothing, which for a font means no
/// advance widths.
///
/// Kept separate from [`find_dictionary`] rather than folded into one "parse any
/// value" helper, so the dictionary path stays byte-for-byte what it was and
/// cannot perturb object recovery, which everything downstream depends on.
fn find_array(bytes: &[u8], start: usize) -> Option<(ByteRange, Vec<PdfValue>)> {
    let mut cursor = start;
    skip_whitespace_and_comments(bytes, &mut cursor);
    if bytes.get(cursor) != Some(&b'[') {
        return None;
    }
    let mut parser = ValueParser::new(bytes, cursor);
    match parser.parse_value().ok()? {
        PdfValue::Array(values) => Some((
            ByteRange {
                start: cursor,
                end: parser.pos,
            },
            values,
        )),
        _ => None,
    }
}

fn find_stream_end(
    bytes: &[u8],
    raw_start: usize,
    length_hint: Option<i64>,
) -> Option<(usize, usize, bool, bool)> {
    if let Some(length) = length_hint.filter(|length| *length >= 0) {
        let length = length as usize;
        let candidate_end = raw_start.checked_add(length)?;
        let after_candidate = skip_optional_eol(bytes, candidate_end);
        if starts_with_keyword(bytes, after_candidate, b"endstream") {
            return Some((
                candidate_end,
                after_candidate + "endstream".len(),
                false,
                false,
            ));
        }
    }

    if let Some(relative) = memmem::find(&bytes[raw_start..], b"endstream") {
        let endstream_start = raw_start + relative;
        let raw_end = trim_eol_backwards(bytes, endstream_start, raw_start);
        return Some((raw_end, endstream_start + "endstream".len(), false, false));
    }

    let sentinel = find_next_sentinel(bytes, raw_start)?;
    let raw_end = trim_eol_backwards(bytes, sentinel.start, raw_start);
    Some((
        raw_end,
        sentinel.end,
        true,
        sentinel.kind == SentinelKind::EndObj,
    ))
}

fn find_endobj(bytes: &[u8], start: usize) -> Option<usize> {
    let relative = memmem::find(&bytes[start..], b"endobj")?;
    Some(start + relative + "endobj".len())
}

#[derive(Debug, Clone, Copy)]
struct SentinelMatch {
    start: usize,
    end: usize,
    kind: SentinelKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SentinelKind {
    EndStream,
    EndObj,
}

fn find_next_sentinel(bytes: &[u8], start: usize) -> Option<SentinelMatch> {
    let endstream = memmem::find(&bytes[start..], b"endstream").map(|offset| SentinelMatch {
        start: start + offset,
        end: start + offset + "endstream".len(),
        kind: SentinelKind::EndStream,
    });
    let endobj = memmem::find(&bytes[start..], b"endobj").map(|offset| SentinelMatch {
        start: start + offset,
        end: start + offset + "endobj".len(),
        kind: SentinelKind::EndObj,
    });

    match (endstream, endobj) {
        (Some(left), Some(right)) => Some(if left.start <= right.start {
            left
        } else {
            right
        }),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

fn skip_stream_eol(bytes: &[u8], cursor: usize) -> usize {
    match (bytes.get(cursor), bytes.get(cursor + 1)) {
        (Some(b'\r'), Some(b'\n')) => cursor + 2,
        (Some(b'\n'), _) | (Some(b'\r'), _) => cursor + 1,
        _ => cursor,
    }
}

fn skip_optional_eol(bytes: &[u8], cursor: usize) -> usize {
    match (bytes.get(cursor), bytes.get(cursor + 1)) {
        (Some(b'\r'), Some(b'\n')) => cursor + 2,
        (Some(b'\n'), _) | (Some(b'\r'), _) => cursor + 1,
        _ => cursor,
    }
}

fn trim_eol_backwards(bytes: &[u8], mut end: usize, floor: usize) -> usize {
    while end > floor && matches!(bytes[end - 1], b'\r' | b'\n') {
        end -= 1;
    }
    end
}

fn starts_with_keyword(bytes: &[u8], start: usize, keyword: &[u8]) -> bool {
    let Some(slice) = bytes.get(start..start + keyword.len()) else {
        return false;
    };
    if slice != keyword {
        return false;
    }
    let before_ok = start == 0 || is_delimiter(bytes[start - 1]);
    let after = start + keyword.len();
    let after_ok = after >= bytes.len() || is_delimiter(bytes[after]);
    before_ok && after_ok
}

fn collect_references(value: &PdfValue) -> Vec<(u32, u16)> {
    match value {
        PdfValue::Reference {
            object_number,
            generation,
        } => vec![(*object_number, *generation)],
        PdfValue::Array(values) => values.iter().flat_map(collect_references).collect(),
        _ => Vec::new(),
    }
}

fn reverse_parse_unsigned_u16(bytes: &[u8], end: usize) -> Option<(u16, usize)> {
    let (value, start) = reverse_parse_ascii_digits(bytes, end, 5)?;
    let value = u16::try_from(value).ok()?;
    Some((value, start))
}

fn reverse_parse_unsigned_u32(bytes: &[u8], end: usize) -> Option<(u32, usize)> {
    let (value, start) = reverse_parse_ascii_digits(bytes, end, 10)?;
    let value = u32::try_from(value).ok()?;
    Some((value, start))
}

fn reverse_parse_ascii_digits(bytes: &[u8], end: usize, max_digits: usize) -> Option<(u64, usize)> {
    if end == 0 {
        return None;
    }
    let mut cursor = end;
    while cursor > 0 && bytes[cursor - 1].is_ascii_whitespace() {
        cursor -= 1;
    }
    let mut start = cursor;
    let mut digits = 0usize;
    while start > 0 && bytes[start - 1].is_ascii_digit() && digits < max_digits {
        start -= 1;
        digits += 1;
    }
    if digits == 0 {
        return None;
    }

    let mut value = 0u64;
    for &byte in &bytes[start..cursor] {
        value = value.checked_mul(10)?.checked_add((byte - b'0') as u64)?;
    }
    Some((value, start))
}

/// Work the cross-reference chain may do, per byte of file.
///
/// One unit is one entry examined; a section costs [`XREF_SECTION_COST`].
/// Sections may not overlap, and the shortest table entry (`0 0n` and a line
/// end) is five bytes, so every table in a file together holds fewer
/// entries than a fifth of its length. A cross-reference stream is refused
/// past [`XREF_DECODE_MULTIPLE`] times the file length decoded, one row per
/// byte at the least, and each is read once. Eight units a byte covers both
/// with room to spare; a chain that spends it is abandoned and the scan's
/// own answer stands.
const XREF_WORK_PER_BYTE: usize = 8;

/// Work charged for entering one cross-reference section.
const XREF_SECTION_COST: usize = 64;

/// Largest decoded cross-reference stream accepted, as a multiple of the
/// file length. A real one is far smaller than its file; a hostile one can
/// inflate megabytes of rows from a few bytes.
const XREF_DECODE_MULTIPLE: usize = 4;

/// Where the cross-reference chain says an in-use object is.
#[derive(Clone, Copy)]
enum XrefEntry {
    /// At a byte offset in the file, with this generation.
    At(usize, u16),
    /// Inside the object stream with this object number.
    Packed(u32),
}

/// The in-use entries read so far and the work spent reading them.
struct XrefChain<'a> {
    bytes: &'a [u8],
    /// Object numbers the scan produced. No entry is kept for any other.
    known: &'a HashSet<u32>,
    /// The newest in-use entry for each known object.
    entries: HashMap<u32, XrefEntry>,
    work: usize,
    budget: usize,
}

impl XrefChain<'_> {
    /// Spends `units` of work; `None` once the budget is gone.
    fn charge(&mut self, units: usize) -> Option<()> {
        self.work = self.work.saturating_add(units);
        (self.work <= self.budget).then_some(())
    }

    /// Records an in-use entry unless a newer section already placed the
    /// object or the scan never saw it.
    fn offer(&mut self, number: u32, entry: XrefEntry) -> Option<()> {
        self.charge(1)?;
        if self.known.contains(&number) {
            self.entries.entry(number).or_insert(entry);
        }
        Some(())
    }

    /// Reads the classic table at `offset`. Returns its trailer and where
    /// the section ends. Free entries are read past and not recorded.
    ///
    /// The format accepted is the renderer's (hayro-syntax 0.5.0,
    /// `xref.rs` 762-786 and `XRefEntry::read` at 670-692): after each
    /// `first count` header, exactly twenty bytes per entry, ten digits of
    /// offset, a separator, five digits of generation, a separator and `n`
    /// for in use. Anything else is `None`, as it is there.
    fn read_table(&mut self, offset: usize) -> Option<(PdfDictionary, usize)> {
        let bytes = self.bytes;
        let blanks = |cursor: &mut usize| {
            while bytes
                .get(*cursor)
                .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | 0x0c | 0))
            {
                *cursor += 1;
            }
        };
        let digits = |field: &[u8]| -> Option<usize> {
            field.iter().try_fold(0usize, |value, byte| {
                byte.is_ascii_digit()
                    .then(|| {
                        value
                            .checked_mul(10)?
                            .checked_add(usize::from(*byte - b'0'))
                    })
                    .flatten()
            })
        };
        let mut cursor = offset.checked_add(4)?;
        loop {
            self.charge(1)?;
            blanks(&mut cursor);
            if bytes.get(cursor..)?.starts_with(b"trailer") {
                cursor = cursor.checked_add(7)?;
                break;
            }
            let mut parser = ValueParser::new(bytes, cursor);
            let first = number_to_u32(parser.parse_number_token().ok()?)?;
            cursor = parser.pos;
            blanks(&mut cursor);
            let mut parser = ValueParser::new(bytes, cursor);
            let count = number_to_u32(parser.parse_number_token().ok()?)?;
            cursor = parser.pos;
            blanks(&mut cursor);
            for index in 0..count {
                let end = cursor.checked_add(20)?;
                let entry = bytes.get(cursor..end)?;
                let position = digits(entry.get(..10)?)?;
                let generation = u16::try_from(digits(entry.get(11..16)?)?).ok()?;
                if entry.get(17) == Some(&b'n') {
                    self.offer(
                        first.checked_add(index)?,
                        XrefEntry::At(position, generation),
                    )?;
                } else {
                    self.charge(1)?;
                }
                cursor = end;
            }
        }
        skip_whitespace_and_comments(bytes, &mut cursor);
        let mut parser = ValueParser::new(bytes, cursor);
        match parser.parse_value().ok()? {
            PdfValue::Dictionary(trailer) => Some((trailer, parser.pos)),
            _ => None,
        }
    }

    /// Reads the rows of a cross-reference stream straight from its decoded
    /// bytes. Free rows are read past and not recorded.
    fn read_stream(&mut self, dictionary: &PdfDictionary, data: &[u8]) -> Option<()> {
        let integers = |value: &PdfValue| -> Option<Vec<usize>> {
            match value {
                PdfValue::Array(items) => items
                    .iter()
                    .map(|item| match item {
                        PdfValue::Integer(number) => usize::try_from(*number).ok(),
                        _ => None,
                    })
                    .collect(),
                _ => None,
            }
        };
        let widths = integers(dictionary.get("W")?)?;
        let [kind_width, second_width, third_width] = <[usize; 3]>::try_from(widths).ok()?;
        if kind_width > 8 || second_width > 8 || third_width > 8 {
            return None;
        }
        let row = kind_width + second_width + third_width;
        if row == 0 {
            return None;
        }
        let ranges = match dictionary.get("Index") {
            Some(index) => integers(index)?,
            None => vec![0, usize::try_from(dictionary.get_integer("Size")?).ok()?],
        };
        let field = |bytes: &[u8]| {
            bytes
                .iter()
                .fold(0u64, |value, byte| value << 8 | u64::from(*byte))
        };
        let mut rows = data.chunks_exact(row);
        for [start, count] in ranges.as_chunks::<2>().0 {
            self.charge(1)?;
            let first = u32::try_from(*start).ok()?;
            for index in 0..*count {
                let Some(row) = rows.next() else {
                    return Some(());
                };
                let (kind, rest) = row.split_at_checked(kind_width)?;
                let (second, third) = rest.split_at_checked(second_width)?;
                let number = first.checked_add(u32::try_from(index).ok()?)?;
                // A missing type field means an in-use object.
                match if kind_width == 0 { 1 } else { field(kind) } {
                    1 => self.offer(
                        number,
                        XrefEntry::At(
                            usize::try_from(field(second)).ok()?,
                            u16::try_from(field(third)).ok()?,
                        ),
                    )?,
                    2 => self.offer(
                        number,
                        XrefEntry::Packed(u32::try_from(field(second)).ok()?),
                    )?,
                    _ => self.charge(1)?,
                }
            }
        }
        Some(())
    }
}

/// Whether `offset` begins a line, allowing leading blanks.
///
/// A cross-reference table is entered only at an `xref` keyword that stands
/// at the start of its line. One written after a `%`, or in the middle of
/// other tokens, is a comment or data, not a section.
fn starts_its_line(bytes: &[u8], offset: usize) -> bool {
    let before = bytes.get(..offset).unwrap_or_default();
    before
        .iter()
        .rev()
        .take(64)
        .find(|byte| !matches!(byte, b' ' | b'\t'))
        .is_none_or(|byte| matches!(byte, b'\n' | b'\r'))
}

/// Points `object_index` at the copy of each object the cross-reference
/// chain calls current, and returns the catalog the newest trailer names
/// together with the work spent.
///
/// The chain does two things and no more. It names the newest trailer's
/// `/Root`, and where the scan found several copies of an object number it
/// says which one is current. It starts at the last `startxref` and follows
/// `/Prev`; a table and its `/XRefStm` are one section; the newest section
/// that lists an object in use wins and an older one never overrides it.
///
/// Free entries are ignored. The interpreter that renders pages ignores
/// them too, and text extraction has to describe the same document it
/// draws: a content stream a later revision marks free but a page still
/// references is drawn, so it is extracted.
///
/// Everything is bounded by what the scan found. An entry is kept only for
/// an object number the scan produced. A table is entered only at an `xref`
/// keyword at the start of a line, outside every scanned object and outside
/// every section already read, so sections cannot overlap or be re-read.
/// A cross-reference stream is one the scan parsed as an object, decoded
/// here whether or not decoded streams are being kept, and refused past
/// [`XREF_DECODE_MULTIPLE`] times the file length. All of it draws on one
/// budget of [`XREF_WORK_PER_BYTE`] units per byte of file.
///
/// The chain is all or nothing, because the renderer's is. A table that is
/// not in the strict twenty-byte entry format, an in-use entry that does
/// not land exactly on that object's header, a section that cannot be
/// read, a `/Root` that is not a catalog, or a spent budget each discard
/// everything read: the scan-order index stands for every object and no
/// catalog is returned.
///
/// # Known differences from the renderer
///
/// Two remain, both older than this function.
///
/// * This function refuses some tables the renderer follows: one with
///   other bytes before `xref` on its line, one swallowed by an unfinished
///   final object's span, and one written inside a stream body. Extraction
///   then uses scan order while the renderer uses the table, so when an
///   object is written twice they can read different copies.
/// * An object whose only cross-reference entry is free, or one appended
///   after the last `%%EOF` and listed nowhere, is not in the renderer's
///   map. This crate's scan still finds it, so its text is extracted from
///   a page the renderer draws without it.
fn apply_xref_chain(
    bytes: &[u8],
    objects: &[PdfObject],
    streams: &[PdfStream],
    object_index: &mut HashMap<(u32, u16), usize>,
) -> (Option<(u32, u16)>, usize, usize) {
    let known: HashSet<u32> = objects.iter().map(|object| object.object_number).collect();
    let mut chain = XrefChain {
        bytes,
        known: &known,
        entries: HashMap::new(),
        work: 0,
        budget: bytes
            .len()
            .saturating_mul(XREF_WORK_PER_BYTE)
            .saturating_add(4096),
    };
    let catalog = follow_xref_chain(&mut chain, objects, streams, object_index);
    (catalog, chain.work, chain.entries.len())
}

/// The body of [`apply_xref_chain`], with `None` for every way the chain
/// can fail to be usable.
fn follow_xref_chain(
    chain: &mut XrefChain<'_>,
    objects: &[PdfObject],
    streams: &[PdfStream],
    object_index: &mut HashMap<(u32, u16), usize>,
) -> Option<(u32, u16)> {
    let bytes = chain.bytes;
    let marker = memchr::memmem::rfind(bytes, b"startxref")?;
    let mut cursor = marker.checked_add(9)?;
    skip_whitespace_and_comments(bytes, &mut cursor);
    let start = number_to_usize(ValueParser::new(bytes, cursor).parse_number_token().ok()?)?;

    // Objects by where their header starts. Objects unpacked from an
    // object stream carry their container's span and come after every
    // directly written object, so the first object at an offset is the one
    // written there.
    let mut by_offset: HashMap<usize, usize> = HashMap::new();
    // Objects unpacked from an object stream, by number and container.
    let mut packed: HashMap<(u32, usize), usize> = HashMap::new();
    // The byte ranges of directly written objects, in file order.
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        match by_offset.entry(object.span.start) {
            std::collections::hash_map::Entry::Occupied(_) => {
                packed.insert((object.object_number, object.span.start), index);
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(index);
                spans.push((object.span.start, object.span.end));
            }
        }
    }
    spans.sort_unstable();
    let inside_object = |offset: usize| {
        let after = spans.partition_point(|(start, _)| *start <= offset);
        after
            .checked_sub(1)
            .and_then(|index| spans.get(index))
            .is_some_and(|(_, end)| offset < *end)
    };
    let at = |offset: usize| -> Option<usize> {
        let mut cursor = offset;
        skip_whitespace_and_comments(bytes, &mut cursor);
        by_offset.get(&cursor).copied()
    };
    let decode_cap = bytes
        .len()
        .saturating_mul(XREF_DECODE_MULTIPLE)
        .saturating_add(4096);
    // Reads the cross-reference stream object at `offset` into the chain
    // and returns its dictionary.
    let read_stream_at = |chain: &mut XrefChain<'_>,
                          seen: &mut HashSet<usize>,
                          offset: usize|
     -> Option<PdfDictionary> {
        let index = at(offset)?;
        if !seen.insert(index) {
            return None;
        }
        let object = objects.get(index)?;
        let dictionary = object.dictionary.as_ref()?;
        let stream = streams.get(object.stream_index?)?;
        let decoded;
        let data: &[u8] = match stream.decoded.as_deref() {
            Some(kept) => kept,
            None => {
                let raw = bytes.get(stream.raw_range.start..stream.raw_range.end)?;
                decoded = decode_filters(
                    raw,
                    &stream.filters,
                    dictionary.get("DecodeParms"),
                    decode_cap,
                )
                .ok()?;
                &decoded
            }
        };
        if data.len() > decode_cap {
            return None;
        }
        chain.read_stream(dictionary, data)?;
        Some(dictionary.clone())
    };

    let mut root: Option<(u32, u16)> = None;
    let mut tables: Vec<(usize, usize)> = Vec::new();
    let mut seen_streams: HashSet<usize> = HashSet::new();
    let mut next = Some(start);
    while let Some(offset) = next.take() {
        chain.charge(XREF_SECTION_COST)?;
        let mut cursor = offset;
        skip_whitespace_and_comments(bytes, &mut cursor);
        let is_table = bytes
            .get(cursor..)
            .is_some_and(|rest| rest.starts_with(b"xref"));
        let trailer = if is_table {
            let enterable = starts_its_line(bytes, cursor)
                && !inside_object(cursor)
                && !tables
                    .iter()
                    .any(|(start, end)| *start <= cursor && cursor < *end);
            if !enterable {
                return None;
            }
            let (trailer, end) = chain.read_table(cursor)?;
            tables.push((cursor, end));
            // A hybrid file's stream belongs to this section: it places the
            // objects the table could not describe.
            if let Some(hybrid) = trailer
                .get_integer("XRefStm")
                .and_then(|value| usize::try_from(value).ok())
            {
                read_stream_at(chain, &mut seen_streams, hybrid)?;
            }
            trailer
        } else {
            read_stream_at(chain, &mut seen_streams, offset)?
        };
        if root.is_none() {
            if let Some(PdfValue::Reference {
                object_number,
                generation,
            }) = trailer.get("Root")
            {
                root = Some((*object_number, *generation));
            }
        }
        next = trailer
            .get_integer("Prev")
            .and_then(|value| usize::try_from(value).ok());
    }

    // Every in-use entry at an offset must land exactly on the header of
    // that object, number and generation, as the scan recorded it. The
    // renderer reads `N G obj` at the offset with no skipping and compares
    // the identifier (hayro-syntax 0.5.0, `xref.rs` 518-522), and on the
    // first offset that fails it throws its whole map away and rebuilds by
    // scanning (534-545). So one bad entry here discards the whole chain.
    let mut placed: Vec<(u32, usize)> = Vec::new();
    let mut packed_entries: Vec<(u32, u32)> = Vec::new();
    for (number, entry) in &chain.entries {
        match *entry {
            XrefEntry::At(offset, generation) => {
                let found = *by_offset.get(&offset)?;
                let object = objects.get(found)?;
                if object.object_number != *number || object.generation != generation {
                    return None;
                }
                placed.push((*number, found));
            }
            XrefEntry::Packed(container) => packed_entries.push((*number, container)),
        }
    }

    let root = root?;
    // Every generation the scan indexed, by object number.
    let mut generations: HashMap<u32, Vec<u16>> = HashMap::new();
    for (number, generation) in object_index.keys() {
        generations.entry(*number).or_default().push(*generation);
    }
    let mut chosen = object_index.clone();
    let choose = |chosen: &mut HashMap<(u32, u16), usize>, number: u32, found: usize| {
        let Some(object) = objects.get(found) else {
            return;
        };
        for generation in generations.get(&number).into_iter().flatten() {
            chosen.remove(&(number, *generation));
        }
        chosen.insert((number, object.generation), found);
    };
    for (number, found) in placed {
        choose(&mut chosen, number, found);
    }
    // Then the objects inside object streams, now that every container is
    // settled. One this scan did not unpack keeps the scan's answer: the
    // renderer does not rebuild its map over an object stream.
    for (number, container) in packed_entries {
        let found = chosen
            .get(&(container, 0))
            .and_then(|index| objects.get(*index))
            .and_then(|container| packed.get(&(number, container.span.start)));
        if let Some(found) = found {
            choose(&mut chosen, number, *found);
        }
    }
    let root_index = *generations
        .get(&root.0)
        .into_iter()
        .flatten()
        .find_map(|generation| chosen.get(&(root.0, *generation)))?;
    if !chain.entries.contains_key(&root.0) {
        return None;
    }
    let is_catalog = objects
        .get(root_index)?
        .dictionary
        .as_ref()
        .and_then(|dictionary| dictionary.get_name("Type"))
        == Some("Catalog");
    if !is_catalog {
        return None;
    }
    *object_index = chosen;
    Some((root.0, objects.get(root_index)?.generation))
}

fn skip_whitespace_and_comments(bytes: &[u8], cursor: &mut usize) {
    loop {
        while *cursor < bytes.len() && bytes[*cursor].is_ascii_whitespace() {
            *cursor += 1;
        }
        if *cursor < bytes.len() && bytes[*cursor] == b'%' {
            while *cursor < bytes.len() && !matches!(bytes[*cursor], b'\r' | b'\n') {
                *cursor += 1;
            }
            continue;
        }
        break;
    }
}

fn is_delimiter(byte: u8) -> bool {
    matches!(
        byte,
        b'\x00'
            | b'\t'
            | b'\n'
            | b'\x0c'
            | b'\r'
            | b' '
            | b'('
            | b')'
            | b'<'
            | b'>'
            | b'['
            | b']'
            | b'{'
            | b'}'
            | b'/'
            | b'%'
    )
}

/// Deepest array or dictionary nesting the object parser follows.
///
/// Real documents nest a handful of levels; an input that nests deeper is
/// malformed or hostile and is refused. The parser keeps its open containers
/// in a list on the heap, so this bound is not what protects the parser's
/// own stack. It bounds the depth of the [`PdfValue`] tree the parser hands
/// out, which later code walks and drops recursively: raising it deepens
/// every one of those walks. Indirect references do not count toward it.
const MAX_VALUE_NESTING: usize = 128;

struct ValueParser<'a> {
    bytes: &'a [u8],
    pos: usize,
    /// Containers currently open, bounded by [`MAX_VALUE_NESTING`].
    depth: usize,
}

/// An array or dictionary the object parser has opened and not yet closed.
enum OpenContainer {
    Array {
        values: Vec<PdfValue>,
        /// Where the element being read began, to detect one that consumed
        /// nothing.
        element_start: usize,
    },
    Dictionary {
        entries: Vec<PdfDictionaryEntry>,
        /// Key of the entry whose value is being read.
        key: String,
    },
}

impl OpenContainer {
    fn into_value(self) -> PdfValue {
        match self {
            Self::Array { values, .. } => PdfValue::Array(values),
            Self::Dictionary { entries, .. } => PdfValue::Dictionary(PdfDictionary { entries }),
        }
    }
}

/// What the first token of a value turned out to be.
enum ValueStart {
    /// A value that holds no other values, read in full.
    Scalar(PdfValue),
    /// The opening delimiter of an array or dictionary, consumed.
    Container(OpenContainer),
}

impl<'a> ValueParser<'a> {
    fn new(bytes: &'a [u8], pos: usize) -> Self {
        Self {
            bytes,
            pos,
            depth: 0,
        }
    }

    /// Record one more open container, refusing nesting past the limit.
    fn enter_container(&mut self) -> Result<()> {
        if self.depth >= MAX_VALUE_NESTING {
            return Err(anyhow!(
                "PDF value nesting exceeds {MAX_VALUE_NESTING} levels"
            ));
        }
        self.depth += 1;
        Ok(())
    }

    /// Parse one value, however deeply its arrays and dictionaries nest.
    ///
    /// Open containers are kept in a list rather than on the call stack, so
    /// the stack this needs does not grow with the input's nesting.
    fn parse_value(&mut self) -> Result<PdfValue> {
        let mut open: Vec<OpenContainer> = Vec::new();
        loop {
            let mut value = match self.parse_value_start()? {
                ValueStart::Scalar(value) => Some(value),
                ValueStart::Container(container) => {
                    open.push(container);
                    None
                }
            };
            // Hand a finished value to the container that holds it, and keep
            // closing containers until one asks for another child.
            loop {
                let Some(top) = open.last_mut() else {
                    // No container is open, so `value` is the whole value: a
                    // container that was just opened is still in the list.
                    return value.ok_or_else(|| anyhow!("unexpected end of input"));
                };
                if let Some(value) = value.take() {
                    match top {
                        OpenContainer::Array {
                            values,
                            element_start,
                        } => {
                            if self.pos == *element_start {
                                // A stray delimiter (`)`, `>`, `{`, `}`) parses
                                // as an empty bareword without consuming it.
                                // Pushing that value and looping would never
                                // advance, so the array is refused.
                                return Err(anyhow!("unexpected delimiter in PDF array"));
                            }
                            values.push(value);
                        }
                        OpenContainer::Dictionary { entries, key } => {
                            entries.push(PdfDictionaryEntry {
                                key: std::mem::take(key),
                                value,
                            });
                        }
                    }
                }
                skip_whitespace_and_comments(self.bytes, &mut self.pos);
                match top {
                    OpenContainer::Array { element_start, .. } => {
                        if self.bytes.get(self.pos) != Some(&b']') {
                            *element_start = self.pos;
                            break;
                        }
                        self.pos += 1;
                    }
                    OpenContainer::Dictionary { key, .. } => {
                        if self.bytes.get(self.pos..self.pos + 2) != Some(b">>") {
                            // `parse_name` consumes at least the solidus, so
                            // every pass makes progress even when the value
                            // that follows is empty.
                            *key = self.parse_name()?;
                            break;
                        }
                        self.pos += 2;
                    }
                }
                self.depth -= 1;
                value = open.pop().map(OpenContainer::into_value);
            }
        }
    }

    /// Read a scalar in full, or consume a container's opening delimiter.
    fn parse_value_start(&mut self) -> Result<ValueStart> {
        skip_whitespace_and_comments(self.bytes, &mut self.pos);
        let Some(byte) = self.bytes.get(self.pos).copied() else {
            return Err(anyhow!("unexpected end of input"));
        };
        let scalar = match byte {
            b'<' if self.bytes.get(self.pos + 1) == Some(&b'<') => {
                self.enter_container()?;
                self.pos += 2;
                return Ok(ValueStart::Container(OpenContainer::Dictionary {
                    entries: Vec::new(),
                    key: String::new(),
                }));
            }
            b'[' => {
                self.enter_container()?;
                self.pos += 1;
                return Ok(ValueStart::Container(OpenContainer::Array {
                    values: Vec::new(),
                    element_start: self.pos,
                }));
            }
            b'/' => PdfValue::Name(self.parse_name()?),
            b'(' => PdfValue::String(decode_pdf_text(&self.parse_literal_string()?)),
            b'<' => PdfValue::HexString(to_upper_hex(&self.parse_hex_string()?)),
            b't' if self.bytes.get(self.pos..self.pos + 4) == Some(b"true") => {
                self.pos += 4;
                PdfValue::Bool(true)
            }
            b'f' if self.bytes.get(self.pos..self.pos + 5) == Some(b"false") => {
                self.pos += 5;
                PdfValue::Bool(false)
            }
            b'n' if self.bytes.get(self.pos..self.pos + 4) == Some(b"null") => {
                self.pos += 4;
                PdfValue::Null
            }
            b'+' | b'-' | b'.' | b'0'..=b'9' => self.parse_number_or_reference()?,
            _ => PdfValue::Raw(self.parse_bareword()),
        };
        Ok(ValueStart::Scalar(scalar))
    }

    fn parse_name(&mut self) -> Result<String> {
        if self.bytes.get(self.pos) != Some(&b'/') {
            return Err(anyhow!("expected name"));
        }
        self.pos += 1;
        let start = self.pos;
        while self.pos < self.bytes.len() && !is_delimiter(self.bytes[self.pos]) {
            self.pos += 1;
        }
        Ok(String::from_utf8_lossy(&self.bytes[start..self.pos]).into_owned())
    }

    fn parse_literal_string(&mut self) -> Result<Vec<u8>> {
        self.pos += 1;
        let mut depth = 1usize;
        let mut out = Vec::new();
        while self.pos < self.bytes.len() {
            let byte = self.bytes[self.pos];
            self.pos += 1;
            match byte {
                b'\\' => {
                    let Some(escaped) = self.bytes.get(self.pos).copied() else {
                        break;
                    };
                    self.pos += 1;
                    match escaped {
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0c),
                        b'(' | b')' | b'\\' => out.push(escaped),
                        b'\r' => {
                            if self.bytes.get(self.pos) == Some(&b'\n') {
                                self.pos += 1;
                            }
                        }
                        b'\n' => {}
                        b'0'..=b'7' => {
                            let mut octal = vec![escaped];
                            for _ in 0..2 {
                                if matches!(self.bytes.get(self.pos), Some(b'0'..=b'7')) {
                                    octal.push(self.bytes[self.pos]);
                                    self.pos += 1;
                                } else {
                                    break;
                                }
                            }
                            let value = u8::from_str_radix(&String::from_utf8_lossy(&octal), 8)
                                .unwrap_or_default();
                            out.push(value);
                        }
                        other => out.push(other),
                    }
                }
                b'(' => {
                    depth += 1;
                    out.push(byte);
                }
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    out.push(byte);
                }
                other => out.push(other),
            }
        }
        Ok(out)
    }

    fn parse_hex_string(&mut self) -> Result<Vec<u8>> {
        self.pos += 1;
        let start = self.pos;
        while self.pos < self.bytes.len() && self.bytes[self.pos] != b'>' {
            self.pos += 1;
        }
        let slice = self
            .bytes
            .get(start..self.pos)
            .ok_or_else(|| anyhow!("invalid hex string"))?;
        if self.bytes.get(self.pos) == Some(&b'>') {
            self.pos += 1;
        }
        decode_ascii_hex(slice)
    }

    fn parse_number_or_reference(&mut self) -> Result<PdfValue> {
        let first = self.parse_number_token()?;
        let save = self.pos;
        skip_whitespace_and_comments(self.bytes, &mut self.pos);
        if matches!(self.bytes.get(self.pos), Some(b'0'..=b'9' | b'+' | b'-')) {
            let second = self.parse_number_token()?;
            skip_whitespace_and_comments(self.bytes, &mut self.pos);
            if self.bytes.get(self.pos) == Some(&b'R')
                && first.fract() == 0.0
                && second.fract() == 0.0
            {
                self.pos += 1;
                return Ok(PdfValue::Reference {
                    object_number: first as u32,
                    generation: second as u16,
                });
            }
            self.pos = save;
            return Ok(number_value(first));
        }
        self.pos = save;
        Ok(number_value(first))
    }

    fn parse_number_token(&mut self) -> Result<f64> {
        let start = self.pos;
        if matches!(self.bytes.get(self.pos), Some(b'+' | b'-')) {
            self.pos += 1;
        }
        while self.pos < self.bytes.len() && matches!(self.bytes[self.pos], b'0'..=b'9' | b'.') {
            self.pos += 1;
        }
        let token = std::str::from_utf8(&self.bytes[start..self.pos])?;
        Ok(token.parse()?)
    }

    fn parse_bareword(&mut self) -> String {
        let start = self.pos;
        while self.pos < self.bytes.len() && !is_delimiter(self.bytes[self.pos]) {
            self.pos += 1;
        }
        String::from_utf8_lossy(&self.bytes[start..self.pos]).into_owned()
    }
}

fn number_value(value: f64) -> PdfValue {
    if value.fract() == 0.0 && value >= i64::MIN as f64 && value <= i64::MAX as f64 {
        PdfValue::Integer(value as i64)
    } else {
        PdfValue::Real(value)
    }
}

fn number_to_u32(value: f64) -> Option<u32> {
    (value.fract() == 0.0 && value >= 0.0 && value <= u32::MAX as f64).then_some(value as u32)
}

fn number_to_usize(value: f64) -> Option<usize> {
    (value.fract() == 0.0 && value >= 0.0 && value <= usize::MAX as f64).then_some(value as usize)
}

fn decode_filters(
    raw: &[u8],
    filters: &[String],
    decode_parms: Option<&PdfValue>,
    max_len: usize,
) -> Result<Vec<u8>> {
    let mut current = raw.to_vec();
    for (filter_index, filter) in filters.iter().enumerate() {
        let parms = filter_decode_parms(decode_parms, filter_index);
        current = match filter.as_str() {
            "FlateDecode" | "Fl" => decode_flate(&current, max_len)?,
            "LZWDecode" | "LZW" => {
                let early_change = parms
                    .and_then(|dict| dict.get_integer("EarlyChange"))
                    .unwrap_or(1);
                let early_change = u8::try_from(early_change)
                    .ok()
                    .filter(|value| *value <= 1)
                    .ok_or_else(|| anyhow!("LZW EarlyChange must be 0 or 1"))?;
                crate::lzw::decode_lzw_limited(&current, early_change, max_len)?
            }
            "ASCIIHexDecode" | "AHx" => decode_ascii_hex(&current)?,
            "ASCII85Decode" | "A85" => decode_ascii85(&current, max_len)?,
            "RunLengthDecode" | "RL" => decode_run_length(&current, max_len)?,
            other => return Err(anyhow!("unsupported PDF filter: {}", other)),
        };
        if matches!(filter.as_str(), "FlateDecode" | "Fl" | "LZWDecode" | "LZW") {
            current = apply_stream_predictor(&current, parms)?;
        }
        if current.len() > max_len {
            return Err(anyhow!(
                "decoded stream len {} exceeds max_decoded_stream_len {}",
                current.len(),
                max_len
            ));
        }
    }
    Ok(current)
}

fn filter_decode_parms(value: Option<&PdfValue>, filter_index: usize) -> Option<&PdfDictionary> {
    match value? {
        PdfValue::Dictionary(dict) if filter_index == 0 => Some(dict),
        PdfValue::Array(values) => values.get(filter_index).and_then(pdf_value_as_dictionary),
        _ => None,
    }
}

fn apply_stream_predictor(data: &[u8], parms: Option<&PdfDictionary>) -> Result<Vec<u8>> {
    let Some(parms) = parms else {
        return Ok(data.to_vec());
    };
    let predictor = parms.get_integer("Predictor").unwrap_or(1);
    if predictor <= 1 {
        return Ok(data.to_vec());
    }
    let predictor = u8::try_from(predictor).map_err(|_| anyhow!("invalid Predictor value"))?;
    let columns = positive_decode_parameter(parms, "Columns", 1)?;
    let colors = positive_decode_parameter(parms, "Colors", 1)?;
    let bits_per_component = positive_decode_parameter(parms, "BitsPerComponent", 8)?;
    crate::lzw::apply_predictor(data, predictor, columns, colors, bits_per_component)
}

fn positive_decode_parameter(dict: &PdfDictionary, key: &str, default: usize) -> Result<usize> {
    match dict.get_integer(key) {
        None => Ok(default),
        Some(value) if value > 0 => {
            usize::try_from(value).map_err(|_| anyhow!("{} decode parameter is too large", key))
        }
        Some(_) => Err(anyhow!("{} decode parameter must be positive", key)),
    }
}

fn is_passthrough_filter_chain(filters: &[String]) -> bool {
    !filters.is_empty() && filters.iter().all(|filter| is_passthrough_filter(filter))
}

fn is_passthrough_filter(filter: &str) -> bool {
    matches!(
        filter,
        "DCTDecode" | "DCT" | "JPXDecode" | "JBIG2Decode" | "CCITTFaxDecode" | "CCF" | "Crypt"
    )
}

fn is_ccitt_filter(filter: &str) -> bool {
    matches!(filter, "CCITTFaxDecode" | "CCF")
}

fn decode_flate(raw: &[u8], max_len: usize) -> Result<Vec<u8>> {
    let mut decompressor = Decompressor::new();
    match decode_flate_with_mode(raw, &mut decompressor, true, max_len) {
        Ok(decoded) => Ok(decoded),
        Err(FlateDecodeFailure::Limit(message)) => Err(anyhow!(message)),
        Err(FlateDecodeFailure::Codec(_)) => {
            decode_flate_with_mode(raw, &mut decompressor, false, max_len)
                .map_err(|failure| anyhow!(failure.message()))
        }
    }
}

enum FlateDecodeFailure {
    Limit(String),
    Codec(String),
}

impl FlateDecodeFailure {
    fn message(self) -> String {
        match self {
            Self::Limit(message) | Self::Codec(message) => message,
        }
    }
}

fn decode_flate_with_mode(
    raw: &[u8],
    decompressor: &mut Decompressor,
    zlib: bool,
    max_len: usize,
) -> std::result::Result<Vec<u8>, FlateDecodeFailure> {
    let allocation_limit = max_len.max(1);
    let mut capacity = raw.len().saturating_mul(4).max(1024).min(allocation_limit);
    loop {
        let mut out = vec![0u8; capacity];
        let result = if zlib {
            decompressor.zlib_decompress(raw, &mut out)
        } else {
            decompressor.deflate_decompress(raw, &mut out)
        };

        match result {
            Ok(actual) => {
                if actual > max_len {
                    return Err(FlateDecodeFailure::Limit(format!(
                        "decoded stream len {} exceeds max_decoded_stream_len {}",
                        actual, max_len
                    )));
                }
                out.truncate(actual);
                return Ok(out);
            }
            Err(DecompressionError::InsufficientSpace) => {
                if capacity >= max_len {
                    return Err(FlateDecodeFailure::Limit(format!(
                        "flate decoded stream exceeds max_decoded_stream_len {}",
                        max_len
                    )));
                }
                capacity = capacity
                    .checked_mul(2)
                    .ok_or_else(|| {
                        FlateDecodeFailure::Limit("flate output size overflow".to_string())
                    })?
                    .min(max_len);
            }
            Err(error) => {
                return Err(FlateDecodeFailure::Codec(format!(
                    "flate decode failed: {}",
                    error
                )))
            }
        }
    }
}

fn decode_ascii_hex(raw: &[u8]) -> Result<Vec<u8>> {
    let mut nybble: Option<u8> = None;
    let mut out = Vec::with_capacity(raw.len() / 2);
    for &byte in raw {
        match byte {
            b'>' => break,
            b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F' => {
                let value = hex_value(byte)?;
                if let Some(high) = nybble.take() {
                    out.push((high << 4) | value);
                } else {
                    nybble = Some(value);
                }
            }
            b'\x00' | b'\t' | b'\n' | b'\x0c' | b'\r' | b' ' => {}
            other => return Err(anyhow!("invalid ASCIIHex byte: 0x{:02X}", other)),
        }
    }
    if let Some(high) = nybble {
        out.push(high << 4);
    }
    Ok(out)
}

/// The error for a filter stage whose output passes the stream limit.
///
/// The wording carries `max_decoded_stream_len` because that is what
/// [`is_decode_length_limit`] recognizes as a budget failure.
fn decoded_len_limit_error(filter: &str, max_len: usize) -> anyhow::Error {
    anyhow!("{filter} decoded stream exceeds max_decoded_stream_len {max_len}")
}

/// One ASCII85 group of five base-85 digits as its four-byte value.
///
/// Five digits can spell a number above `u32::MAX` (`s8W-!` is the largest
/// valid group); such a group is malformed, and computing it in 32 bits would
/// overflow.
fn ascii85_group_value(group: &[u32; 5]) -> Result<u32> {
    let value = group
        .iter()
        .fold(0u64, |value, digit| value * 85 + u64::from(*digit));
    u32::try_from(value).map_err(|_| anyhow!("ASCII85 group out of range"))
}

fn decode_ascii85(raw: &[u8], max_len: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut group = [0u32; 5];
    let mut len = 0usize;
    let mut i = 0usize;
    while i < raw.len() {
        // `z` expands one byte to four, so the limit is enforced as the output
        // grows rather than after the whole stage has been materialized.
        if out.len() > max_len {
            return Err(decoded_len_limit_error("ASCII85", max_len));
        }
        match raw[i] {
            b'~' if raw.get(i + 1) == Some(&b'>') => break,
            b'z' if len == 0 => {
                out.extend_from_slice(&[0, 0, 0, 0]);
                i += 1;
                continue;
            }
            b'\x00' | b'\t' | b'\n' | b'\x0c' | b'\r' | b' ' => {
                i += 1;
                continue;
            }
            byte @ b'!'..=b'u' => {
                group[len] = (byte - b'!') as u32;
                len += 1;
            }
            other => return Err(anyhow!("invalid ASCII85 byte: 0x{:02X}", other)),
        }
        i += 1;
        if len == 5 {
            let value = ascii85_group_value(&group)?;
            out.extend_from_slice(&value.to_be_bytes());
            len = 0;
        }
    }
    if len > 0 {
        for item in group.iter_mut().skip(len) {
            *item = 84;
        }
        let value = ascii85_group_value(&group)?;
        let bytes = value.to_be_bytes();
        out.extend_from_slice(&bytes[..len - 1]);
    }
    if out.len() > max_len {
        return Err(decoded_len_limit_error("ASCII85", max_len));
    }
    Ok(out)
}

fn decode_run_length(raw: &[u8], max_len: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while cursor < raw.len() {
        // A repeat run expands two bytes to 128, and chained RunLength
        // filters compound that, so the limit is enforced as the output grows
        // rather than after the whole stage has been materialized.
        if out.len() > max_len {
            return Err(decoded_len_limit_error("RunLength", max_len));
        }
        let length = raw[cursor];
        cursor += 1;
        match length {
            128 => break,
            0..=127 => {
                let count = length as usize + 1;
                let end = cursor
                    .checked_add(count)
                    .ok_or_else(|| anyhow!("run-length overflow"))?;
                let slice = raw
                    .get(cursor..end)
                    .ok_or_else(|| anyhow!("truncated run-length literal"))?;
                out.extend_from_slice(slice);
                cursor = end;
            }
            129..=255 => {
                let count = 257usize - length as usize;
                let byte = *raw
                    .get(cursor)
                    .ok_or_else(|| anyhow!("truncated run-length repeat"))?;
                cursor += 1;
                out.extend(std::iter::repeat_n(byte, count));
            }
        }
    }
    if out.len() > max_len {
        return Err(decoded_len_limit_error("RunLength", max_len));
    }
    Ok(out)
}

fn hex_value(byte: u8) -> Result<u8> {
    Ok(match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => 10 + byte - b'a',
        b'A'..=b'F' => 10 + byte - b'A',
        _ => return Err(anyhow!("invalid hex byte")),
    })
}

fn extract_text_fragments(
    bytes: &[u8],
    object_number: u32,
    generation: u16,
    stream_index: usize,
    font_bindings: &HashMap<String, FontBinding>,
) -> Vec<PdfTextFragment> {
    let mut fragments = Vec::new();
    let mut cursor = 0usize;
    let mut operands = Vec::new();
    let mut current_font: Option<String> = None;

    while let Some(token) = next_content_token(bytes, &mut cursor) {
        match token {
            ContentToken::Value(value) => operands.push(value),
            ContentToken::Operator(operator) => {
                let binding = current_font
                    .as_deref()
                    .and_then(|name| font_bindings.get(name));
                match operator.as_str() {
                    "BI" => {
                        skip_inline_image(bytes, &mut cursor, &operands);
                        operands.clear();
                    }
                    "Tf" => {
                        current_font = operands.iter().find_map(|value| match value {
                            ContentValue::Name(name) => Some(name.clone()),
                            _ => None,
                        });
                    }
                    "Tj" | "'" => {
                        if let Some(text) = operands
                            .last()
                            .and_then(|value| content_value_text(value, binding))
                        {
                            if !text.is_empty() {
                                fragments.push(PdfTextFragment {
                                    object_number,
                                    generation,
                                    stream_index,
                                    operator: operator.clone(),
                                    text,
                                    font: current_font.clone(),
                                });
                            }
                        }
                    }
                    "\"" => {
                        if let Some(text) = operands
                            .last()
                            .and_then(|value| content_value_text(value, binding))
                        {
                            if !text.is_empty() {
                                fragments.push(PdfTextFragment {
                                    object_number,
                                    generation,
                                    stream_index,
                                    operator: operator.clone(),
                                    text,
                                    font: current_font.clone(),
                                });
                            }
                        }
                    }
                    "TJ" => {
                        if let Some(ContentValue::Array(items)) = operands.last() {
                            let text = items
                                .iter()
                                .filter_map(|value| content_value_text(value, binding))
                                .collect::<Vec<_>>()
                                .join("");
                            if !text.is_empty() {
                                fragments.push(PdfTextFragment {
                                    object_number,
                                    generation,
                                    stream_index,
                                    operator: operator.clone(),
                                    text,
                                    font: current_font.clone(),
                                });
                            }
                        }
                    }
                    _ => {}
                }
                operands.clear();
            }
        }
    }

    fragments
}

fn extract_semantic_text_runs(
    bytes: &[u8],
    page_index: usize,
    stream_index: usize,
    font_bindings: &HashMap<String, FontBinding>,
) -> Vec<PdfTextRun> {
    let mut runs = Vec::new();
    let mut cursor = 0usize;
    let mut operands = Vec::new();
    let mut state = ContentState::default();

    while let Some(token) = next_content_token(bytes, &mut cursor) {
        match token {
            ContentToken::Value(value) => operands.push(value),
            ContentToken::Operator(operator) => {
                match operator.as_str() {
                    // Inline image: `BI <params> ID <binary> EI`.
                    //
                    // The binary is raw and unbounded — this is the one place a
                    // content stream carries bytes that are not operands — so
                    // without this branch the tokenizer reads image data as
                    // operators and strings. Measured on a fixture whose 1x1
                    // RGB payload happened to contain `(AB)Tj`: the extracted
                    // text came back as `BeforeABAfter`, three characters of
                    // fabricated text from a bitmap. Silently wrong text is
                    // worse than missing text.
                    "BI" => {
                        skip_inline_image(bytes, &mut cursor, &operands);
                        operands.clear();
                    }
                    "q" => {
                        if state.graphics_stack.len() >= MAX_GRAPHICS_STATE_DEPTH {
                            if state.unsaved_states == 0 {
                                state.overflow_state = Some((
                                    state.ctm,
                                    state.fill_color.clone(),
                                    state.stroke_color.clone(),
                                ));
                            }
                            state.unsaved_states = state.unsaved_states.saturating_add(1);
                        } else {
                            state.graphics_stack.push(state.ctm);
                            state
                                .color_stack
                                .push((state.fill_color.clone(), state.stroke_color.clone()));
                        }
                    }
                    "Q" => {
                        if state.unsaved_states > 0 {
                            state.unsaved_states -= 1;
                            if let Some((matrix, fill, stroke)) = state.overflow_state.clone() {
                                state.ctm = matrix;
                                state.fill_color = fill;
                                state.stroke_color = stroke;
                            }
                            if state.unsaved_states == 0 {
                                state.overflow_state = None;
                            }
                        } else {
                            if let Some(matrix) = state.graphics_stack.pop() {
                                state.ctm = matrix;
                            }
                            if let Some((fill, stroke)) = state.color_stack.pop() {
                                state.fill_color = fill;
                                state.stroke_color = stroke;
                            }
                        }
                    }
                    "cm" => {
                        if let Some(matrix) = operands_as_six_numbers(&operands) {
                            state.ctm = matrix_multiply(state.ctm, matrix);
                        }
                    }
                    "BT" => {
                        state.text_matrix = IDENTITY_MATRIX;
                        state.line_matrix = IDENTITY_MATRIX;
                    }
                    "Tf" => {
                        state.font_name = operands.iter().find_map(|value| match value {
                            ContentValue::Name(name) => Some(name.clone()),
                            _ => None,
                        });
                        state.font_size = operands.iter().rev().find_map(content_number);
                    }
                    "TL" => {
                        if let Some(leading) = operands.iter().rev().find_map(content_number) {
                            state.leading = leading;
                        }
                    }
                    "Tc" => {
                        if let Some(char_spacing) = operands.iter().rev().find_map(content_number) {
                            state.char_spacing = char_spacing;
                        }
                    }
                    "Tw" => {
                        if let Some(word_spacing) = operands.iter().rev().find_map(content_number) {
                            state.word_spacing = word_spacing;
                        }
                    }
                    "Tz" => {
                        if let Some(scale) = operands.iter().rev().find_map(content_number) {
                            state.horizontal_scaling = scale;
                        }
                    }
                    "Ts" => {
                        if let Some(rise) = operands.iter().rev().find_map(content_number) {
                            state.text_rise = rise;
                        }
                    }
                    "Tr" => {
                        if let Some(mode) = operands.iter().rev().find_map(content_number) {
                            state.render_mode = TextRenderMode::from(mode as i64);
                        }
                    }
                    "rg" | "sc" => {
                        if let Some(components) = operands_as_numbers(&operands) {
                            state.fill_color = PdfColor {
                                space: ColorSpace::DeviceRGB,
                                components,
                            };
                        }
                    }
                    "RG" | "SC" => {
                        if let Some(components) = operands_as_numbers(&operands) {
                            state.stroke_color = PdfColor {
                                space: ColorSpace::DeviceRGB,
                                components,
                            };
                        }
                    }
                    "g" => {
                        if let Some(components) = operands_as_numbers(&operands) {
                            state.fill_color = PdfColor {
                                space: ColorSpace::DeviceGray,
                                components,
                            };
                        }
                    }
                    "G" => {
                        if let Some(components) = operands_as_numbers(&operands) {
                            state.stroke_color = PdfColor {
                                space: ColorSpace::DeviceGray,
                                components,
                            };
                        }
                    }
                    "k" => {
                        if let Some(components) = operands_as_numbers(&operands) {
                            state.fill_color = PdfColor {
                                space: ColorSpace::DeviceCMYK,
                                components,
                            };
                        }
                    }
                    "K" => {
                        if let Some(components) = operands_as_numbers(&operands) {
                            state.stroke_color = PdfColor {
                                space: ColorSpace::DeviceCMYK,
                                components,
                            };
                        }
                    }
                    "Td" => {
                        if let Some((tx, ty)) = operands_as_pair(&operands) {
                            state.line_matrix = matrix_translate(state.line_matrix, tx, ty);
                            state.text_matrix = state.line_matrix;
                        }
                    }
                    "TD" => {
                        if let Some((tx, ty)) = operands_as_pair(&operands) {
                            state.leading = -ty;
                            state.line_matrix = matrix_translate(state.line_matrix, tx, ty);
                            state.text_matrix = state.line_matrix;
                        }
                    }
                    "Tm" => {
                        if let Some(matrix) = operands_as_six_numbers(&operands) {
                            state.text_matrix = matrix;
                            state.line_matrix = matrix;
                        }
                    }
                    "T*" => {
                        state.line_matrix =
                            matrix_translate(state.line_matrix, 0.0, -state.leading);
                        state.text_matrix = state.line_matrix;
                    }
                    "'" => {
                        state.line_matrix =
                            matrix_translate(state.line_matrix, 0.0, -state.leading);
                        state.text_matrix = state.line_matrix;
                        if let Some(run) = build_text_run(
                            page_index,
                            stream_index,
                            &operator,
                            operands.last(),
                            &state,
                            font_bindings,
                        ) {
                            runs.push(run);
                        }
                    }
                    "\"" => {
                        state.line_matrix =
                            matrix_translate(state.line_matrix, 0.0, -state.leading);
                        state.text_matrix = state.line_matrix;
                        if let Some(run) = build_text_run(
                            page_index,
                            stream_index,
                            &operator,
                            operands.last(),
                            &state,
                            font_bindings,
                        ) {
                            runs.push(run);
                        }
                    }
                    "Tj" => {
                        if let Some(run) = build_text_run(
                            page_index,
                            stream_index,
                            &operator,
                            operands.last(),
                            &state,
                            font_bindings,
                        ) {
                            runs.push(run);
                        }
                    }
                    "TJ" => {
                        if let Some(run) = build_text_run_from_array(
                            page_index,
                            stream_index,
                            &operator,
                            operands.last(),
                            &state,
                            font_bindings,
                        ) {
                            runs.push(run);
                        }
                    }
                    _ => {}
                }
                operands.clear();
            }
        }
    }

    runs
}

#[derive(Debug, Clone)]
enum ContentToken {
    Value(ContentValue),
    Operator(String),
}

#[derive(Debug, Clone)]
enum ContentValue {
    String(Vec<u8>),
    HexString(Vec<u8>),
    Name(String),
    Number(f32),
    Array(Vec<ContentValue>),
    Other,
}

/// Advance `cursor` past an inline image's binary payload.
///
/// Called with the cursor just after the `BI` operator. The payload begins one
/// whitespace byte after `ID` and ends at a whitespace-delimited `EI`, so the
/// scan is for that terminator. The length is computed first when the image is
/// unfiltered and its geometry is known, which removes the ambiguity: a bitmap
/// may legitimately contain the letters `EI`, and stopping there would resume
/// tokenizing mid-payload and produce fabricated text.
///
/// A filtered image (DCT, JPX, CCITT, ASCII85) has no length the parser can
/// compute without decoding, so it falls back to the terminator scan. That is
/// what the spec prescribes for a reader that cannot determine the length, and
/// a false terminator inside compressed data costs the rest of one stream
/// rather than corrupting every stream — a bounded failure rather than an
/// unbounded one.
fn skip_inline_image(bytes: &[u8], cursor: &mut usize, params: &[ContentValue]) {
    let mut width = None;
    let mut height = None;
    let mut bits = None;
    let mut components = 1u32;
    let mut filtered = false;
    let mut index = 0usize;
    while index < params.len() {
        if let ContentValue::Name(name) = &params[index] {
            match name.as_str() {
                "W" => width = params.get(index + 1).and_then(content_value_number),
                "H" => height = params.get(index + 1).and_then(content_value_number),
                "BPC" => bits = params.get(index + 1).and_then(content_value_number),
                "CS" | "ColorSpace" => {
                    if let Some(ContentValue::Name(space)) = params.get(index + 1) {
                        components = match space.as_str() {
                            "G" | "DeviceGray" => 1,
                            "RGB" | "DeviceRGB" => 3,
                            "CMYK" | "DeviceCMYK" => 4,
                            "I" | "Indexed" => 1,
                            _ => 1,
                        }
                    }
                }
                // Any filter means the payload is not the raw pixel array.
                "F" | "Filter" => filtered = true,
                _ => {}
            }
        }
        index += 1;
    }

    // Past `ID` and its single following whitespace byte.
    while *cursor < bytes.len() && !is_content_whitespace(bytes[*cursor]) {
        *cursor += 1;
    }
    if *cursor < bytes.len() {
        *cursor += 1;
    }

    if !filtered {
        if let (Some(w), Some(h), Some(b)) = (width, height, bits) {
            let w = w.max(0.0) as usize;
            let h = h.max(0.0) as usize;
            let b = b.max(0.0) as usize;
            let row_bits = w.saturating_mul(b).saturating_mul(components as usize);
            // Rows are byte-aligned.
            let row_bytes = row_bits.div_ceil(8);
            let total = row_bytes.saturating_mul(h);
            // Bound the advance to the buffer: a malformed length must not walk
            // the cursor past the end and silently end the stream.
            *cursor = (*cursor).saturating_add(total).min(bytes.len());
            return;
        }
    }

    // Fall back to the terminator: `EI` delimited by whitespace on both sides.
    let mut i = *cursor;
    while i + 1 < bytes.len() {
        if bytes[i] == b'E'
            && bytes[i + 1] == b'I'
            && (i == 0 || is_content_whitespace(bytes[i - 1]))
            && (i + 2 >= bytes.len() || is_content_whitespace(bytes[i + 2]))
        {
            *cursor = i + 2;
            return;
        }
        i += 1;
    }
    *cursor = bytes.len();
}

fn content_value_number(value: &ContentValue) -> Option<f32> {
    match value {
        ContentValue::Number(n) => Some(*n),
        _ => None,
    }
}

fn is_content_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | b'\x0c' | b'\0')
}

/// Deepest array nesting the content-stream tokenizer follows.
///
/// A content stream only nests arrays one level (`TJ`, dash patterns), so
/// anything deeper is malformed. The arrays still open are kept in a list,
/// not on the call stack, so the bound limits memory and nonsense, not
/// stack: at one recursive call per level, 128 levels needed more than
/// 192 KiB of a 256 KiB thread in an unoptimized build.
const MAX_CONTENT_ARRAY_NESTING: usize = 128;

/// Reads the next token of a content stream.
///
/// Returns `None`, the same answer as the end of the stream, when arrays
/// nest past [`MAX_CONTENT_ARRAY_NESTING`] or an array is never closed, so a
/// caller stops reading a stream it cannot make sense of.
fn next_content_token(bytes: &[u8], cursor: &mut usize) -> Option<ContentToken> {
    skip_whitespace_and_comments(bytes, cursor);
    if bytes.get(*cursor) != Some(&b'[') {
        return next_scalar_content_token(bytes, cursor);
    }
    // The arrays opened and not yet closed, outermost first.
    let mut open: Vec<Vec<ContentValue>> = Vec::new();
    loop {
        skip_whitespace_and_comments(bytes, cursor);
        match bytes.get(*cursor) {
            Some(b'[') => {
                if open.len() >= MAX_CONTENT_ARRAY_NESTING {
                    return None;
                }
                *cursor += 1;
                open.push(Vec::new());
            }
            Some(b']') => {
                *cursor += 1;
                let finished = ContentValue::Array(open.pop()?);
                match open.last_mut() {
                    Some(parent) => parent.push(finished),
                    None => return Some(ContentToken::Value(finished)),
                }
            }
            _ => {
                let item = match next_scalar_content_token(bytes, cursor)? {
                    ContentToken::Value(value) => value,
                    ContentToken::Operator(_) => ContentValue::Other,
                };
                open.last_mut()?.push(item);
            }
        }
    }
}

/// Reads one token that is not an array. The cursor must already be past
/// any whitespace.
fn next_scalar_content_token(bytes: &[u8], cursor: &mut usize) -> Option<ContentToken> {
    let byte = *bytes.get(*cursor)?;
    match byte {
        b'(' => {
            let mut parser = ValueParser::new(bytes, *cursor);
            let value = parser.parse_literal_string().ok()?;
            *cursor = parser.pos;
            Some(ContentToken::Value(ContentValue::String(value)))
        }
        b'<' if bytes.get(*cursor + 1) == Some(&b'<') => {
            *cursor += 2;
            Some(ContentToken::Operator("<<".to_string()))
        }
        b'>' if bytes.get(*cursor + 1) == Some(&b'>') => {
            *cursor += 2;
            Some(ContentToken::Operator(">>".to_string()))
        }
        b'<' => {
            let mut parser = ValueParser::new(bytes, *cursor);
            let value = parser.parse_hex_string().ok()?;
            *cursor = parser.pos;
            Some(ContentToken::Value(ContentValue::HexString(value)))
        }
        b'/' => {
            let mut parser = ValueParser::new(bytes, *cursor);
            let value = parser.parse_name().ok()?;
            *cursor = parser.pos;
            Some(ContentToken::Value(ContentValue::Name(value)))
        }
        b'+' | b'-' | b'.' | b'0'..=b'9' => {
            let mut parser = ValueParser::new(bytes, *cursor);
            let number = parser.parse_number_token().ok()?;
            *cursor = parser.pos;
            Some(ContentToken::Value(ContentValue::Number(number as f32)))
        }
        _ => {
            let start = *cursor;
            while *cursor < bytes.len() && !is_delimiter(bytes[*cursor]) {
                *cursor += 1;
            }
            if start == *cursor {
                *cursor += 1; // Force forward progress on unsupported delimiters
            }
            let token = String::from_utf8_lossy(&bytes[start..*cursor]).into_owned();
            Some(ContentToken::Operator(token))
        }
    }
}

fn content_value_text(value: &ContentValue, binding: Option<&FontBinding>) -> Option<String> {
    Some(content_value_text_with_fallback(value, binding)?.0)
}

/// Like `content_value_text`, but also returns the number of CMap-fallback chars
/// and the [`DecodingPath`] that produced them.
fn content_value_text_with_fallback(
    value: &ContentValue,
    binding: Option<&FontBinding>,
) -> Option<(String, u32, DecodingPath)> {
    match value {
        ContentValue::String(bytes) | ContentValue::HexString(bytes) => {
            if let Some(b) = binding {
                let (text, fallback, path) = b.decode_counting(bytes);
                return Some((text, fallback, path));
            }
            let text = decode_pdf_text(bytes);
            let fallback = text.chars().count() as u32;
            Some((text, fallback, DecodingPath::RawByteFallback))
        }
        _ => None,
    }
}

fn content_number(value: &ContentValue) -> Option<f32> {
    match value {
        ContentValue::Number(value) => Some(*value),
        _ => None,
    }
}

fn operands_as_pair(values: &[ContentValue]) -> Option<(f32, f32)> {
    let mut numbers = values.iter().filter_map(content_number).rev();
    let second = numbers.next()?;
    let first = numbers.next()?;
    Some((first, second))
}

/// The numeric operands of a color operator, as its components.
///
/// At most the last [`MAX_COLOR_COMPONENTS`] are kept.
fn operands_as_numbers(values: &[ContentValue]) -> Option<Vec<f64>> {
    let mut numbers: Vec<f64> = values
        .iter()
        .rev()
        .filter_map(|v| content_number(v).map(|n| n as f64))
        .take(MAX_COLOR_COMPONENTS)
        .collect();
    numbers.reverse();
    if numbers.is_empty() {
        None
    } else {
        Some(numbers)
    }
}

fn operands_as_six_numbers(values: &[ContentValue]) -> Option<ContentMatrix> {
    let numbers = values.iter().filter_map(content_number).collect::<Vec<_>>();
    let slice = numbers.get(numbers.len().checked_sub(6)?..)?;
    Some([slice[0], slice[1], slice[2], slice[3], slice[4], slice[5]])
}

fn operands_as_quad(values: &[ContentValue]) -> Option<(f32, f32, f32, f32)> {
    let numbers = values.iter().filter_map(content_number).collect::<Vec<_>>();
    let slice = numbers.get(numbers.len().checked_sub(4)?..)?;
    Some((slice[0], slice[1], slice[2], slice[3]))
}

#[cfg(feature = "rasterize")]
fn operands_as_rgb(values: &[ContentValue]) -> Option<(f32, f32, f32)> {
    let numbers = values.iter().filter_map(content_number).collect::<Vec<_>>();
    let slice = numbers.get(numbers.len().checked_sub(3)?..)?;
    Some((slice[0], slice[1], slice[2]))
}

fn last_name_operand(values: &[ContentValue]) -> Option<&str> {
    values.iter().rev().find_map(|value| match value {
        ContentValue::Name(name) => Some(name.as_str()),
        _ => None,
    })
}

#[cfg(feature = "rasterize")]
impl<'a> PageRasterizer<'a> {
    fn new(doc: &'a PdfDocument, crop_box: [f32; 4], dpi: f32) -> Result<Option<Self>> {
        let [x_min, y_min, _, _] = crop_box;
        let scale = dpi / 72.0;
        let Some((px_width, px_height, rgba_bytes)) = raster_surface_layout(crop_box, dpi) else {
            return Ok(None);
        };
        doc.decoded_byte_budget.charge_resource_bytes(rgba_bytes)?;
        // Paper is white. A PDF page begins on an opaque white medium, and its
        // content — including glyphs drawn as filled vector OUTLINES (the
        // DocuSign-signed contract class: 240k Béziers, zero Tj font ops) —
        // paints on top. Left transparent, the black glyph fills flatten to
        // black-on-black when the PNG is consumed (by OCR or a viewer), so a
        // page of readable black text rendered as a solid black raster. An
        // opaque white ground is what a physical page is and what every full
        // interpreter assumes.
        let Some(mut pixmap) = Pixmap::new(px_width, px_height) else {
            return Ok(None);
        };
        pixmap.fill(Color::WHITE);
        Ok(Some(Self {
            doc,
            base_ctm: [
                scale,
                0.0,
                0.0,
                -scale,
                -x_min * scale,
                px_height as f32 + y_min * scale,
            ],
            pixmap,
            px_width,
            px_height,
            state_stack: vec![GraphicsState::default()],
            unsaved_states: 0,
            overflow_state: None,
            path_builder: PathBuilder::new(),
            pending_clip: None,
            form_stack: Vec::new(),
            // The white ground is the canvas, NOT content: `painted` still
            // gates on real drawing. A page this fast path can't paint (e.g. a
            // pure image-XObject scan it doesn't composite) stays `painted=false`
            // → finish_png returns None → the caller falls through to the image
            // fallback, exactly as before. Only the background colour changed.
            painted: false,
        }))
    }

    fn finish_png(self) -> Result<Option<Vec<u8>>> {
        if !self.painted {
            return Ok(None);
        }
        let rgba_len = self.pixmap.data().len();
        self.doc
            .decoded_byte_budget
            .charge_resource_bytes(rgba_len)?;
        let mut rgba = Vec::new();
        rgba.try_reserve_exact(rgba_len)
            .map_err(|error| anyhow!("allocate bounded raster PNG input: {error}"))?;
        for pixel in self.pixmap.pixels() {
            let color = pixel.demultiply();
            rgba.extend_from_slice(&[color.red(), color.green(), color.blue(), color.alpha()]);
        }
        self.doc
            .decoded_byte_budget
            .encode_rgba_png(&rgba, self.px_width, self.px_height)
            .map(Some)
    }

    fn interpret_stream(&mut self, bytes: &[u8], resources: Option<&PdfDictionary>) -> Result<()> {
        let mut cursor = 0usize;
        let mut operands = Vec::new();
        while let Some(token) = next_content_token(bytes, &mut cursor) {
            match token {
                ContentToken::Value(value) => operands.push(value),
                ContentToken::Operator(operator) => {
                    self.execute_operator(operator.as_str(), &operands, resources)?;
                    operands.clear();
                }
            }
        }
        Ok(())
    }

    fn execute_operator(
        &mut self,
        operator: &str,
        operands: &[ContentValue],
        resources: Option<&PdfDictionary>,
    ) -> Result<()> {
        match operator {
            "q" => {
                if self.state_stack.len() >= MAX_GRAPHICS_STATE_DEPTH {
                    if self.unsaved_states == 0 {
                        self.overflow_state = self.state_stack.last().cloned();
                    }
                    self.unsaved_states = self.unsaved_states.saturating_add(1);
                } else {
                    let current = self.state_stack.last().cloned().unwrap_or_default();
                    self.state_stack.push(current);
                }
            }
            "Q" => {
                if self.unsaved_states > 0 {
                    self.unsaved_states -= 1;
                    if let (Some(saved), Some(current)) =
                        (self.overflow_state.as_ref(), self.state_stack.last_mut())
                    {
                        *current = saved.clone();
                    }
                    if self.unsaved_states == 0 {
                        self.overflow_state = None;
                    }
                } else if self.state_stack.len() > 1 {
                    self.state_stack.pop();
                }
            }
            "cm" => {
                if let Some(matrix) = operands_as_six_numbers(operands) {
                    if let Some(state) = self.state_stack.last_mut() {
                        state.ctm = matrix_multiply(state.ctm, matrix);
                    }
                }
            }
            "m" => {
                if let Some((x, y)) = operands_as_pair(operands) {
                    self.path_builder.move_to(x, y);
                }
            }
            "l" => {
                if let Some((x, y)) = operands_as_pair(operands) {
                    self.path_builder.line_to(x, y);
                }
            }
            "c" => {
                if let Some(matrix) = operands_as_six_numbers(operands) {
                    self.path_builder.cubic_to(
                        matrix[0], matrix[1], matrix[2], matrix[3], matrix[4], matrix[5],
                    );
                }
            }
            "h" => {
                self.path_builder.close();
            }
            "re" => {
                if let Some((x, y, width, height)) = operands_as_quad(operands) {
                    self.path_builder.move_to(x, y);
                    self.path_builder.line_to(x + width, y);
                    self.path_builder.line_to(x + width, y + height);
                    self.path_builder.line_to(x, y + height);
                    self.path_builder.close();
                }
            }
            "f" | "F" => self.execute_paint(Some(FillRule::Winding), false, false)?,
            "f*" => self.execute_paint(Some(FillRule::EvenOdd), false, false)?,
            "S" => self.execute_paint(None, true, false)?,
            "s" => self.execute_paint(None, true, true)?,
            "B" => self.execute_paint(Some(FillRule::Winding), true, false)?,
            "B*" => self.execute_paint(Some(FillRule::EvenOdd), true, false)?,
            "b" => self.execute_paint(Some(FillRule::Winding), true, true)?,
            "b*" => self.execute_paint(Some(FillRule::EvenOdd), true, true)?,
            "n" => {
                self.execute_paint(None, false, false)?;
            }
            "w" => {
                if let Some(width) = operands.iter().rev().find_map(content_number) {
                    if let Some(state) = self.state_stack.last_mut() {
                        state.stroke.width = width.max(0.001);
                    }
                }
            }
            "J" => {
                if let Some(cap) = operands.iter().rev().find_map(content_number) {
                    if let Some(state) = self.state_stack.last_mut() {
                        state.stroke.line_cap = match cap as i32 {
                            1 => LineCap::Round,
                            2 => LineCap::Square,
                            _ => LineCap::Butt,
                        };
                    }
                }
            }
            "j" => {
                if let Some(join) = operands.iter().rev().find_map(content_number) {
                    if let Some(state) = self.state_stack.last_mut() {
                        state.stroke.line_join = match join as i32 {
                            1 => LineJoin::Round,
                            2 => LineJoin::Bevel,
                            _ => LineJoin::Miter,
                        };
                    }
                }
            }
            "rg" => {
                if let Some((red, green, blue)) = operands_as_rgb(operands) {
                    if let Some(state) = self.state_stack.last_mut() {
                        state.fill_paint.set_color(pdf_color(red, green, blue));
                    }
                }
            }
            "RG" => {
                if let Some((red, green, blue)) = operands_as_rgb(operands) {
                    if let Some(state) = self.state_stack.last_mut() {
                        state.stroke_paint.set_color(pdf_color(red, green, blue));
                    }
                }
            }
            "g" => {
                if let Some(gray) = operands.iter().rev().find_map(content_number) {
                    if let Some(state) = self.state_stack.last_mut() {
                        let color = pdf_color(gray, gray, gray);
                        state.fill_paint.set_color(color);
                    }
                }
            }
            "G" => {
                if let Some(gray) = operands.iter().rev().find_map(content_number) {
                    if let Some(state) = self.state_stack.last_mut() {
                        let color = pdf_color(gray, gray, gray);
                        state.stroke_paint.set_color(color);
                    }
                }
            }
            "W" => {
                self.pending_clip = Some(FillRule::Winding);
            }
            "W*" => {
                self.pending_clip = Some(FillRule::EvenOdd);
            }
            "Do" => {
                let Some(name) = last_name_operand(operands) else {
                    return Ok(());
                };
                let Some(form) =
                    self.doc
                        .resolve_form_xobject(resources, name, self.form_stack.as_slice())?
                else {
                    return Ok(());
                };
                let mut state = self.state_stack.last().cloned().unwrap_or_default();
                state.ctm = matrix_multiply(state.ctm, form.matrix);
                self.state_stack.push(state);
                self.form_stack.push(form.object_id);
                let nested_result =
                    self.interpret_stream(form.bytes.as_slice(), form.resources.as_ref());
                self.form_stack.pop();
                self.state_stack.pop();
                nested_result?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Refuse one more clip mask when the masks alive would outgrow the budget.
    ///
    /// A clip mask is one byte per surface pixel. Saved states share a mask
    /// until a nested clip changes it, so `W n q W n q …` holds one distinct
    /// mask per level that clips again, and only those are counted. The masks
    /// are released when the page is done, so they are checked against the
    /// decoded-byte budget rather than charged to it.
    fn ensure_clip_mask_budget(&self) -> Result<()> {
        let mask_bytes = (self.px_width as usize).saturating_mul(self.px_height as usize);
        let mut distinct: Vec<*const Mask> = self
            .state_stack
            .iter()
            .filter_map(|state| state.clip_mask.as_ref().map(Rc::as_ptr))
            .collect();
        distinct.sort_unstable();
        distinct.dedup();
        let alive = distinct.len() + 1;
        self.doc
            .decoded_byte_budget
            .ensure_available(alive.saturating_mul(mask_bytes))
    }

    fn execute_paint(
        &mut self,
        fill_rule: Option<FillRule>,
        stroke: bool,
        close: bool,
    ) -> Result<()> {
        if close {
            self.path_builder.close();
        }
        let pending_clip = self.pending_clip.take();
        let builder = std::mem::replace(&mut self.path_builder, PathBuilder::new());
        let Some(path) = builder.finish() else {
            self.path_builder = PathBuilder::new();
            return Ok(());
        };
        // A clip allocates a mask when the state has none, or when the one
        // it has is shared with a saved state and must be copied to change.
        if pending_clip.is_some()
            && self.state_stack.last().is_some_and(|state| {
                state
                    .clip_mask
                    .as_ref()
                    .is_none_or(|mask| Rc::strong_count(mask) > 1)
            })
        {
            self.ensure_clip_mask_budget()?;
        }
        let Some(state) = self.state_stack.last_mut() else {
            self.path_builder = PathBuilder::new();
            return Ok(());
        };
        let transform = pdf_matrix_to_skia(matrix_multiply(self.base_ctm, state.ctm));
        if let Some(rule) = pending_clip {
            if let Some(mask) = state.clip_mask.as_mut() {
                Rc::make_mut(mask).intersect_path(&path, rule, true, transform);
            } else if let Some(mut mask) = Mask::new(self.px_width, self.px_height) {
                mask.fill_path(&path, rule, true, transform);
                state.clip_mask = Some(Rc::new(mask));
            }
        }
        let clip_mask = state.clip_mask.as_deref();
        if let Some(rule) = fill_rule {
            self.pixmap
                .fill_path(&path, &state.fill_paint, rule, transform, clip_mask);
            self.painted = true;
        }
        if stroke {
            self.pixmap.stroke_path(
                &path,
                &state.stroke_paint,
                &state.stroke,
                transform,
                clip_mask,
            );
            self.painted = true;
        }
        self.path_builder = PathBuilder::new();
        Ok(())
    }
}

impl<'a> TablePrimitiveExtractor<'a> {
    fn new(doc: &'a PdfDocument) -> Self {
        Self {
            doc,
            state_stack: vec![PrimitiveState::default()],
            unsaved_states: 0,
            overflow_state: None,
            path_segments: Vec::new(),
            current_point: None,
            subpath_start: None,
            segments: Vec::new(),
            form_stack: Vec::new(),
            placed_images: Vec::new(),
        }
    }

    fn finish(mut self) -> (Vec<LineSegment>, Vec<PdfImagePlacement>) {
        self.clear_path();
        (self.segments, self.placed_images)
    }

    fn interpret_stream(&mut self, bytes: &[u8], resources: Option<&PdfDictionary>) -> Result<()> {
        let mut cursor = 0usize;
        let mut operands = Vec::new();
        while let Some(token) = next_content_token(bytes, &mut cursor) {
            match token {
                ContentToken::Value(value) => operands.push(value),
                ContentToken::Operator(operator) => {
                    self.execute_operator(operator.as_str(), &operands, resources)?;
                    operands.clear();
                }
            }
        }
        Ok(())
    }

    fn execute_operator(
        &mut self,
        operator: &str,
        operands: &[ContentValue],
        resources: Option<&PdfDictionary>,
    ) -> Result<()> {
        match operator {
            "q" => {
                // One state per `q` with no limit made a stream of nothing
                // but `q` cost twelve times its own size in saved matrices.
                let state = self.state_stack.last().copied().unwrap_or_default();
                if self.state_stack.len() >= MAX_GRAPHICS_STATE_DEPTH {
                    if self.unsaved_states == 0 {
                        self.overflow_state = Some(state);
                    }
                    self.unsaved_states = self.unsaved_states.saturating_add(1);
                } else {
                    self.state_stack.push(state);
                }
            }
            "Q" => {
                if self.unsaved_states > 0 {
                    self.unsaved_states -= 1;
                    if let (Some(saved), Some(current)) =
                        (self.overflow_state, self.state_stack.last_mut())
                    {
                        *current = saved;
                    }
                    if self.unsaved_states == 0 {
                        self.overflow_state = None;
                    }
                } else if self.state_stack.len() > 1 {
                    self.state_stack.pop();
                }
            }
            "cm" => {
                if let Some(matrix) = operands_as_six_numbers(operands) {
                    if let Some(state) = self.state_stack.last_mut() {
                        state.ctm = matrix_multiply(state.ctm, matrix);
                    }
                }
            }
            "m" => {
                if let Some((x, y)) = operands_as_pair(operands) {
                    self.current_point = Some((x, y));
                    self.subpath_start = Some((x, y));
                }
            }
            "l" => {
                if let Some((x, y)) = operands_as_pair(operands) {
                    if let Some((cx, cy)) = self.current_point {
                        self.path_segments.push(LineSegment {
                            x0: cx,
                            y0: cy,
                            x1: x,
                            y1: y,
                        });
                    }
                    self.current_point = Some((x, y));
                }
            }
            "c" => {
                if let Some(matrix) = operands_as_six_numbers(operands) {
                    self.current_point = Some((matrix[4], matrix[5]));
                    if self.subpath_start.is_none() {
                        self.subpath_start = self.current_point;
                    }
                }
            }
            "h" => {
                self.close_subpath();
            }
            "re" => {
                if let Some((x, y, width, height)) = operands_as_quad(operands) {
                    self.path_segments.extend([
                        LineSegment {
                            x0: x,
                            y0: y,
                            x1: x + width,
                            y1: y,
                        },
                        LineSegment {
                            x0: x + width,
                            y0: y,
                            x1: x + width,
                            y1: y + height,
                        },
                        LineSegment {
                            x0: x + width,
                            y0: y + height,
                            x1: x,
                            y1: y + height,
                        },
                        LineSegment {
                            x0: x,
                            y0: y + height,
                            x1: x,
                            y1: y,
                        },
                    ]);
                    self.current_point = Some((x, y));
                    self.subpath_start = Some((x, y));
                }
            }
            "S" | "s" | "B" | "B*" | "b" | "b*" => {
                if matches!(operator, "s" | "b" | "b*") {
                    self.close_subpath();
                }
                self.flush_path();
            }
            "f" | "F" | "f*" => {
                self.clear_path();
            }
            "n" => {
                self.clear_path();
            }
            "Do" => {
                let Some(name) = last_name_operand(operands) else {
                    return Ok(());
                };

                // 1. Try resolving as a nested vector form
                if let Some(form) =
                    self.doc
                        .resolve_form_xobject(resources, name, self.form_stack.as_slice())?
                {
                    let mut state = self.state_stack.last().copied().unwrap_or_default();
                    state.ctm = matrix_multiply(state.ctm, form.matrix);
                    self.state_stack.push(state);
                    self.form_stack.push(form.object_id);
                    let nested_result =
                        self.interpret_stream(form.bytes.as_slice(), form.resources.as_ref());
                    self.form_stack.pop();
                    self.state_stack.pop();
                    nested_result?;
                    return Ok(());
                }

                // 2. Try resolving as a raster image
                if let Some(object_number) = self.doc.resolve_image_xobject(resources, name) {
                    let state = self.state_stack.last().copied().unwrap_or_default();
                    let (x, y) = matrix_position(state.ctm);
                    let w = state.ctm[0]; // Scale X
                    let h = state.ctm[3]; // Scale Y
                    self.placed_images.push(PdfImagePlacement {
                        name: name.to_string(),
                        object_number,
                        x,
                        y,
                        w,
                        h,
                    });
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn close_subpath(&mut self) {
        if let (Some((cx, cy)), Some((sx, sy))) = (self.current_point, self.subpath_start) {
            if (cx - sx).abs() > f32::EPSILON || (cy - sy).abs() > f32::EPSILON {
                self.path_segments.push(LineSegment {
                    x0: cx,
                    y0: cy,
                    x1: sx,
                    y1: sy,
                });
            }
            self.current_point = Some((sx, sy));
        }
    }

    fn flush_path(&mut self) {
        let ctm = self.state_stack.last().copied().unwrap_or_default().ctm;
        self.segments
            .extend(self.path_segments.drain(..).map(|segment| {
                let (x0, y0) = transform_point(ctm, segment.x0, segment.y0);
                let (x1, y1) = transform_point(ctm, segment.x1, segment.y1);
                LineSegment { x0, y0, x1, y1 }
            }));
        self.current_point = None;
        self.subpath_start = None;
    }

    fn clear_path(&mut self) {
        self.path_segments.clear();
        self.current_point = None;
        self.subpath_start = None;
    }
}

fn build_text_run(
    page_index: usize,
    stream_index: usize,
    operator: &str,
    operand: Option<&ContentValue>,
    state: &ContentState,
    font_bindings: &HashMap<String, FontBinding>,
) -> Option<PdfTextRun> {
    let binding = state
        .font_name
        .as_ref()
        .and_then(|name| font_bindings.get(name));
    let (text, cmap_fallback_chars, decoding_path) =
        content_value_text_with_fallback(operand?, binding)?;
    if text.is_empty() {
        return None;
    }
    let position = matrix_position(matrix_multiply(state.ctm, state.text_matrix));
    let mut run = PdfTextRun {
        page_index,
        stream_index,
        operator: operator.to_string(),
        text: text.clone(),
        cmap_fallback_chars,
        decoding_path,
        font: state
            .font_name
            .clone()
            .or_else(|| binding.map(|font| font.resource_name.clone())),
        font_object: binding.map(|font| font.object_number),
        font_generation: binding.map(|font| font.generation),
        font_size: state.font_size,
        x: Some(position.0),
        y: Some(position.1),
        w: Default::default(),
        h: Default::default(),
        fill_color: state.fill_color.clone(),
        stroke_color: state.stroke_color.clone(),
        render_mode: state.render_mode,
        char_spacing: state.char_spacing,
        word_spacing: state.word_spacing,
        horizontal_scaling: state.horizontal_scaling,
        text_rise: state.text_rise,
    };
    if let Some(sz) = state.font_size {
        run.h = Some(sz);
        run.w = Some(text.chars().count() as f32 * sz * 0.5); // Approximation for bounding box
    }
    Some(run)
}

fn build_text_run_from_array(
    page_index: usize,
    stream_index: usize,
    operator: &str,
    operand: Option<&ContentValue>,
    state: &ContentState,
    font_bindings: &HashMap<String, FontBinding>,
) -> Option<PdfTextRun> {
    let items = match operand? {
        ContentValue::Array(items) => items,
        _ => return None,
    };
    let binding = state
        .font_name
        .as_ref()
        .and_then(|name| font_bindings.get(name));

    // PDF 32000-1 §9.4.3: a number in a TJ array translates the text matrix
    // by `-n/1000 * font_size` text-space units (horizontally for the usual
    // writing mode). A negative `n` therefore pushes the cursor RIGHT past
    // the previous glyph's natural advance — a visual gap that producers
    // (Typst, modern xelatex, recent InDesign) use as a word break instead
    // of emitting a literal U+0020 glyph.
    //
    // Kerning tightening (n > 0) and incidental positive kerning (small
    // negative n) must NOT become spaces. Typesetters keep kerning under
    // ~50 thousandths-of-em; word spacing is usually 250-300. A threshold
    // of 100 sits cleanly between the two regimes and matches what pdf.js
    // and pdftotext use by default.
    const TJ_SPACE_THRESHOLD: f32 = 100.0;

    let mut text = String::new();
    let mut cmap_fallback_chars: u32 = 0;
    let mut path_candidates: Vec<DecodingPath> = Vec::new();
    let mut pending_advance: f32 = 0.0;
    let mut last_char_is_space = true; // suppress leading space
    for value in items {
        match value {
            ContentValue::Number(n) => {
                pending_advance += -n;
            }
            other => {
                if let Some((s, fb, p)) = content_value_text_with_fallback(other, binding) {
                    if !s.is_empty() {
                        if pending_advance >= TJ_SPACE_THRESHOLD && !last_char_is_space {
                            text.push(' ');
                        }
                        pending_advance = 0.0;
                        cmap_fallback_chars = cmap_fallback_chars.saturating_add(fb);
                        path_candidates.push(p);
                        last_char_is_space = s.chars().next_back().is_some_and(char::is_whitespace);
                        text.push_str(&s);
                    }
                }
            }
        }
    }
    let decoding_path = path_candidates
        .into_iter()
        .min_by_key(decoding_path_priority)
        .unwrap_or(DecodingPath::RawByteFallback);
    if text.is_empty() {
        return None;
    }
    let position = matrix_position(matrix_multiply(state.ctm, state.text_matrix));
    let mut run = PdfTextRun {
        page_index,
        stream_index,
        operator: operator.to_string(),
        text: text.clone(),
        cmap_fallback_chars,
        decoding_path,
        font: state
            .font_name
            .clone()
            .or_else(|| binding.map(|font| font.resource_name.clone())),
        font_object: binding.map(|font| font.object_number),
        font_generation: binding.map(|font| font.generation),
        font_size: state.font_size,
        x: Some(position.0),
        y: Some(position.1),
        w: Default::default(),
        h: Default::default(),
        fill_color: state.fill_color.clone(),
        stroke_color: state.stroke_color.clone(),
        render_mode: state.render_mode,
        char_spacing: state.char_spacing,
        word_spacing: state.word_spacing,
        horizontal_scaling: state.horizontal_scaling,
        text_rise: state.text_rise,
    };
    if let Some(sz) = state.font_size {
        run.h = Some(sz);
        run.w = Some(text.chars().count() as f32 * sz * 0.5); // Approximation for bounding box
    }
    Some(run)
}

fn parse_cmap(bytes: &[u8], object_number: u32, generation: u16) -> Option<PdfCMap> {
    let text = String::from_utf8_lossy(bytes);
    let mut name = None;
    let mut code_space_ranges = Vec::new();
    let mut entries = Vec::new();

    // Extract CMap name
    if let Some(pos) = text.find("/CMapName") {
        name = text[pos..]
            .split_whitespace()
            .nth(1)
            .map(|value| value.trim_start_matches('/').to_string());
    }

    // Parse codespacerange sections
    {
        let mut rest = text.as_ref();
        while let Some(begin) = rest.find("begincodespacerange") {
            let after = &rest[begin + "begincodespacerange".len()..];
            let end_pos = after.find("endcodespacerange").unwrap_or(after.len());
            let section = &after[..end_pos];
            for pair in parse_all_hex_pairs(section) {
                code_space_ranges.push(PdfCMapCodeSpaceRange {
                    start: pair.0,
                    end: pair.1,
                });
            }
            rest = &after[end_pos..];
        }
    }

    // Parse bfchar sections — tolerant of all-on-one-line or one-per-line
    {
        let mut rest = text.as_ref();
        while let Some(begin) = rest.find("beginbfchar") {
            let after = &rest[begin + "beginbfchar".len()..];
            let end_pos = after.find("endbfchar").unwrap_or(after.len());
            let section = &after[..end_pos];
            for pair in parse_all_hex_pairs(section) {
                entries.push(PdfCMapEntry::Char {
                    src: pair.0,
                    dst: pair.1,
                });
            }
            rest = &after[end_pos..];
        }
    }

    // Parse bfrange sections
    {
        let mut rest = text.as_ref();
        while let Some(begin) = rest.find("beginbfrange") {
            let after = &rest[begin + "beginbfrange".len()..];
            let end_pos = after.find("endbfrange").unwrap_or(after.len());
            let section = &after[..end_pos];
            // bfrange entries: each is <start> <end> <dst_start> or <start> <end> [<d1> <d2>...]
            for line in section.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if trimmed.contains('[') {
                    if let Some((start, end, dsts)) = parse_bfrange_array_line(trimmed) {
                        entries.push(PdfCMapEntry::RangeArray { start, end, dsts });
                    }
                } else if let Some((start, end, dst_start)) = parse_bfrange_line(trimmed) {
                    entries.push(PdfCMapEntry::Range {
                        start,
                        end,
                        dst_start,
                    });
                }
            }
            rest = &after[end_pos..];
        }
    }
    Some(PdfCMap {
        object_number,
        generation,
        name,
        code_space_ranges,
        entries,
    })
}

fn parse_two_hex_tokens(line: &str) -> Option<(String, String)> {
    let tokens = line
        .split_whitespace()
        .filter(|token| token.starts_with('<') && token.ends_with('>'))
        .collect::<Vec<_>>();
    Some((tokens.first()?.to_string(), tokens.get(1)?.to_string()))
}

/// Parse all `<src> <dst>` hex pairs from a line.
/// Some PDF generators pack many bfchar entries on a single line.
fn parse_all_hex_pairs(line: &str) -> Vec<(String, String)> {
    let tokens: Vec<&str> = line
        .split_whitespace()
        .filter(|token| token.starts_with('<') && token.ends_with('>'))
        .collect();
    tokens
        .chunks(2)
        .filter_map(|pair| {
            if pair.len() == 2 {
                Some((pair[0].to_string(), pair[1].to_string()))
            } else {
                None
            }
        })
        .collect()
}

fn parse_bfrange_line(line: &str) -> Option<(String, String, String)> {
    let tokens = line
        .split_whitespace()
        .filter(|token| token.starts_with('<') && token.ends_with('>'))
        .collect::<Vec<_>>();
    Some((
        tokens.first()?.to_string(),
        tokens.get(1)?.to_string(),
        tokens.get(2)?.to_string(),
    ))
}

fn parse_bfrange_array_line(line: &str) -> Option<(String, String, Vec<String>)> {
    let parts = line.split('[').collect::<Vec<_>>();
    let prefix = parts.first()?.trim();
    let array = parts.get(1)?.trim().trim_end_matches(']');
    let (start, end) = parse_two_hex_tokens(prefix)?;
    let dsts = array
        .split_whitespace()
        .filter(|token| token.starts_with('<') && token.ends_with('>'))
        .map(str::to_string)
        .collect::<Vec<_>>();
    Some((start, end, dsts))
}

fn decode_pdf_text(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return decode_utf16_be(&bytes[2..]);
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return decode_utf16_le(&bytes[2..]);
    }
    if bytes.len() >= 2 && bytes.iter().step_by(2).all(|byte| *byte == 0) {
        return decode_utf16_be(bytes);
    }
    if bytes.len() >= 2 && bytes.iter().skip(1).step_by(2).all(|byte| *byte == 0) {
        return decode_utf16_le(bytes);
    }
    bytes.iter().map(|byte| *byte as char).collect()
}

fn decode_utf16_be(bytes: &[u8]) -> String {
    String::from_utf16_lossy(
        &bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|chunk| u16::from_be_bytes(*chunk))
            .collect::<Vec<_>>(),
    )
}

fn decode_utf16_le(bytes: &[u8]) -> String {
    String::from_utf16_lossy(
        &bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|chunk| u16::from_le_bytes(*chunk))
            .collect::<Vec<_>>(),
    )
}

fn is_textish(bytes: &[u8]) -> bool {
    let sample = &bytes[..bytes.len().min(256)];
    let printable = sample
        .iter()
        .filter(|byte| matches!(**byte, b'\n' | b'\r' | b'\t' | 0x20..=0x7E))
        .count();
    printable * 100 / sample.len().max(1) >= 85
}

fn preview_text(bytes: &[u8], max_len: usize) -> String {
    let mut text = String::from_utf8_lossy(&bytes[..bytes.len().min(max_len)]).into_owned();
    text = text.replace('\r', "\\r").replace('\n', "\\n");
    text
}

fn to_upper_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(&mut out, "{:02X}", byte);
    }
    out
}

fn matrix_multiply(lhs: ContentMatrix, rhs: ContentMatrix) -> ContentMatrix {
    [
        lhs[0] * rhs[0] + lhs[2] * rhs[1],
        lhs[1] * rhs[0] + lhs[3] * rhs[1],
        lhs[0] * rhs[2] + lhs[2] * rhs[3],
        lhs[1] * rhs[2] + lhs[3] * rhs[3],
        lhs[0] * rhs[4] + lhs[2] * rhs[5] + lhs[4],
        lhs[1] * rhs[4] + lhs[3] * rhs[5] + lhs[5],
    ]
}

fn matrix_translate(base: ContentMatrix, tx: f32, ty: f32) -> ContentMatrix {
    matrix_multiply(base, [1.0, 0.0, 0.0, 1.0, tx, ty])
}

fn matrix_position(matrix: ContentMatrix) -> (f32, f32) {
    (matrix[4], matrix[5])
}

fn transform_point(matrix: ContentMatrix, x: f32, y: f32) -> (f32, f32) {
    (
        matrix[0] * x + matrix[2] * y + matrix[4],
        matrix[1] * x + matrix[3] * y + matrix[5],
    )
}

#[cfg(feature = "rasterize")]
fn pdf_matrix_to_skia(matrix: ContentMatrix) -> SkTransform {
    SkTransform::from_row(
        matrix[0], matrix[2], matrix[1], matrix[3], matrix[4], matrix[5],
    )
}

fn bytes_to_u32(bytes: &[u8]) -> u32 {
    bytes
        .iter()
        .fold(0u32, |value, byte| (value << 8) | u32::from(*byte))
}

fn hex_token_bytes(token: &str) -> Result<Vec<u8>> {
    let token = token.trim();
    let token = token
        .strip_prefix('<')
        .and_then(|token| token.strip_suffix('>'))
        .ok_or_else(|| anyhow!("invalid hex token"))?;
    decode_ascii_hex(token.as_bytes())
}

fn hex_token_u32(token: &str) -> Option<u32> {
    let bytes = hex_token_bytes(token).ok()?;
    Some(bytes_to_u32(&bytes))
}

fn unicode_hex_token(token: &str) -> Option<String> {
    let bytes = hex_token_bytes(token).ok()?;
    // ToUnicode CMap dst values are UTF-16BE per PDF 32000-1 §9.10.3. The
    // `decode_pdf_text` heuristic only recognises UTF-16BE when there's
    // a BOM or one byte of each pair is zero — so `<2019>` (bytes 0x20
    // 0x19) falls through to per-byte Latin-1 and produces " \u{0019}"
    // (SPACE + DEVICE CONTROL 3) instead of "\u{2019}" RIGHT SINGLE
    // QUOTATION MARK. Decode as UTF-16BE directly when the payload is
    // even-length; odd-length payloads (rare, malformed CMaps) keep the
    // raw-byte fallback so we don't drop data.
    Some(if !bytes.is_empty() && bytes.len() % 2 == 0 {
        decode_utf16_be(&bytes)
    } else {
        bytes.iter().map(|byte| *byte as char).collect()
    })
}

fn unicode_hex_scalar(token: &str) -> Option<u32> {
    unicode_hex_token(token)?.chars().next().map(u32::from)
}

fn image_magic(raw: &[u8]) -> Option<&'static str> {
    if raw.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpeg")
    } else if raw.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("png")
    } else if raw.starts_with(b"GIF87a") || raw.starts_with(b"GIF89a") {
        Some("gif")
    } else if raw.starts_with(b"II*\0") || raw.starts_with(b"MM\0*") {
        Some("tiff")
    } else if raw.starts_with(b"BM") {
        Some("bmp")
    } else {
        None
    }
}

fn font_magic(raw: &[u8]) -> Option<&'static str> {
    if raw.starts_with(&[0x00, 0x01, 0x00, 0x00]) {
        Some("truetype")
    } else if raw.starts_with(b"OTTO") {
        Some("opentype-cff")
    } else if raw.starts_with(b"true") {
        Some("truetype")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{write::ZlibEncoder, Compression};
    use std::io::{Cursor, Write};

    /// The reference the `as_chunks` decoders replaced: `chunks_exact(2)`, which
    /// ignores a trailing odd byte. The decoders sit on the text-extraction
    /// path, so "same pairs, same dropped remainder" is asserted, not assumed.
    fn reference_utf16(bytes: &[u8], big_endian: bool) -> String {
        let units: Vec<u16> = bytes
            .chunks(2)
            .filter(|chunk| chunk.len() == 2)
            .map(|chunk| {
                if big_endian {
                    u16::from_be_bytes([chunk[0], chunk[1]])
                } else {
                    u16::from_le_bytes([chunk[0], chunk[1]])
                }
            })
            .collect();
        String::from_utf16_lossy(&units)
    }

    #[test]
    fn utf16_decoders_match_the_chunked_reference_on_even_odd_and_tiny_inputs() {
        // "Aé" + U+1F600 as a surrogate pair, big-endian.
        let even_be: [u8; 8] = [0x00, 0x41, 0x00, 0xE9, 0xD8, 0x3D, 0xDE, 0x00];
        let mut even_le = even_be;
        for pair in even_le.chunks_mut(2) {
            pair.swap(0, 1);
        }
        let mut cases: Vec<Vec<u8>> = vec![Vec::new(), vec![0x41], even_be.to_vec()];
        // Every odd-length prefix and the even input with one stray trailing byte.
        for len in 1..=even_be.len() {
            cases.push(even_be[..len].to_vec());
        }
        let mut stray = even_be.to_vec();
        stray.push(0x7F);
        cases.push(stray);
        // A lone high surrogate, which must stay lossy in both.
        cases.push(vec![0xD8, 0x3D, 0x00]);

        for bytes in &cases {
            assert_eq!(
                decode_utf16_be(bytes),
                reference_utf16(bytes, true),
                "BE {bytes:?}"
            );
        }
        let mut le_cases = cases.clone();
        le_cases.push(even_le.to_vec());
        for bytes in &le_cases {
            assert_eq!(
                decode_utf16_le(bytes),
                reference_utf16(bytes, false),
                "LE {bytes:?}"
            );
        }
        // The odd trailing byte is dropped, not decoded and not an error.
        assert_eq!(decode_utf16_be(&[0x00, 0x41, 0x7F]), "A");
        assert_eq!(decode_utf16_le(&[0x41, 0x00, 0x7F]), "A");
    }

    #[test]
    fn bounded_path_reader_stops_one_byte_past_document_limit() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("oversized.pdf");
        std::fs::write(&path, b"%PDF-1.7\n0123456789abcdef\n").unwrap();
        let open_error = PdfDocument::open(
            &path,
            ParseOptions {
                max_document_bytes: 16,
                ..ParseOptions::default()
            },
        )
        .expect_err("path input must be rejected at its source-byte limit");
        let open_error_chain = format!("{open_error:#}");
        assert!(open_error_chain.contains("max_document_bytes 16"));

        let mut source = Cursor::new(vec![b'P'; 64]);

        let error = read_pdf_bytes_bounded(&mut source, 16)
            .expect_err("bounded path read must reject byte 17");

        assert!(error.to_string().contains("max_document_bytes 16"));
        assert_eq!(
            source.position(),
            17,
            "reader must not consume the full input"
        );
    }

    /// Regression: a page whose only content is a BLACK vector fill (exactly
    /// how DocuSign-signed digital contracts draw their text — outlined glyphs,
    /// zero Tj font ops) must render as black ink on a WHITE ground. Before the
    /// white-paper fix the pixmap began transparent, so the black fill flattened
    /// to black-on-black and the whole page came out solid black — feeding OCR a
    /// black image and reading nothing. Corners must be white (paper), the
    /// centre black (ink).
    #[cfg(feature = "rasterize")]
    #[test]
    fn rasterizes_black_vector_fill_on_white_paper() {
        // Minimal one-page PDF: MediaBox 100×100, content fills the middle
        // 50×50 square black (default fill colour) — no fonts, just paths.
        let pdf = br#"%PDF-1.4
1 0 obj
<< /Type /Catalog /Pages 2 0 R >>
endobj
2 0 obj
<< /Type /Pages /Kids [3 0 R] /Count 1 >>
endobj
3 0 obj
<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R /Resources << >> >>
endobj
4 0 obj
<< /Length 34 >>
stream
0 0 0 rg
25 25 50 50 re
f
endstream
endobj
"#;
        let doc = PdfDocument::from_slice(pdf, ParseOptions::default()).unwrap();
        let png = doc
            .page_rasterized_png(0, 72.0)
            .expect("rasterize ok")
            .expect("some png");
        let img = image::load_from_memory(&png).unwrap().to_luma8();
        let (w, h) = (img.width(), img.height());
        let corner = img.get_pixel(1, 1).0[0];
        let centre = img.get_pixel(w / 2, h / 2).0[0];
        assert!(
            corner > 220,
            "paper corner must be white, got luma {corner}"
        );
        assert!(centre < 40, "ink centre must be black, got luma {centre}");
    }

    #[test]
    fn parses_xref_free_content_stream() {
        let pdf = br#"%PDF-1.4
1 0 obj
<< /Type /Catalog >>
endobj
2 0 obj
<< /Length 40 >>
stream
BT
/F1 12 Tf
72 720 Td
(Hello World) Tj
ET
endstream
endobj
"#;

        let doc = PdfDocument::from_slice(pdf, ParseOptions::default()).unwrap();
        assert_eq!(doc.objects.len(), 2);
        assert_eq!(doc.streams.len(), 1);
        assert_eq!(doc.streams[0].kind, PdfStreamKind::Content);
        assert_eq!(doc.text_fragments.len(), 1);
        assert_eq!(doc.text_fragments[0].text, "Hello World");
        assert_eq!(doc.text_fragments[0].font.as_deref(), Some("F1"));
    }

    #[test]
    fn respects_extract_text_option() {
        let pdf = br#"%PDF-1.4
1 0 obj
<< /Type /Catalog >>
endobj
2 0 obj
<< /Length 40 >>
stream
BT
/F1 12 Tf
72 720 Td
(Hidden Text) Tj
ET
endstream
endobj
"#;

        let doc = PdfDocument::from_slice(
            pdf,
            ParseOptions {
                extract_text: false,
                ..ParseOptions::default()
            },
        )
        .unwrap();

        assert_eq!(doc.streams.len(), 1);
        assert!(doc.text_fragments.is_empty());
    }

    #[test]
    fn decodes_flate_content_stream() {
        let content = b"BT\n/F1 12 Tf\n(Compressed) Tj\nET\n";
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(content).unwrap();
        let encoded = encoder.finish().unwrap();
        let pdf = format!(
            "%PDF-1.7\n1 0 obj\n<< /Filter /FlateDecode /Length {} >>\nstream\n",
            encoded.len()
        )
        .into_bytes();
        let mut pdf = pdf;
        pdf.extend_from_slice(&encoded);
        pdf.extend_from_slice(b"\nendstream\nendobj\n");

        let doc = PdfDocument::from_bytes(pdf, ParseOptions::default()).unwrap();
        assert_eq!(doc.text_fragments[0].text, "Compressed");
        assert!(matches!(
            doc.streams[0].decode_status,
            DecodeStatus::Decoded
        ));
    }

    #[test]
    fn decodes_asciihex_then_flate_filter_chain() {
        let content = b"BT\n<00480069> Tj\nET\n";
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(content).unwrap();
        let flated = encoder.finish().unwrap();
        let hex = to_upper_hex(&flated) + ">";
        let pdf = format!(
            "%PDF-1.7\n7 0 obj\n<< /Filter [/ASCIIHexDecode /FlateDecode] /Length {} >>\nstream\n{}\nendstream\nendobj\n",
            hex.len(),
            hex
        );

        let doc = PdfDocument::from_slice(pdf.as_bytes(), ParseOptions::default()).unwrap();
        assert_eq!(doc.text_fragments[0].text, "Hi");
    }

    #[test]
    fn parses_cmap_stream() {
        let pdf = br#"%PDF-1.4
5 0 obj
<< /Type /CMap /Length 95 >>
stream
/CMapName /Identity-H
1 begincodespacerange
<0000> <FFFF>
endcodespacerange
2 beginbfchar
<0048> <0048>
<0069> <0069>
endbfchar
endcmap
endstream
endobj
"#;

        let doc = PdfDocument::from_slice(pdf, ParseOptions::default()).unwrap();
        assert_eq!(doc.cmaps.len(), 1);
        assert_eq!(doc.cmaps[0].name.as_deref(), Some("Identity-H"));
        assert_eq!(doc.cmaps[0].entries.len(), 2);
    }

    #[test]
    fn survives_missing_endobj_after_valid_object() {
        let pdf = br#"%PDF-1.4
1 0 obj
<< /Length 18 >>
stream
BT
(Ok) Tj
ET
endstream
endobj
2 0 obj
<< /Length 5 >>
stream
oops
"#;

        let doc = PdfDocument::from_slice(pdf, ParseOptions::default()).unwrap();
        assert!(!doc.objects.is_empty());
        assert_eq!(doc.text_fragments[0].text, "Ok");
    }

    #[test]
    fn treats_dctdecode_as_raw_passthrough() {
        let jpeg = vec![
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00,
        ];
        let pdf = format!(
            "%PDF-1.7\n14 0 obj\n<< /Subtype /Image /Filter /DCTDecode /Width 1 /Height 1 /Length {} >>\nstream\n",
            jpeg.len()
        )
        .into_bytes();
        let mut pdf = pdf;
        pdf.extend_from_slice(&jpeg);
        pdf.extend_from_slice(b"\nendstream\nendobj\n");

        let doc = PdfDocument::from_bytes(pdf, ParseOptions::default()).unwrap();
        assert_eq!(doc.streams.len(), 1);
        assert_eq!(doc.streams[0].kind, PdfStreamKind::Image);
        assert!(matches!(doc.streams[0].decode_status, DecodeStatus::Raw));
    }

    #[test]
    fn preserves_page_tree_order() {
        let pdf = br#"%PDF-1.7
1 0 obj
<< /Type /Catalog /Pages 2 0 R >>
endobj
2 0 obj
<< /Type /Pages /Kids [17 0 R 3 0 R] /Count 2 >>
endobj
3 0 obj
<< /Type /Page /Parent 2 0 R >>
endobj
17 0 obj
<< /Type /Page /Parent 2 0 R >>
endobj
"#;

        let doc = PdfDocument::from_slice(pdf, ParseOptions::default()).unwrap();
        let pages = doc.pages();
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].object_number, 17);
        assert_eq!(pages[1].object_number, 3);
    }

    #[test]
    fn resolves_inherited_font_resources_and_tounicode() {
        let cmap = b"/CMapName /Test\n1 begincodespacerange\n<00> <FF>\nendcodespacerange\n2 beginbfchar\n<01> <0048>\n<02> <0069>\nendbfchar\nendcmap\n";
        let content = b"BT\n/F1 12 Tf\n72 720 Td\n<0102> Tj\nET\n";
        let pdf = format!(
            "%PDF-1.7\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 /Resources 6 0 R >>\nendobj\n3 0 obj\n<< /Type /Page /Parent 2 0 R /Contents 4 0 R >>\nendobj\n4 0 obj\n<< /Length {} >>\nstream\n{}\nendstream\nendobj\n5 0 obj\n<< /Length {} >>\nstream\n{}\nendstream\nendobj\n6 0 obj\n<< /Font << /F1 7 0 R >> >>\nendobj\n7 0 obj\n<< /Type /Font /Subtype /Type0 /BaseFont /Dummy /ToUnicode 5 0 R >>\nendobj\n",
            content.len(),
            String::from_utf8_lossy(content),
            cmap.len(),
            String::from_utf8_lossy(cmap),
        );

        let doc = PdfDocument::from_slice(pdf.as_bytes(), ParseOptions::default()).unwrap();
        let pages = doc.pages();
        assert_eq!(pages.len(), 1);
        let runs = doc.semantic_text_runs_for_page(&pages[0]);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].text, "Hi");
        assert_eq!(runs[0].font.as_deref(), Some("F1"));
        assert_eq!(runs[0].font_object, Some(7));
    }

    #[test]
    fn simd_marker_scan_finds_obj_keywords() {
        let pdf = br#"%PDF-1.7
1 0 obj
<< /Type /Catalog >>
endobj
2 0 obj
<< /Length 4 >>
stream
test
endstream
endobj
"#;

        let markers = find_obj_markers_simd(pdf);
        assert!(markers.len() >= 2);
        assert!(markers
            .iter()
            .any(|offset| &pdf[*offset..*offset + 3] == b"obj"));
    }

    #[cfg(feature = "arrow")]
    #[test]
    fn parallel_arrow_exhaust_matches_sequential() {
        let pdf = br#"%PDF-1.4
1 0 obj
<< /Type /Catalog /Pages 2 0 R >>
endobj
2 0 obj
<< /Type /Pages /Kids [3 0 R] /Count 1 >>
endobj
3 0 obj
<< /Type /Page /Parent 2 0 R /Contents 4 0 R >>
endobj
4 0 obj
<< /Length 33 >>
stream
BT
/F1 12 Tf
(Hello SIMD) Tj
ET
endstream
endobj
"#;

        let doc = PdfDocument::from_slice(pdf, ParseOptions::default()).unwrap();
        let sequential = doc.semantic_text_runs_record_batch().unwrap();
        let parallel = doc.semantic_text_runs_record_batch_parallel().unwrap();
        assert_eq!(sequential.num_rows(), parallel.num_rows());
        assert_eq!(sequential.num_columns(), parallel.num_columns());
    }

    #[test]
    fn resyncs_stream_without_endstream_marker() {
        let pdf = br#"%PDF-1.4
1 0 obj
<< /Length 18 >>
stream
BT
(Ok) Tj
ET
endobj
2 0 obj
<< /Type /Catalog >>
endobj
"#;

        let doc = PdfDocument::from_slice(pdf, ParseOptions::default()).unwrap();
        assert_eq!(doc.objects.len(), 2);
        assert!(doc
            .warnings
            .iter()
            .any(|warning| warning.message.contains("sentinel resync")));
    }

    #[test]
    fn parses_empty_stream_without_length() {
        let pdf = br#"%PDF-1.4
1 0 obj
<< >>
stream
endstream
endobj
2 0 obj
<< /Type /Catalog >>
endobj
"#;

        let doc = PdfDocument::from_slice(pdf, ParseOptions::default()).unwrap();
        assert_eq!(doc.streams.len(), 1);
        assert_eq!(doc.streams[0].raw_len, 0);
    }

    #[cfg(feature = "arrow")]
    #[test]
    fn builds_page_multimodal_batch() {
        let jpeg = vec![0xFF, 0xD8, 0xFF, 0xD9];
        let mut pdf = br#"%PDF-1.7
1 0 obj
<< /Type /Catalog /Pages 2 0 R >>
endobj
2 0 obj
<< /Type /Pages /Kids [3 0 R] /Count 1 >>
endobj
3 0 obj
<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 400] /Contents 4 0 R /Resources 5 0 R >>
endobj
4 0 obj
<< /Length 37 >>
stream
BT
/F1 12 Tf
10 20 Td
(Hello) Tj
ET
endstream
endobj
5 0 obj
<< /XObject << /Im1 6 0 R >> >>
endobj
6 0 obj
<< /Subtype /Image /Filter /DCTDecode /Width 1 /Height 1 /Length "#
            .to_vec();
        pdf.extend_from_slice(jpeg.len().to_string().as_bytes());
        pdf.extend_from_slice(
            br#" >>
stream
"#,
        );
        pdf.extend_from_slice(&jpeg);
        pdf.extend_from_slice(
            br#"
endstream
endobj
"#,
        );

        let doc = PdfDocument::from_bytes(pdf, ParseOptions::default()).unwrap();
        let batch = doc.page_multimodal_record_batch_parallel().unwrap();
        assert_eq!(batch.num_rows(), 1);
        assert_eq!(batch.num_columns(), 6);
    }

    #[cfg(all(feature = "arrow", feature = "rasterize"))]
    #[test]
    fn rasterizes_vector_only_page_to_png_fallback() {
        let pdf = br#"%PDF-1.7
1 0 obj
<< /Type /Catalog /Pages 2 0 R >>
endobj
2 0 obj
<< /Type /Pages /Kids [3 0 R] /Count 1 >>
endobj
3 0 obj
<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R >>
endobj
4 0 obj
<< /Length 31 >>
stream
10 10 80 80 re
f
endstream
endobj
"#;

        let doc = PdfDocument::from_slice(pdf, ParseOptions::default()).unwrap();
        let batch = doc.page_multimodal_record_batch(0).unwrap();
        let image_format = batch
            .column_by_name("image_format")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
            .unwrap();
        let image_bytes = batch
            .column_by_name("image_bytes")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow::array::BinaryArray>()
            .unwrap();

        assert_eq!(image_format.value(0), "png");
        assert!(image_bytes.value_length(0) > 0);
        assert_eq!(&image_bytes.value(0)[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[cfg(all(feature = "arrow", feature = "rasterize"))]
    #[test]
    fn rasterizer_applies_colors_and_clipping() {
        let pdf = br#"%PDF-1.7
1 0 obj
<< /Type /Catalog /Pages 2 0 R >>
endobj
2 0 obj
<< /Type /Pages /Kids [3 0 R] /Count 1 >>
endobj
3 0 obj
<< /Type /Page /Parent 2 0 R /MediaBox [0 0 72 72] /Contents 4 0 R >>
endobj
4 0 obj
<< /Length 75 >>
stream
0 g
0 0 72 72 re
f
18 18 36 36 re
W
n
1 0 0 rg
0 0 72 72 re
f
endstream
endobj
"#;

        let doc = PdfDocument::from_slice(pdf, ParseOptions::default()).unwrap();
        let batch = doc.page_multimodal_record_batch(0).unwrap();
        let image_bytes = batch
            .column_by_name("image_bytes")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow::array::BinaryArray>()
            .unwrap();
        let png = Pixmap::decode_png(image_bytes.value(0)).unwrap();
        let center = png.pixel(150, 150).unwrap();
        let corner = png.pixel(10, 10).unwrap();

        assert!(center.red() > 200);
        assert!(center.green() < 40);
        assert!(center.blue() < 40);
        assert!(corner.red() < 40);
        assert!(corner.green() < 40);
        assert!(corner.blue() < 40);
    }

    #[test]
    fn table_grid_routes_points_top_down() {
        let segments = vec![
            LineSegment {
                x0: 0.0,
                y0: 0.0,
                x1: 100.0,
                y1: 0.0,
            },
            LineSegment {
                x0: 0.0,
                y0: 50.0,
                x1: 100.0,
                y1: 50.0,
            },
            LineSegment {
                x0: 0.0,
                y0: 100.0,
                x1: 100.0,
                y1: 100.0,
            },
            LineSegment {
                x0: 0.0,
                y0: 0.0,
                x1: 0.0,
                y1: 100.0,
            },
            LineSegment {
                x0: 50.0,
                y0: 0.0,
                x1: 50.0,
                y1: 100.0,
            },
            LineSegment {
                x0: 100.0,
                y0: 0.0,
                x1: 100.0,
                y1: 100.0,
            },
        ];

        let grid = TableGrid::build(&segments, 1.0);
        assert_eq!(grid.canonical_x, vec![0.0, 50.0, 100.0]);
        assert_eq!(grid.canonical_y, vec![0.0, 50.0, 100.0]);
        assert_eq!(grid.route_point(25.0, 75.0), Some((0, 0)));
        assert_eq!(grid.route_point(75.0, 75.0), Some((0, 1)));
        assert_eq!(grid.route_point(25.0, 25.0), Some((1, 0)));
        assert_eq!(grid.route_point(75.0, 25.0), Some((1, 1)));
    }

    #[test]
    fn routes_pdf_text_runs_into_table_cells() {
        let content = "0 0 100 100 re S\n0 50 m 100 50 l S\n50 0 m 50 100 l S\nBT\n/F1 12 Tf\n10 80 Td\n(A1) Tj\nET\nBT\n/F1 12 Tf\n60 80 Td\n(B1) Tj\nET\nBT\n/F1 12 Tf\n10 30 Td\n(A2) Tj\nET\n";
        let pdf = format!(
            "%PDF-1.7\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R >>\nendobj\n4 0 obj\n<< /Length {} >>\nstream\n{}endstream\nendobj\n",
            content.len(),
            content
        );

        let doc = PdfDocument::from_slice(pdf.as_bytes(), ParseOptions::default()).unwrap();
        let grid = doc.page_table_grid(0, 2.0).unwrap().unwrap();
        assert_eq!(grid.canonical_x, vec![0.0, 50.0, 100.0]);
        assert_eq!(grid.canonical_y, vec![0.0, 50.0, 100.0]);

        let assignments = doc.page_table_text_assignments(0, 2.0).unwrap();
        assert!(assignments
            .iter()
            .any(|item| item.text == "A1" && item.row == 0 && item.col == 0));
        assert!(assignments
            .iter()
            .any(|item| item.text == "B1" && item.row == 0 && item.col == 1));
        assert!(assignments
            .iter()
            .any(|item| item.text == "A2" && item.row == 1 && item.col == 0));
    }

    #[test]
    fn merges_table_spans_into_layout_cells() {
        let content = "0 0 100 100 re S\n0 50 m 100 50 l S\n50 0 m 50 50 l S\nBT\n/F1 12 Tf\n10 80 Td\n(HEADER) Tj\nET\nBT\n/F1 12 Tf\n10 30 Td\n(A2) Tj\nET\nBT\n/F1 12 Tf\n60 30 Td\n(B2) Tj\nET\n";
        let pdf = format!(
            "%PDF-1.7\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R >>\nendobj\n4 0 obj\n<< /Length {} >>\nstream\n{}endstream\nendobj\n",
            content.len(),
            content
        );

        let doc = PdfDocument::from_slice(pdf.as_bytes(), ParseOptions::default()).unwrap();
        let layout = doc.page_table_layout(0, 2.0).unwrap().unwrap();

        assert!(layout.cells.iter().any(|cell| cell.row_start == 0
            && cell.row_end == 0
            && cell.col_start == 0
            && cell.col_end == 1
            && cell.text.contains("HEADER")));
        assert!(layout.cells.iter().any(|cell| cell.row_start == 1
            && cell.row_end == 1
            && cell.col_start == 0
            && cell.col_end == 0
            && cell.text.contains("A2")));
        assert!(layout.cells.iter().any(|cell| cell.row_start == 1
            && cell.row_end == 1
            && cell.col_start == 1
            && cell.col_end == 1
            && cell.text.contains("B2")));
    }

    #[cfg(feature = "arrow")]
    #[test]
    fn materializes_table_layout_record_batch() {
        let content = "0 0 100 100 re S\n0 50 m 100 50 l S\n50 0 m 50 100 l S\nBT\n/F1 12 Tf\n10 80 Td\n(A1) Tj\nET\nBT\n/F1 12 Tf\n60 80 Td\n(B1) Tj\nET\n";
        let pdf = format!(
            "%PDF-1.7\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R >>\nendobj\n4 0 obj\n<< /Length {} >>\nstream\n{}endstream\nendobj\n",
            content.len(),
            content
        );

        let doc = PdfDocument::from_slice(pdf.as_bytes(), ParseOptions::default()).unwrap();
        let batch = doc.page_table_layout_record_batch(0, 2.0).unwrap();
        assert_eq!(batch.num_columns(), 10);
        assert_eq!(batch.num_rows(), 4);

        let table_index = batch
            .column_by_name("table_index")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow::array::UInt32Array>()
            .unwrap();
        let row_span = batch
            .column_by_name("row_span")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow::array::UInt32Array>()
            .unwrap();
        let text = batch
            .column_by_name("text")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
            .unwrap();
        assert_eq!(table_index.value(0), 0);
        assert_eq!(row_span.value(0), 1);
        assert!(text.iter().flatten().any(|value| value == "A1"));
        assert!(text.iter().flatten().any(|value| value == "B1"));
    }

    #[cfg(feature = "arrow")]
    #[test]
    fn materializes_grouped_table_record_batch() {
        let content = "0 0 100 100 re S\n0 50 m 100 50 l S\n50 0 m 50 50 l S\nBT\n/F1 12 Tf\n10 80 Td\n(HEADER) Tj\nET\nBT\n/F1 12 Tf\n10 30 Td\n(A2) Tj\nET\nBT\n/F1 12 Tf\n60 30 Td\n(B2) Tj\nET\n";
        let pdf = format!(
            "%PDF-1.7\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R >>\nendobj\n4 0 obj\n<< /Length {} >>\nstream\n{}endstream\nendobj\n",
            content.len(),
            content
        );

        let doc = PdfDocument::from_slice(pdf.as_bytes(), ParseOptions::default()).unwrap();
        let tables = doc.page_tables(0, 2.0).unwrap();
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].header_row_count, 1);
        assert_eq!(tables[0].col_count, 2);
        assert!(tables[0].markdown.contains("HEADER"));

        let batch = doc.page_table_record_batch(0, 2.0).unwrap();
        assert_eq!(batch.num_rows(), 1);
        assert_eq!(batch.num_columns(), 13);
        let header_row_count = batch
            .column_by_name("header_row_count")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow::array::UInt32Array>()
            .unwrap();
        let markdown = batch
            .column_by_name("markdown")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
            .unwrap();
        assert_eq!(header_row_count.value(0), 1);
        assert!(batch.column_by_name("cells_json").is_none());
        assert!(markdown.value(0).contains("| HEADER |"));
    }

    #[test]
    fn flattens_multi_row_headers_into_markdown() {
        let content = "0 0 100 150 re S\n0 100 m 100 100 l S\n0 50 m 100 50 l S\n50 0 m 50 100 l S\nBT\n/F1 12 Tf\n10 130 Td\n(GROUP) Tj\nET\nBT\n/F1 12 Tf\n10 80 Td\n(A) Tj\nET\nBT\n/F1 12 Tf\n60 80 Td\n(B) Tj\nET\nBT\n/F1 12 Tf\n10 30 Td\n(V1) Tj\nET\nBT\n/F1 12 Tf\n60 30 Td\n(V2) Tj\nET\n";
        let pdf = format!(
            "%PDF-1.7\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 150] /Contents 4 0 R >>\nendobj\n4 0 obj\n<< /Length {} >>\nstream\n{}endstream\nendobj\n",
            content.len(),
            content
        );

        let doc = PdfDocument::from_slice(pdf.as_bytes(), ParseOptions::default()).unwrap();
        let tables = doc.page_tables(0, 2.0).unwrap();
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].header_row_count, 2);
        assert!(tables[0].markdown.contains("GROUP / A"));
        assert!(tables[0].markdown.contains("GROUP / B"));
        assert!(tables[0].markdown.contains("| V1 | V2 |"));
    }

    #[cfg(feature = "arrow")]
    #[test]
    fn routes_ocr_runs_back_into_table_cells() {
        let content = "0 0 100 100 re S\n0 50 m 100 50 l S\n50 0 m 50 100 l S\n";
        let pdf = format!(
            "%PDF-1.7\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R >>\nendobj\n4 0 obj\n<< /Length {} >>\nstream\n{}endstream\nendobj\n",
            content.len(),
            content
        );

        let doc = PdfDocument::from_slice(pdf.as_bytes(), ParseOptions::default()).unwrap();
        let scale = 300.0 / 72.0;
        let ocr_runs = vec![
            OcrTextRun {
                text: "A1".to_string(),
                px_x: 25.0 * scale,
                px_y: (100.0 - 75.0) * scale,
                px_height: 12.0 * scale,
            },
            OcrTextRun {
                text: "B2".to_string(),
                px_x: 75.0 * scale,
                px_y: (100.0 - 25.0) * scale,
                px_height: 12.0 * scale,
            },
        ];

        let layout = doc
            .page_table_layout_from_ocr(0, 2.0, 300.0, &ocr_runs)
            .unwrap()
            .unwrap();
        assert!(layout.cells.iter().any(|cell| {
            cell.row_start == 0 && cell.col_start == 0 && cell.text.contains("A1")
        }));
        assert!(layout.cells.iter().any(|cell| {
            cell.row_start == 1 && cell.col_start == 1 && cell.text.contains("B2")
        }));

        let batch = doc
            .page_table_layout_record_batch_from_ocr(0, 2.0, 300.0, &ocr_runs)
            .unwrap();
        let text = batch
            .column_by_name("text")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
            .unwrap();
        assert!(text.iter().flatten().any(|value| value == "A1"));
        assert!(text.iter().flatten().any(|value| value == "B2"));
    }

    #[cfg(feature = "arrow")]
    #[test]
    fn routes_ocr_record_batch_back_into_table_cells() {
        use arrow::array::{ArrayRef, Float32Array, StringArray};
        use arrow::datatypes::{DataType, Field, Schema};
        use std::sync::Arc;

        let content = "0 0 100 100 re S\n0 50 m 100 50 l S\n50 0 m 50 100 l S\n";
        let pdf = format!(
            "%PDF-1.7\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R >>\nendobj\n4 0 obj\n<< /Length {} >>\nstream\n{}endstream\nendobj\n",
            content.len(),
            content
        );

        let doc = PdfDocument::from_slice(pdf.as_bytes(), ParseOptions::default()).unwrap();
        let scale = 300.0 / 72.0;
        let schema = Arc::new(Schema::new(vec![
            Field::new("text", DataType::Utf8, false),
            Field::new("px_x", DataType::Float32, false),
            Field::new("px_y", DataType::Float32, false),
            Field::new("px_height", DataType::Float32, false),
        ]));
        let text: ArrayRef = Arc::new(StringArray::from(vec!["A1", "B2"]));
        let px_x: ArrayRef = Arc::new(Float32Array::from(vec![25.0 * scale, 75.0 * scale]));
        let px_y: ArrayRef = Arc::new(Float32Array::from(vec![
            (100.0 - 75.0) * scale,
            (100.0 - 25.0) * scale,
        ]));
        let px_height: ArrayRef = Arc::new(Float32Array::from(vec![12.0 * scale, 12.0 * scale]));
        let ocr_batch = RecordBatch::try_new(schema, vec![text, px_x, px_y, px_height]).unwrap();

        let layout = doc
            .page_table_layout_from_ocr_batch(0, 2.0, 300.0, &ocr_batch)
            .unwrap()
            .unwrap();
        assert!(layout.cells.iter().any(|cell| {
            cell.row_start == 0 && cell.col_start == 0 && cell.text.contains("A1")
        }));
        assert!(layout.cells.iter().any(|cell| {
            cell.row_start == 1 && cell.col_start == 1 && cell.text.contains("B2")
        }));

        let out = doc
            .page_table_layout_record_batch_from_ocr_batch(0, 2.0, 300.0, &ocr_batch)
            .unwrap();
        let out_text = out
            .column_by_name("text")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
            .unwrap();
        assert!(out_text.iter().flatten().any(|value| value == "A1"));
        assert!(out_text.iter().flatten().any(|value| value == "B2"));
    }

    #[cfg(feature = "arrow")]
    #[test]
    fn ignores_filtered_unknown_stream_raw_bytes_in_semantic_pass() {
        let raw = b"BT\n/F1 12 Tf\n(Should Not Parse) Tj\nET\n";
        let pdf = format!(
            "%PDF-1.7\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n3 0 obj\n<< /Type /Page /Parent 2 0 R /Contents 4 0 R >>\nendobj\n4 0 obj\n<< /Length {} /Filter /FlateDecode >>\nstream\n{}\nendstream\nendobj\n",
            raw.len(),
            String::from_utf8_lossy(raw),
        );

        let doc = PdfDocument::from_slice(pdf.as_bytes(), ParseOptions::default()).unwrap();
        assert_eq!(doc.streams.len(), 1);
        assert_eq!(doc.streams[0].kind, PdfStreamKind::Unknown);
        assert!(matches!(
            doc.streams[0].decode_status,
            DecodeStatus::Failed(_)
        ));

        let pages = doc.pages();
        assert_eq!(pages.len(), 1);
        assert!(doc.semantic_text_runs_for_page(&pages[0]).is_empty());

        let batch = doc.page_multimodal_record_batch(0).unwrap();
        assert_eq!(batch.num_rows(), 1);
    }

    #[test]
    fn page_box_prefers_inherited_crop_box_over_media_box() {
        let pdf = br#"%PDF-1.7
1 0 obj
<< /Type /Catalog /Pages 2 0 R >>
endobj
2 0 obj
<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 200 400] /CropBox [10 20 150 300] >>
endobj
3 0 obj
<< /Type /Page /Parent 2 0 R >>
endobj
"#;

        let doc = PdfDocument::from_slice(pdf, ParseOptions::default()).unwrap();
        let pages = doc.pages();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].media_box, Some([0.0, 0.0, 200.0, 400.0]));
        assert_eq!(pages[0].crop_box, Some([10.0, 20.0, 150.0, 300.0]));
        assert_eq!(pages[0].page_box(), Some([10.0, 20.0, 150.0, 300.0]));
    }

    // ── CMap fallback tracking ────────────────────────────────────────────

    /// A font with a proper ToUnicode CMap should produce zero fallback chars.
    #[test]
    fn cmap_fully_mapped_produces_zero_fallback_chars() {
        // Minimal PDF with a ToUnicode CMap that maps 0x48 → 'H', 0x69 → 'i'.
        let pdf = build_pdf_with_tounicode_cmap(b"\x48\x69", "Hi");
        let doc = PdfDocument::from_slice(&pdf, ParseOptions::default()).unwrap();
        let runs = doc.semantic_text_runs();
        let relevant: Vec<_> = runs.iter().filter(|r| r.text.contains("Hi")).collect();
        assert!(
            !relevant.is_empty(),
            "Expected at least one run containing 'Hi'"
        );
        for run in &relevant {
            assert_eq!(
                run.cmap_fallback_chars, 0,
                "Run {:?} should have zero fallback chars when CMap is complete",
                run.text
            );
        }
    }

    /// A byte that is NOT in the ToUnicode CMap must be counted as a fallback.
    #[test]
    fn cmap_unmapped_byte_counted_as_fallback() {
        // CMap maps only 0x48 → 'H'; byte 0x69 is unmapped (raw fallback = 'i').
        let pdf = build_pdf_with_partial_cmap(b"\x48\x69");
        let doc = PdfDocument::from_slice(&pdf, ParseOptions::default()).unwrap();
        let runs = doc.semantic_text_runs();
        // At least one run must report exactly 1 fallback char (the 0x69).
        let fallback_total: u32 = runs.iter().map(|r| r.cmap_fallback_chars).sum();
        assert_eq!(
            fallback_total, 1,
            "Exactly the unmapped byte 0x69 should be counted as fallback; got {}",
            fallback_total
        );
    }

    /// A font with no ToUnicode CMap at all should count ALL chars as fallback.
    #[test]
    fn no_cmap_all_chars_counted_as_fallback() {
        // No ToUnicode stream; the font is a simple Type1 (no UTF-16 fallback).
        let pdf = build_pdf_without_cmap(b"\x48\x69");
        let doc = PdfDocument::from_slice(&pdf, ParseOptions::default()).unwrap();
        let runs = doc.semantic_text_runs();
        let non_empty: Vec<_> = runs.iter().filter(|r| !r.text.is_empty()).collect();
        assert!(!non_empty.is_empty());
        let fallback_total: u32 = non_empty.iter().map(|r| r.cmap_fallback_chars).sum();
        let char_total: u32 = non_empty
            .iter()
            .map(|r| r.text.chars().count() as u32)
            .sum();
        assert_eq!(
            fallback_total, char_total,
            "All chars should be fallback when no CMap exists"
        );
    }

    /// A 1-byte Type1 ToUnicode CMap that declares a 2-byte `<0000> <FFFF>`
    /// codespace range while mapping 1-byte ligature codes (`<1C>` -> `ffi`,
    /// `<1D>` -> `fl`, `<1E>` -> `ff`, `<1F>` -> `fi`, `<1B>` -> thin space)
    /// and a 2-byte `<0020>` space entry must decode cleanly on both
    /// `semantic_text_runs` and `text_fragments` with zero fallback chars.
    #[test]
    fn cmap_decodes_one_byte_ligatures_under_two_byte_codespace_and_in_text_fragments() {
        let cmap = "/CIDInit /ProcSet findresource begin\n\
            12 dict begin\n\
            begincmap\n\
            /CMapName /Adobe-Identity-UCS def\n\
            /CMapType 2 def\n\
            1 begincodespacerange\n\
            <0000> <FFFF>\n\
            endcodespacerange\n\
            8 beginbfchar\n\
            <1B> <2009>\n\
            <1C> <006600660069>\n\
            <1D> <0066006C>\n\
            <1E> <00660066>\n\
            <1F> <00660069>\n\
            <0020> <0020>\n\
            <38> <0038>\n\
            <B0> <00B0>\n\
            endbfchar\n\
            endcmap\n";
        let content = "BT\n/F1 12 Tf\n<1C201D201E201F20381BB0> Tj\nET";
        let pdf = build_pdf_with_font_and_stream(Some(cmap), content, "Type1");
        let doc = PdfDocument::from_slice(&pdf, ParseOptions::default()).unwrap();
        let runs = doc.semantic_text_runs();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].text, "ffi fl ff fi 8\u{2009}°");
        assert_eq!(runs[0].cmap_fallback_chars, 0);
        assert_eq!(doc.text_fragments.len(), 1);
        assert_eq!(doc.text_fragments[0].text, "ffi fl ff fi 8\u{2009}°");
    }

    /// A 2-byte Type0 CID font with a ToUnicode CMap mapping `<0210>` -> `.`
    /// must decode in `doc.text_fragments` as well as `doc.semantic_text_runs()`.
    #[test]
    fn text_fragments_decode_cid_font_via_page_font_bindings() {
        let cmap = "/CIDInit /ProcSet findresource begin\n\
            12 dict begin\n\
            begincmap\n\
            /CMapName /Adobe-Identity-UCS def\n\
            /CMapType 2 def\n\
            1 begincodespacerange\n\
            <0000> <FFFF>\n\
            endcodespacerange\n\
            1 beginbfchar\n\
            <0210> <002E>\n\
            endbfchar\n\
            endcmap\n";
        let content = "BT\n/F1 10 Tf\n<0210> Tj\nET";
        let pdf = build_pdf_with_font_and_stream(Some(cmap), content, "Type0");
        let doc = PdfDocument::from_slice(&pdf, ParseOptions::default()).unwrap();
        assert_eq!(doc.text_fragments.len(), 1);
        assert_eq!(doc.text_fragments[0].text, ".");
        let runs = doc.semantic_text_runs();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].text, ".");
        assert_eq!(runs[0].cmap_fallback_chars, 0);
    }

    // ── Test PDF builders ─────────────────────────────────────────────────

    /// Build a minimal PDF with a complete ToUnicode CMap mapping the given
    /// `raw_bytes` to the `expected_text` string.
    fn build_pdf_with_tounicode_cmap(raw_bytes: &[u8], expected_text: &str) -> Vec<u8> {
        // Build bfchar entries: one entry per byte.
        let chars: Vec<(u8, char)> = raw_bytes
            .iter()
            .zip(expected_text.chars())
            .map(|(&b, c)| (b, c))
            .collect();
        let bfchar_entries: String = chars
            .iter()
            .map(|(b, c)| format!("<{:02X}> <{:04X}>\n", b, *c as u32))
            .collect();
        let bfchar_count = chars.len();

        let hex_string: String = raw_bytes.iter().map(|b| format!("{:02X}", b)).collect();

        let cmap = format!(
            "/CIDInit /ProcSet findresource begin\n\
             12 dict begin\n\
             begincmap\n\
             /CIDSystemInfo 3 dict dup begin\n\
             /Registry (Adobe) def\n\
             /Ordering (UCS) def\n\
             /Supplement 0 def\n\
             end def\n\
             /CMapName /Adobe-Identity-UCS def\n\
             /CMapType 2 def\n\
             1 begincodespacerange\n\
             <00> <FF>\n\
             endcodespacerange\n\
             {bfchar_count} beginbfchar\n\
             {bfchar_entries}\
             endbfchar\n\
             endcmap\n\
             CMapName currentdict /CMap defineresource pop\n\
             end\n\
             end\n"
        );

        build_pdf_with_font_and_stream(
            Some(&cmap),
            &format!("BT\n/F1 12 Tf\n<{hex_string}> Tj\nET"),
            "Type1",
        )
    }

    /// Build a PDF with a partial CMap: only the first byte is mapped.
    fn build_pdf_with_partial_cmap(raw_bytes: &[u8]) -> Vec<u8> {
        assert!(raw_bytes.len() >= 2);
        let b0 = raw_bytes[0];
        let cmap = format!(
            "/CIDInit /ProcSet findresource begin\n\
             12 dict begin\n\
             begincmap\n\
             /CIDSystemInfo 3 dict dup begin\n\
             /Registry (Adobe) def\n\
             /Ordering (UCS) def\n\
             /Supplement 0 def\n\
             end def\n\
             /CMapName /Adobe-Identity-UCS def\n\
             /CMapType 2 def\n\
             1 begincodespacerange\n\
             <00> <FF>\n\
             endcodespacerange\n\
             1 beginbfchar\n\
             <{:02X}> <0048>\n\
             endbfchar\n\
             endcmap\n\
             CMapName currentdict /CMap defineresource pop\n\
             end\n\
             end\n",
            b0
        );
        let hex_string: String = raw_bytes.iter().map(|b| format!("{:02X}", b)).collect();
        build_pdf_with_font_and_stream(
            Some(&cmap),
            &format!("BT\n/F1 12 Tf\n<{hex_string}> Tj\nET"),
            "Type1",
        )
    }

    /// Build a PDF with no ToUnicode CMap at all.
    fn build_pdf_without_cmap(raw_bytes: &[u8]) -> Vec<u8> {
        let hex_string: String = raw_bytes.iter().map(|b| format!("{:02X}", b)).collect();
        build_pdf_with_font_and_stream(
            None,
            &format!("BT\n/F1 12 Tf\n<{hex_string}> Tj\nET"),
            "Type1",
        )
    }

    /// Low-level PDF builder with optional ToUnicode stream.
    fn build_pdf_with_font_and_stream(
        cmap: Option<&str>,
        content: &str,
        font_subtype: &str,
    ) -> Vec<u8> {
        if let Some(cmap_str) = cmap {
            format!(
                "%PDF-1.4\n\
                 1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
                 2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n\
                 3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792]\n\
                    /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>\nendobj\n\
                 4 0 obj\n<< /Length {} >>\nstream\n{}\nendstream\nendobj\n\
                 5 0 obj\n<< /Type /Font /Subtype /{} /BaseFont /TestFont\n\
                    /ToUnicode 6 0 R >>\nendobj\n\
                 6 0 obj\n<< /Length {} >>\nstream\n{}\nendstream\nendobj\n",
                content.len(),
                content,
                font_subtype,
                cmap_str.len(),
                cmap_str,
            )
            .into_bytes()
        } else {
            format!(
                "%PDF-1.4\n\
                 1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
                 2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n\
                 3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792]\n\
                    /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>\nendobj\n\
                 4 0 obj\n<< /Length {} >>\nstream\n{}\nendstream\nendobj\n\
                 5 0 obj\n<< /Type /Font /Subtype /{} /BaseFont /TestFont >>\nendobj\n",
                content.len(),
                content,
                font_subtype,
            )
            .into_bytes()
        }
    }
}
