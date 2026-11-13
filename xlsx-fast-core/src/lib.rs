#![deny(missing_docs)]
#![warn(rust_2018_idioms)]

//! # xlsx-fast-core
//!
//! High-performance Excel (.xlsx) parser with SIMD optimizations.
//!
//! ## Features
//!
//! - **Zero-copy parsing** via memory-mapped files
//! - **SIMD-optimized** cell reference parsing (NEON/AVX2/SSE2)
//! - **Streaming decompression** with libdeflater for large files
//! - **Date decoding** supporting both 1900 and 1904 date systems
//! - **Formula extraction** with optional prefix handling
//! - **Range filtering** for selective cell extraction
//! - **Arrow RecordBatch** streaming callbacks (optional feature)
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use xlsx_fast_core::{parse_xlsx, ParseOptions};
//!
//! let workbook = parse_xlsx("spreadsheet.xlsx", ParseOptions::default())?;
//! for sheet in &workbook.sheets {
//!     println!("Sheet: {} ({} cells)", sheet.name, sheet.cells.len());
//! }
//! # Ok::<(), anyhow::Error>(())
//! ```
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │  Memory-mapped ZIP Archive (rawzip + memmap2)               │
//! └─────────────────────────────────────────────────────────────┘
//!                               ↓
//! ┌─────────────────────────────────────────────────────────────┐
//! │  Streaming Decompression (libdeflater / flate2 fallback)    │
//! └─────────────────────────────────────────────────────────────┘
//!                               ↓
//! ┌─────────────────────────────────────────────────────────────┐
//! │  XML Parsing (quick-xml) with SIMD cell ref parsing         │
//! └─────────────────────────────────────────────────────────────┘
//!                               ↓
//! ┌─────────────────────────────────────────────────────────────┐
//! │  Output: Workbook struct | Arrow RecordBatch callbacks      │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Files on disk
//!
//! Functions that take a path memory-map the file. It must not be modified
//! or truncated, by this process or any other, while it is being read: a
//! truncated file can make reads of the mapping fault (`SIGBUS` on Unix),
//! and a modified one can change the bytes under the parser. Functions that
//! take bytes have no such requirement.

use anyhow::{anyhow, Context, Result};
use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
#[cfg(feature = "arrow")]
use ooxml_archive::SharedInflateBudget;
use ooxml_archive::{
    parse_relationships, part_dir, rels_path_for_part, resolve_part, utf8::LossyUtf8, OoxmlArchive,
    PartReader,
};
use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::attributes::Attribute;
use quick_xml::events::{BytesRef, BytesStart, Event};
use quick_xml::{Reader, XmlVersion};
use serde::Serialize;
use std::borrow::Cow;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, Cursor};
use std::path::Path;
use std::sync::Arc;

fn local_name(name: &[u8]) -> &[u8] {
    if let Some(pos) = name.iter().position(|&b| b == b':') {
        &name[pos + 1..]
    } else {
        name
    }
}

/// Appends the character data `event` carries to `out`.
///
/// The reader reports the text of an element in pieces: literal text, CDATA
/// sections, and one event per entity or character reference. Every caller
/// that collects text passes all three kinds here, so a reference lands in
/// the same place as the text around it. Any other event appends nothing.
///
/// Decoding is lenient, so one bad character costs that character rather than
/// the whole workbook: a stray non-UTF-8 byte becomes U+FFFD, and CDATA is
/// literal character data that is never entity-resolved.
///
/// Nothing is allocated here: the hot path of a sheet is the text of its
/// `<v>` elements, which is appended straight from the reader's buffer.
fn push_character_data(event: &Event<'_>, out: &mut String) {
    match event {
        Event::Text(text) => out.push_str(text),
        Event::CData(data) => out.push_str(data),
        Event::GeneralRef(reference) => push_reference(reference, out),
        _ => {}
    }
}

/// Appends what the reference `&name;` stands for to `out`.
///
/// The five predefined entities and decimal or hexadecimal character
/// references resolve to their character. Anything else (an entity this
/// parser does not know, or a character reference that names no character,
/// such as `&#0;`) is kept exactly as written, never dropped.
fn push_reference(reference: &BytesRef<'_>, out: &mut String) {
    let name: &str = reference;
    match reference.resolve_char_ref() {
        Ok(Some(character)) => {
            out.push(character);
            return;
        }
        Ok(None) => {
            if let Some(text) = resolve_predefined_entity(name) {
                out.push_str(text);
                return;
            }
        }
        Err(_) => {}
    }
    out.push('&');
    out.push_str(name);
    out.push(';');
}

/// Makes `reader` as tolerant of character data as this parser needs.
///
/// Whitespace is significant in cell text, so nothing is trimmed (trimming
/// would also delete the spaces around a reference, which arrives as its own
/// event). A lone `&` that starts no reference is ordinary text in the files
/// people actually have, not a reason to reject the workbook.
fn configure_reader<R>(reader: &mut Reader<R>) {
    let config = reader.config_mut();
    config.trim_text(false);
    config.allow_dangling_amp = true;
}

/// Decodes an attribute value and resolves the references in it.
///
/// The value is normalized as XML 1.0 requires: a literal tab, line feed or
/// carriage return becomes a space (and a CR LF pair one space), while the
/// same characters written as character references (`&#10;`) are kept.
///
/// `tag` is the start or empty tag the attribute was read from, through
/// `input`. A value in which `input` replaced bytes that are not UTF-8 with
/// U+FFFD (see [`LossyUtf8`]) is an error, as a value that is not UTF-8
/// always was. A U+FFFD the part really holds, or a replacement elsewhere in
/// the part, is not.
///
/// # Errors
///
/// Fails when the value is not UTF-8, or holds a reference that cannot be
/// resolved.
fn attribute_value<'a, R: BufRead>(
    attribute: &Attribute<'a>,
    input: &LossyUtf8<R>,
    tag: &str,
) -> Result<Cow<'a, str>> {
    if input.repaired_in(tag.as_bytes(), 1..=2, attribute.value.as_bytes()) {
        return Err(anyhow!("attribute value is not UTF-8"));
    }
    Ok(attribute.normalized_value(XmlVersion::Implicit1_0)?)
}

/// Reads a part as UTF-8, each ill-formed sequence replaced by U+FFFD.
///
/// `limit` is the size the part's raw bytes were held to; the repaired part
/// may not outgrow it, since repair can triple a part's size.
fn repairing<R: BufRead>(input: R, limit: usize) -> LossyUtf8<R> {
    LossyUtf8::with_limit(input, limit as u64)
}

/// The repair limit for a part read whole, with no part limit in scope.
fn buffer_limit(xml: &[u8]) -> usize {
    ooxml_archive::DEFAULT_MAX_INFLATE.max(xml.len())
}

/// Byte length of one `ST_Xstring` escape, `_xHHHH_`.
const XSTRING_ESCAPE_LEN: usize = 7;

/// Returns the UTF-16 code unit of the `_xHHHH_` escape `bytes` starts with.
fn xstring_code_unit(bytes: &[u8]) -> Option<u16> {
    let escape = bytes.get(..XSTRING_ESCAPE_LEN)?;
    if escape[0] != b'_' || escape[1] != b'x' || escape[6] != b'_' {
        return None;
    }
    let mut unit = 0u16;
    for &digit in &escape[2..6] {
        unit = (unit << 4) | u16::try_from(char::from(digit).to_digit(16)?).ok()?;
    }
    Some(unit)
}

/// Decodes the `_xHHHH_` escapes of an ECMA-376 `ST_Xstring`.
///
/// Excel writes characters XML cannot carry, such as a carriage return, as
/// `_x000D_`: the UTF-16 code unit in hexadecimal. A literal underscore that
/// would otherwise start an escape is itself written `_x005F_`, so decoding
/// runs left to right and never rescans its own output. Two escapes that form
/// a surrogate pair decode to one character; an unpaired surrogate becomes
/// U+FFFD.
fn decode_xstring(text: &str) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    let mut decoded = String::new();
    let mut copied = 0;
    let mut cursor = 0;
    while let Some(found) = text[cursor..].find("_x") {
        let start = cursor + found;
        let Some(unit) = xstring_code_unit(&bytes[start..]) else {
            cursor = start + 2;
            continue;
        };
        let mut end = start + XSTRING_ESCAPE_LEN;
        let mut units = [unit, 0];
        let mut unit_count = 1;
        if (0xD800..0xDC00).contains(&unit) {
            if let Some(low) =
                xstring_code_unit(&bytes[end..]).filter(|low| (0xDC00..0xE000).contains(low))
            {
                units[1] = low;
                unit_count = 2;
                end += XSTRING_ESCAPE_LEN;
            }
        }
        decoded.push_str(&text[copied..start]);
        decoded.extend(
            char::decode_utf16(units[..unit_count].iter().copied())
                .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER)),
        );
        copied = end;
        cursor = end;
    }
    if copied == 0 {
        return Cow::Borrowed(text);
    }
    decoded.push_str(&text[copied..]);
    Cow::Owned(decoded)
}

/// Decodes the `ST_Xstring` escapes in `text[from..]` in place.
///
/// `from` is where the current `<t>` element's text starts, so an escape is
/// never assembled from two adjacent elements.
fn decode_xstring_tail(text: &mut String, from: usize) {
    let decoded = match decode_xstring(&text[from..]) {
        Cow::Owned(decoded) => Some(decoded),
        Cow::Borrowed(_) => None,
    };
    if let Some(decoded) = decoded {
        text.truncate(from);
        text.push_str(&decoded);
    }
}

/// Arrow RecordBatch streaming output (optional feature).
///
/// Exports spreadsheet data as Arrow Record Batch files, suitable for
/// columnar ETL pipelines and analytics systems. Used by `xlsx-fast-flight`
/// (flight server), `xlsx-fast-py` (Python bindings), and integration
/// tests in sibling parsers.
#[cfg(feature = "arrow")]
pub mod arrow;

/// Configuration options for XLSX parsing.
///
/// # Example
///
/// ```rust
/// use xlsx_fast_core::ParseOptions;
///
/// let options = ParseOptions {
///     decode_dates: true,      // Convert Excel dates to ISO8601
///     include_formulas: true,  // Extract formula text
///     raw_values: false,       // Parse numeric values
///     max_inflate: 64 * 1024 * 1024, // 64 MiB limit
/// };
/// ```
#[derive(Debug, Clone, Copy)]
pub struct ParseOptions {
    /// Convert date-formatted numeric cells to ISO8601 strings.
    ///
    /// When `true`, cells with date number formats are converted from
    /// Excel's serial date representation to ISO8601 strings.
    /// Supports both the 1900 (Windows) and 1904 (Mac) date systems.
    pub decode_dates: bool,

    /// Include formula text when present.
    ///
    /// When `true`, the `formula` field is populated with the formula
    /// string (e.g., `"SUM(A1:A10)"`). The `value` field contains the
    /// cached result.
    pub include_formulas: bool,

    /// Emit raw values for non-string cells (skip numeric/date parsing).
    ///
    /// When `true`, numeric cells are returned as raw strings without
    /// any date or number formatting applied.
    pub raw_values: bool,

    /// Max uncompressed part size to load into memory (bytes).
    ///
    /// Parts larger than this threshold are streamed rather than
    /// loaded entirely into memory. Default: 128 MiB.
    pub max_inflate: usize,
}

impl Default for ParseOptions {
    fn default() -> Self {
        Self {
            decode_dates: true,
            include_formulas: false,
            raw_values: false,
            max_inflate: 128 * 1024 * 1024, // 128 MiB
        }
    }
}

/// Filter to extract only cells within a specific range.
///
/// Row and column indices are 1-based to match Excel conventions.
///
/// # Example
///
/// ```rust
/// use xlsx_fast_core::RangeFilter;
///
/// // Extract cells A1:C10 (columns 1-3, rows 1-10)
/// let filter = RangeFilter {
///     row_start: 1,
///     row_end: 10,
///     col_start: 1,
///     col_end: 3,
/// };
/// ```
#[derive(Debug, Clone, Copy)]
pub struct RangeFilter {
    /// First row to include (1-based, inclusive).
    pub row_start: u32,
    /// Last row to include (1-based, inclusive).
    pub row_end: u32,
    /// First column to include (1-based, A=1, B=2, ...).
    pub col_start: u32,
    /// Last column to include (1-based, inclusive).
    pub col_end: u32,
}

impl RangeFilter {
    fn contains(&self, row: u32, col: u32) -> bool {
        row >= self.row_start && row <= self.row_end && col >= self.col_start && col <= self.col_end
    }
}

/// A complete Excel workbook containing multiple sheets.
///
/// This is the main output type returned by [`parse_xlsx`].
#[derive(Debug, Serialize)]
pub struct Workbook {
    /// All worksheets in the workbook, in order.
    pub sheets: Vec<Sheet>,
}

/// A single worksheet within an Excel workbook.
#[derive(Debug, Serialize)]
pub struct Sheet {
    /// Sheet index (1-based, matching Excel's internal ID).
    pub id: u32,
    /// Sheet name as displayed in Excel's tab bar.
    pub name: String,
    /// All cells with values in this sheet.
    pub cells: Vec<Cell>,
}

/// A single cell within a worksheet.
#[derive(Debug, Serialize)]
pub struct Cell {
    /// Row index (1-based).
    pub row: u32,
    /// Column index (1-based, A=1, B=2, ...).
    pub col: u32,
    /// Formula text if present and `include_formulas` is enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub formula: Option<String>,
    /// The cell's value (string, number, boolean, or error).
    pub value: CellValue,
}

/// The decoded value of a single Excel cell.
///
/// Variants map directly to the OOXML cell types (`t` attribute) plus a
/// post-processed `Date` variant emitted when `decode_dates` is enabled and the
/// cell carries a date-formatted style. `Raw` is used when `raw_values` is set,
/// preserving the original string form of a numeric cell without conversion.
#[derive(Debug, Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum CellValue {
    /// The cell is present in the sheet stream but has no value.
    Empty,
    /// Boolean cell (`t="b"`).
    Bool {
        /// Decoded boolean value.
        v: bool,
    },
    /// Numeric cell (`t="n"` or default). Holds the IEEE-754 double parsed from
    /// the cell's `<v>` element.
    Number {
        /// Decoded numeric value.
        v: f64,
    },
    /// String cell, either shared (`t="s"`) or inline (`t="inlineStr"`).
    /// Wrapped in `Arc<str>` so shared-string entries can be cheaply duplicated
    /// across many cells without reallocating.
    String {
        /// Decoded string value.
        v: Arc<str>,
    },
    /// Raw passthrough of a numeric cell's underlying text (used when
    /// `raw_values` is enabled to skip numeric/date parsing).
    Raw {
        /// Raw text from the `<v>` element, unparsed.
        v: Arc<str>,
    },
    /// Error cell (`t="e"`), e.g. `#REF!` or `#DIV/0!`.
    Error {
        /// The Excel error code as written in the sheet XML.
        v: Arc<str>,
    },
    /// Date-formatted numeric cell decoded against the workbook's date system
    /// (1900 or 1904). Only produced when `decode_dates` is `true`.
    Date {
        /// ISO 8601 representation (`YYYY-MM-DD[THH:MM:SS]`).
        iso: String,
        /// Original Excel serial date value, preserved for round-tripping.
        serial: f64,
    },
}

/// Benchmark-only helpers exposed for `benches/` and `examples/`. Not part of
/// the stable public API; callers outside this crate should use
/// [`parse_xlsx`] directly.
pub mod bench_support {
    use super::*;
    use std::path::Path;

    /// Aggregate cell counts produced by [`count_cells_stats`].
    #[derive(Debug, Clone, Copy)]
    pub struct CountStats {
        /// Total cells visited in the sheet stream (including empty entries).
        pub total: u64,
        /// Subset of `total` whose value is not [`CellValue::Empty`].
        pub non_empty: u64,
    }

    /// Re-export of the crate-internal SIMD-accelerated cell-ref parser, used
    /// by `benches/cell_ref.rs` to measure decode throughput in isolation.
    pub fn parse_cell_ref_bytes(bytes: &[u8]) -> Option<(u32, u32)> {
        crate::parse_cell_ref_bytes(bytes)
    }

    /// Count every cell encountered while streaming the workbook. Equivalent
    /// to `count_cells_stats(...).total`.
    pub fn count_cells(path: impl AsRef<Path>, options: ParseOptions) -> Result<u64> {
        Ok(count_cells_stats(path, options)?.total)
    }

    /// Count only cells whose value is not [`CellValue::Empty`]. Equivalent to
    /// `count_cells_stats(...).non_empty`.
    pub fn count_cells_used(path: impl AsRef<Path>, options: ParseOptions) -> Result<u64> {
        Ok(count_cells_stats(path, options)?.non_empty)
    }

    /// Stream the workbook and return both total and non-empty cell counts in
    /// a single pass; used by `benches/compare.rs` to benchmark parser
    /// throughput without materializing a [`Workbook`].
    pub fn count_cells_stats(path: impl AsRef<Path>, options: ParseOptions) -> Result<CountStats> {
        let path = path.as_ref();
        let archive = open_archive(path)?;

        let workbook_path = archive.office_document_path(options.max_inflate, "xl/workbook.xml")?;

        let workbook_xml = read_part_bytes(&archive, &workbook_path, options.max_inflate)
            .with_context(|| format!("read workbook part: {workbook_path}"))?;

        let workbook_info = parse_workbook(&workbook_xml)?;
        let workbook_dir = part_dir(&workbook_path);
        let workbook_rels_path = rels_path_for_part(&workbook_path);
        let workbook_rels_xml = read_part_bytes(&archive, &workbook_rels_path, options.max_inflate)
            .with_context(|| format!("read workbook rels: {workbook_rels_path}"))?;
        let workbook_rels = parse_relationships(&workbook_rels_xml)?;

        let shared_strings_path = workbook_rels
            .by_type
            .get(REL_SHARED_STRINGS)
            .map(|t| resolve_part(&workbook_dir, t));
        let styles_path = workbook_rels
            .by_type
            .get(REL_STYLES)
            .map(|t| resolve_part(&workbook_dir, t));

        let shared_strings = if let Some(path) = shared_strings_path.as_deref() {
            if let Some(reader) = open_optional_part_reader(&archive, path, options.max_inflate)? {
                parse_shared_strings_reader(reader, options.max_inflate)?
            } else {
                StringTable::default()
            }
        } else {
            StringTable::default()
        };

        let styles = if options.decode_dates {
            if let Some(path) = styles_path.as_deref() {
                if let Some(xml) = read_optional_part_bytes(&archive, path, options.max_inflate)? {
                    parse_styles(&xml)?
                } else {
                    Styles::default()
                }
            } else {
                Styles::default()
            }
        } else {
            Styles::default()
        };

        let mut total: u64 = 0;
        let mut non_empty: u64 = 0;
        for sheet in workbook_info.sheets {
            let target = workbook_rels
                .by_id
                .get(&sheet.rel_id)
                .ok_or_else(|| anyhow!("missing relationship for sheet {}", sheet.name))?;
            let sheet_path = resolve_part(&workbook_dir, target);
            let reader = open_part_reader(&archive, &sheet_path, options.max_inflate)
                .with_context(|| format!("read sheet: {sheet_path}"))?;
            let xml_reader = Reader::from_reader(repairing(reader, options.max_inflate));
            parse_sheet_reader_with(
                xml_reader,
                &shared_strings,
                &styles,
                workbook_info.date_1904,
                &options,
                None,
                |cell| {
                    total += 1;
                    if !matches!(cell.value, CellValueRef::Empty) {
                        non_empty += 1;
                    }
                    Ok(())
                },
            )?;
        }

        Ok(CountStats { total, non_empty })
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CellValueRef<'a> {
    Empty,
    Bool { v: bool },
    Number { v: f64 },
    String { v: &'a str },
    Raw { v: &'a str },
    Error { v: &'a str },
    Date { iso: String, serial: f64 },
}

/// One NDJSON worksheet header. Emitted once per sheet, before its cells, so a
/// streaming reader can attribute following `cell` records without buffering
/// the sheet.
#[derive(Debug, Serialize)]
struct NdjsonSheet<'a> {
    #[serde(rename = "kind")]
    kind: &'static str,
    id: u32,
    name: &'a str,
}

/// One NDJSON cell record. `sheet_id` is repeated on every line on purpose: a
/// reader may start mid-stream and still know which sheet a cell belongs to.
#[derive(Debug, Serialize)]
struct NdjsonCell<'a> {
    #[serde(rename = "kind")]
    kind: &'static str,
    sheet_id: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    sheet_name: Option<&'a str>,
    row: u32,
    col: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    formula: Option<&'a str>,
    #[serde(flatten)]
    value: CellValueRef<'a>,
}

#[derive(Debug)]
struct CellPartsRef<'a> {
    row: u32,
    col: u32,
    formula: Option<&'a str>,
    value: CellValueRef<'a>,
    /// Index into the shared-string table when `value` is that entry, so a
    /// caller that keeps cells can share one allocation per entry.
    shared_string: Option<usize>,
}

#[derive(Debug, Default)]
struct WorkbookInfo {
    date_1904: bool,
    sheets: Vec<SheetInfo>,
}

#[derive(Debug, Default, Clone)]
struct SheetInfo {
    name: String,
    sheet_id: u32,
    rel_id: String,
}

#[derive(Debug, Default)]
struct Styles {
    is_date_style: Vec<bool>,
}

#[derive(Debug, Clone, Copy)]
enum CellType {
    Number,
    SharedString,
    Bool,
    Error,
    Str,
    InlineStr,
}

#[derive(Debug, Default)]
struct StringTable {
    data: Vec<u8>,
    index: Vec<(usize, usize)>,
}

impl StringTable {
    fn push(&mut self, value: &str) {
        let start = self.data.len();
        self.data.extend_from_slice(value.as_bytes());
        self.index.push((start, value.len()));
    }

    fn get(&self, idx: usize) -> Option<&str> {
        let (start, len) = *self.index.get(idx)?;
        std::str::from_utf8(&self.data[start..start + len]).ok()
    }

    fn len(&self) -> usize {
        self.index.len()
    }
}

type XlsxArchive = OoxmlArchive;

fn open_archive(path: &Path) -> Result<XlsxArchive> {
    OoxmlArchive::open(path)
}

#[cfg(feature = "arrow")]
fn open_archive_with_budget(
    path: &Path,
    inflate_budget: SharedInflateBudget,
) -> Result<XlsxArchive> {
    OoxmlArchive::open_with_budget(path, inflate_budget)
}

fn open_archive_bytes(bytes: Vec<u8>) -> Result<XlsxArchive> {
    OoxmlArchive::from_bytes(bytes)
}

fn read_part_bytes<'a>(
    archive: &'a XlsxArchive,
    name: &str,
    max_inflate: usize,
) -> Result<ooxml_archive::PartBytes<'a>> {
    archive.read_part(name, max_inflate)
}

fn open_part_reader<'a>(
    archive: &'a XlsxArchive,
    name: &str,
    max_inflate: usize,
) -> Result<PartReader<'a>> {
    archive.open_part_reader(name, max_inflate, false)
}

fn read_optional_part_bytes<'a>(
    archive: &'a XlsxArchive,
    name: &str,
    max_inflate: usize,
) -> Result<Option<ooxml_archive::PartBytes<'a>>> {
    if !archive.has_part(name) {
        return Ok(None);
    }
    read_part_bytes(archive, name, max_inflate)
        .map(Some)
        .with_context(|| format!("read optional XLSX part: {name}"))
}

fn open_optional_part_reader<'a>(
    archive: &'a XlsxArchive,
    name: &str,
    max_inflate: usize,
) -> Result<Option<PartReader<'a>>> {
    if !archive.has_part(name) {
        return Ok(None);
    }
    open_part_reader(archive, name, max_inflate)
        .map(Some)
        .with_context(|| format!("read optional XLSX part: {name}"))
}

/// Parse an `.xlsx` file from disk and return a fully materialized [`Workbook`].
///
/// Memory-maps the archive, decodes shared strings and styles once, then walks
/// every sheet stream to collect [`Cell`]s into [`Sheet`]s. Suitable for files
/// that fit comfortably in memory.
pub fn parse_xlsx(path: impl AsRef<Path>, options: ParseOptions) -> Result<Workbook> {
    let path = path.as_ref();
    let archive = open_archive(path)?;
    parse_xlsx_from_archive(archive, options)
}

/// Parse an Excel (.xlsx) workbook from raw bytes.
///
/// Accepts the complete OOXML package as a byte slice (e.g. from an HTTP
/// upload or in-memory buffer) and returns a fully materialized
/// [`Workbook`].  No temporary file is written.
pub fn parse_xlsx_bytes(bytes: Vec<u8>, options: ParseOptions) -> Result<Workbook> {
    let archive = open_archive_bytes(bytes)?;
    parse_xlsx_from_archive(archive, options)
}

/// Internal: parse an XLSX from an already-open archive.
fn parse_xlsx_from_archive(archive: XlsxArchive, options: ParseOptions) -> Result<Workbook> {
    let workbook_path = archive.office_document_path(options.max_inflate, "xl/workbook.xml")?;

    let workbook_xml = read_part_bytes(&archive, &workbook_path, options.max_inflate)
        .with_context(|| format!("read workbook part: {workbook_path}"))?;

    let workbook_info = parse_workbook(&workbook_xml)?;
    let workbook_dir = part_dir(&workbook_path);
    let workbook_rels_path = rels_path_for_part(&workbook_path);
    let workbook_rels_xml = read_part_bytes(&archive, &workbook_rels_path, options.max_inflate)
        .with_context(|| format!("read workbook rels: {workbook_rels_path}"))?;
    let workbook_rels = parse_relationships(&workbook_rels_xml)?;

    let shared_strings_path = workbook_rels
        .by_type
        .get(REL_SHARED_STRINGS)
        .map(|t| resolve_part(&workbook_dir, t));
    let styles_path = workbook_rels
        .by_type
        .get(REL_STYLES)
        .map(|t| resolve_part(&workbook_dir, t));

    let shared_strings = if let Some(path) = shared_strings_path.as_deref() {
        if let Some(reader) = open_optional_part_reader(&archive, path, options.max_inflate)? {
            parse_shared_strings_reader(reader, options.max_inflate)?
        } else {
            StringTable::default()
        }
    } else {
        StringTable::default()
    };

    let styles = if options.decode_dates {
        if let Some(path) = styles_path.as_deref() {
            if let Some(xml) = read_optional_part_bytes(&archive, path, options.max_inflate)? {
                parse_styles(&xml)?
            } else {
                Styles::default()
            }
        } else {
            Styles::default()
        }
    } else {
        Styles::default()
    };

    let mut sheets = Vec::with_capacity(workbook_info.sheets.len());
    // One allocation per shared string that is actually used, however many
    // cells refer to it. Copying the text into every cell instead lets a
    // small file with one long string and many references to it allocate
    // their product.
    let mut shared_values: Vec<Option<Arc<str>>> = vec![None; shared_strings.len()];

    for sheet in workbook_info.sheets {
        let target = workbook_rels
            .by_id
            .get(&sheet.rel_id)
            .ok_or_else(|| anyhow!("missing relationship for sheet {}", sheet.name))?;
        let sheet_path = resolve_part(&workbook_dir, target);
        let reader = open_part_reader(&archive, &sheet_path, options.max_inflate)
            .with_context(|| format!("read sheet: {sheet_path}"))?;
        let mut cells = Vec::new();
        let xml_reader = Reader::from_reader(repairing(reader, options.max_inflate));
        parse_sheet_reader_with(
            xml_reader,
            &shared_strings,
            &styles,
            workbook_info.date_1904,
            &options,
            None,
            |parts| {
                let shared_slot = parts
                    .shared_string
                    .and_then(|index| shared_values.get_mut(index));
                let value = match (shared_slot, parts.value) {
                    (Some(slot), CellValueRef::String { v }) => CellValue::String {
                        v: Arc::clone(slot.get_or_insert_with(|| Arc::from(v))),
                    },
                    (_, value) => cell_value_ref_to_owned(value),
                };
                cells.push(Cell {
                    row: parts.row,
                    col: parts.col,
                    formula: parts.formula.map(str::to_string),
                    value,
                });
                Ok(())
            },
        )?;
        sheets.push(Sheet {
            id: sheet.sheet_id,
            name: sheet.name,
            cells,
        });
    }

    Ok(Workbook { sheets })
}

/// Relationship type for the shared-string table part.
pub const REL_SHARED_STRINGS: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings";
/// Relationship type for the styles part, which carries date formatting.
pub const REL_STYLES: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles";

/// Stream a workbook to `out_path` as NDJSON: one `sheet` record per worksheet
/// followed by one `cell` record per non-empty cell.
pub fn parse_xlsx_to_ndjson(
    path: impl AsRef<Path>,
    out_path: impl AsRef<Path>,
    options: ParseOptions,
) -> Result<()> {
    let mut out = std::io::BufWriter::new(
        File::create(out_path.as_ref())
            .with_context(|| format!("create output: {}", out_path.as_ref().display()))?,
    );
    parse_xlsx_to_writer(path, &mut out, options)
}

/// Streaming variant of [`parse_xlsx_to_ndjson`] that writes NDJSON records
/// into an arbitrary [`Write`](std::io::Write) sink (stdout, network socket,
/// in-memory buffer). The same record schema is used: one `sheet` line per
/// worksheet followed by one `cell` line per non-empty cell.
pub fn parse_xlsx_to_writer<W: std::io::Write>(
    path: impl AsRef<Path>,
    out: &mut W,
    options: ParseOptions,
) -> Result<()> {
    let path = path.as_ref();
    let archive = open_archive(path)?;

    let workbook_path = archive.office_document_path(options.max_inflate, "xl/workbook.xml")?;

    let workbook_xml = read_part_bytes(&archive, &workbook_path, options.max_inflate)
        .with_context(|| format!("read workbook part: {workbook_path}"))?;

    let workbook_info = parse_workbook(&workbook_xml)?;
    let workbook_dir = part_dir(&workbook_path);
    let workbook_rels_path = rels_path_for_part(&workbook_path);
    let workbook_rels_xml = read_part_bytes(&archive, &workbook_rels_path, options.max_inflate)
        .with_context(|| format!("read workbook rels: {workbook_rels_path}"))?;
    let workbook_rels = parse_relationships(&workbook_rels_xml)?;

    let shared_strings_path = workbook_rels
        .by_type
        .get(REL_SHARED_STRINGS)
        .map(|t| resolve_part(&workbook_dir, t));
    let styles_path = workbook_rels
        .by_type
        .get(REL_STYLES)
        .map(|t| resolve_part(&workbook_dir, t));

    let shared_strings = if let Some(path) = shared_strings_path.as_deref() {
        if let Ok(reader) = open_part_reader(&archive, path, options.max_inflate) {
            parse_shared_strings_reader(reader, options.max_inflate)?
        } else {
            StringTable::default()
        }
    } else {
        StringTable::default()
    };

    let styles = if options.decode_dates {
        if let Some(path) = styles_path.as_deref() {
            if let Ok(xml) = read_part_bytes(&archive, path, options.max_inflate) {
                parse_styles(&xml)?
            } else {
                Styles::default()
            }
        } else {
            Styles::default()
        }
    } else {
        Styles::default()
    };

    for sheet in workbook_info.sheets {
        let target = workbook_rels
            .by_id
            .get(&sheet.rel_id)
            .ok_or_else(|| anyhow!("missing relationship for sheet {}", sheet.name))?;
        let sheet_path = resolve_part(&workbook_dir, target);

        let sheet_record = NdjsonSheet {
            kind: "sheet",
            id: sheet.sheet_id,
            name: &sheet.name,
        };
        serde_json::to_writer(&mut *out, &sheet_record)?;
        out.write_all(b"\n")?;

        let reader = open_part_reader(&archive, &sheet_path, options.max_inflate)
            .with_context(|| format!("read sheet: {sheet_path}"))?;
        let xml_reader = Reader::from_reader(repairing(reader, options.max_inflate));
        parse_sheet_reader_with(
            xml_reader,
            &shared_strings,
            &styles,
            workbook_info.date_1904,
            &options,
            None,
            |cell| {
                let cell_record = NdjsonCell {
                    kind: "cell",
                    sheet_id: sheet.sheet_id,
                    sheet_name: None,
                    row: cell.row,
                    col: cell.col,
                    formula: cell.formula,
                    value: cell.value,
                };
                serde_json::to_writer(&mut *out, &cell_record)?;
                out.write_all(b"\n")?;
                Ok(())
            },
        )?;
    }

    Ok(())
}

fn parse_workbook(xml: &[u8]) -> Result<WorkbookInfo> {
    let mut reader = Reader::from_reader(repairing(Cursor::new(xml), buffer_limit(xml)));
    configure_reader(&mut reader);
    let mut buf = Vec::new();
    let mut info = WorkbookInfo::default();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let qname = e.name();
                let name = local_name(qname.as_ref().as_bytes());
                if name == b"workbookPr" {
                    for attr in e.attributes().with_checks(false).flatten() {
                        if local_name(attr.key.as_ref().as_bytes()) == b"date1904" {
                            let value = attribute_value(&attr, reader.get_ref(), &e)?.to_string();
                            info.date_1904 = value == "1" || value.eq_ignore_ascii_case("true");
                        }
                    }
                } else if name == b"sheet" {
                    let mut sheet = SheetInfo::default();
                    for attr in e.attributes().with_checks(false).flatten() {
                        let key_raw = attr.key;
                        let key = local_name(key_raw.as_ref().as_bytes());
                        if key == b"name" {
                            sheet.name = attribute_value(&attr, reader.get_ref(), &e)?.to_string();
                        } else if key == b"sheetId" {
                            let value = attribute_value(&attr, reader.get_ref(), &e)?;
                            sheet.sheet_id =
                                lexical_core::parse::<u32>(value.as_bytes()).unwrap_or(0);
                        } else if key == b"r:id" || key == b"id" {
                            sheet.rel_id =
                                attribute_value(&attr, reader.get_ref(), &e)?.to_string();
                        }
                    }
                    if !sheet.rel_id.is_empty() {
                        info.sheets.push(sheet);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(err) => return Err(anyhow!("parse workbook: {err}")),
            _ => {}
        }
        buf.clear();
    }
    Ok(info)
}

fn parse_shared_strings_reader<R: BufRead>(reader: R, limit: usize) -> Result<StringTable> {
    let mut reader = Reader::from_reader(repairing(reader, limit));
    configure_reader(&mut reader);
    let mut buf = Vec::new();
    let mut strings = StringTable::default();

    let mut in_si = false;
    let mut in_t = false;
    // Phonetic runs (`rPh`) hold a reading aid, not cell text.
    let mut phonetic_depth = 0u32;
    let mut text_start = 0;
    let mut current = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let qname = e.name();
                let name = local_name(qname.as_ref().as_bytes());

                if name == b"si" {
                    in_si = true;
                    in_t = false;
                    phonetic_depth = 0;
                    current.clear();
                } else if in_si && name == b"rPh" {
                    phonetic_depth = phonetic_depth.saturating_add(1);
                } else if in_si && name == b"t" && phonetic_depth == 0 {
                    in_t = true;
                    text_start = current.len();
                }
            }
            Ok(Event::Empty(e)) => {
                // `<si/>` is an empty string that still occupies an index.
                if !in_si && local_name(e.name().as_ref().as_bytes()) == b"si" {
                    strings.push("");
                }
            }
            Ok(event @ (Event::Text(_) | Event::CData(_) | Event::GeneralRef(_))) => {
                if in_t {
                    push_character_data(&event, &mut current);
                }
            }
            Ok(Event::End(e)) => {
                let qname = e.name();
                let name = local_name(qname.as_ref().as_bytes());

                if name == b"t" {
                    if in_t {
                        in_t = false;
                        decode_xstring_tail(&mut current, text_start);
                    }
                } else if name == b"rPh" {
                    phonetic_depth = phonetic_depth.saturating_sub(1);
                } else if name == b"si" {
                    in_si = false;
                    in_t = false;
                    strings.push(current.as_str());
                    current.clear();
                }
            }
            Ok(Event::Eof) => break,
            Err(err) => return Err(anyhow!("parse shared strings: {err}")),
            _ => {}
        }
        buf.clear();
    }

    Ok(strings)
}

fn parse_styles(xml: &[u8]) -> Result<Styles> {
    let mut reader = Reader::from_reader(repairing(Cursor::new(xml), buffer_limit(xml)));
    configure_reader(&mut reader);
    let mut buf = Vec::new();

    let mut num_fmts: HashMap<u32, String> = HashMap::new();
    let mut is_date_style = Vec::new();
    let mut in_cell_xfs = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let qname = e.name();
                let name = local_name(qname.as_ref().as_bytes());

                if name == b"numFmt" {
                    let mut id = None;
                    let mut code = None;
                    for attr in e.attributes().with_checks(false).flatten() {
                        let key_raw = attr.key;
                        let key = local_name(key_raw.as_ref().as_bytes());
                        if key == b"numFmtId" {
                            let value = attribute_value(&attr, reader.get_ref(), &e)?;
                            id = lexical_core::parse::<u32>(value.as_bytes()).ok();
                        } else if key == b"formatCode" {
                            code = Some(attribute_value(&attr, reader.get_ref(), &e)?.to_string());
                        }
                    }
                    if let (Some(id), Some(code)) = (id, code) {
                        num_fmts.insert(id, code);
                    }
                } else if name == b"cellXfs" {
                    in_cell_xfs = true;
                } else if in_cell_xfs && name == b"xf" {
                    let mut num_fmt_id = None;
                    for attr in e.attributes().with_checks(false).flatten() {
                        if local_name(attr.key.as_ref().as_bytes()) == b"numFmtId" {
                            let value = attribute_value(&attr, reader.get_ref(), &e)?;
                            num_fmt_id = lexical_core::parse::<u32>(value.as_bytes()).ok();
                        }
                    }
                    let is_date = num_fmt_id
                        .map(|id| is_date_format_id(id, &num_fmts))
                        .unwrap_or(false);
                    is_date_style.push(is_date);
                }
            }
            Ok(Event::End(e)) => {
                if local_name(e.name().as_ref().as_bytes()) == b"cellXfs" {
                    in_cell_xfs = false;
                }
            }
            Ok(Event::Eof) => break,
            Err(err) => return Err(anyhow!("parse styles: {err}")),
            _ => {}
        }
        buf.clear();
    }

    Ok(Styles { is_date_style })
}

fn is_date_format_id(id: u32, custom: &HashMap<u32, String>) -> bool {
    const BUILTIN_DATE_FORMATS: &[u32] = &[
        14, 15, 16, 17, 18, 19, 20, 21, 22, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 45, 46, 47, 50,
        51, 52, 53, 54, 55, 56, 57, 58,
    ];
    if BUILTIN_DATE_FORMATS.contains(&id) {
        return true;
    }
    if let Some(code) = custom.get(&id) {
        return format_has_date_tokens(code);
    }
    false
}

fn format_has_date_tokens(code: &str) -> bool {
    let mut in_quotes = false;
    let mut in_brackets = 0u32;
    let mut escape = false;
    for ch in code.chars() {
        if escape {
            escape = false;
            continue;
        }
        match ch {
            '\\' => {
                escape = true;
            }
            '"' => {
                in_quotes = !in_quotes;
            }
            '[' => {
                if !in_quotes {
                    in_brackets += 1;
                }
            }
            ']' => {
                if !in_quotes && in_brackets > 0 {
                    in_brackets -= 1;
                }
            }
            _ => {
                if in_quotes || in_brackets > 0 {
                    continue;
                }
                let c = ch.to_ascii_lowercase();
                if matches!(c, 'y' | 'm' | 'd' | 'h' | 's') {
                    return true;
                }
            }
        }
    }
    false
}

fn parse_sheet_reader_with<R: std::io::BufRead, F>(
    mut reader: Reader<LossyUtf8<R>>,
    shared_strings: &StringTable,
    styles: &Styles,
    date_1904: bool,
    options: &ParseOptions,
    range_filter: Option<&[RangeFilter]>,
    mut emit: F,
) -> Result<()>
where
    F: for<'a> FnMut(CellPartsRef<'a>) -> Result<()>,
{
    configure_reader(&mut reader);
    let mut buf = Vec::new();
    let mut in_sheet_data = false;
    let mut current_row: u32 = 0;
    let mut col_cursor: u32 = 0;
    let mut row_outside = false;

    let ranges = range_filter.unwrap_or(&[]);
    let has_ranges = !ranges.is_empty();
    let mut row_min = u32::MAX;
    let mut row_max = 0;
    let mut col_min = u32::MAX;
    let mut col_max = 0;
    if has_ranges {
        for range in ranges {
            row_min = row_min.min(range.row_start);
            row_max = row_max.max(range.row_end);
            col_min = col_min.min(range.col_start);
            col_max = col_max.max(range.col_end);
        }
    }

    let mut in_cell = false;
    let mut cell_row: u32 = 0;
    let mut cell_col: u32 = 0;
    let mut cell_type = CellType::Number;
    let mut cell_style: Option<u32> = None;
    let mut cell_value = String::new();
    let mut has_value = false;
    let mut cell_formula = String::new();
    let mut has_formula = false;
    let mut in_value = false;
    let mut in_formula = false;
    let mut in_inline_str = false;
    let mut in_inline_t = false;
    // Phonetic runs (`rPh`) inside an inline string are not cell text.
    let mut inline_phonetic_depth = 0u32;
    let mut inline_text_start = 0;
    let mut inline_buf = String::new();
    let mut skip_cell = false;
    let mut skip_row_depth: u32 = 0;
    // Last valid row seen: a `<row>` without `r` is the one after it.
    let mut last_row: u32 = 0;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let qname = e.name();
                let name = local_name(qname.as_ref().as_bytes());

                if skip_row_depth > 0 {
                    if name == b"row" {
                        skip_row_depth += 1;
                    }
                    buf.clear();
                    continue;
                }
                match name {
                    b"sheetData" => in_sheet_data = true,
                    b"row" if in_sheet_data => {
                        current_row = row_index(&e, reader.get_ref(), last_row)?;
                        col_cursor = 0;
                        if !EXCEL_ROWS.contains(&current_row) {
                            row_outside = true;
                            skip_row_depth = 1;
                        } else if has_ranges {
                            last_row = current_row;
                            row_outside = current_row < row_min || current_row > row_max;
                        } else {
                            last_row = current_row;
                            row_outside = false;
                        }
                        if row_outside {
                            skip_row_depth = 1;
                        }
                    }
                    b"c" if in_sheet_data => {
                        in_cell = true;
                        skip_cell = false;
                        cell_type = CellType::Number;
                        cell_style = None;
                        cell_value.clear();
                        cell_formula.clear();
                        has_value = false;
                        has_formula = false;
                        in_inline_str = false;
                        in_inline_t = false;
                        inline_phonetic_depth = 0;
                        inline_buf.clear();
                        let mut cell_ref_row: u32 = 0;
                        let mut cell_ref_col: u32 = 0;
                        let mut has_cell_ref = false;
                        let mut invalid_cell_ref = false;
                        for attr in e.attributes().with_checks(false).flatten() {
                            let key_raw = attr.key;
                            let key = local_name(key_raw.as_ref().as_bytes());
                            if key == b"r" {
                                let value = attribute_value(&attr, reader.get_ref(), &e)?;
                                if let Some((row, col)) = parse_cell_ref(value.as_ref()) {
                                    cell_ref_row = row;
                                    cell_ref_col = col;
                                    has_cell_ref = true;
                                } else {
                                    invalid_cell_ref = true;
                                }
                            } else if key == b"t" {
                                let value = attribute_value(&attr, reader.get_ref(), &e)?;
                                cell_type = match value.as_ref() {
                                    "s" => CellType::SharedString,
                                    "b" => CellType::Bool,
                                    "e" => CellType::Error,
                                    "str" => CellType::Str,
                                    "inlineStr" => CellType::InlineStr,
                                    _ => CellType::Number,
                                };
                            } else if key == b"s" {
                                let value = attribute_value(&attr, reader.get_ref(), &e)?;
                                cell_style = lexical_core::parse::<u32>(value.as_bytes()).ok();
                            }
                        }
                        if invalid_cell_ref {
                            skip_cell = true;
                            cell_row = 0;
                            cell_col = 0;
                        } else if has_cell_ref {
                            cell_row = if cell_ref_row > 0 {
                                cell_ref_row
                            } else {
                                current_row
                            };
                            cell_col = if cell_ref_col > 0 {
                                cell_ref_col
                            } else {
                                col_cursor.saturating_add(1)
                            };
                        } else {
                            cell_row = current_row;
                            cell_col = col_cursor.saturating_add(1);
                        }
                        col_cursor = cell_col;
                        if has_ranges {
                            if cell_row == 0 || cell_col == 0 {
                                skip_cell = invalid_cell_ref;
                            } else if row_outside
                                || cell_row < row_min
                                || cell_row > row_max
                                || cell_col < col_min
                                || cell_col > col_max
                            {
                                skip_cell = true;
                            } else {
                                let mut matched = false;
                                for range in ranges {
                                    if range.contains(cell_row, cell_col) {
                                        matched = true;
                                        break;
                                    }
                                }
                                skip_cell = !matched;
                            }
                        }
                    }
                    b"v" if in_cell => {
                        in_value = true;
                    }
                    b"f" if in_cell => {
                        in_formula = true;
                    }
                    b"is" if in_cell => {
                        in_inline_str = true;
                        inline_phonetic_depth = 0;
                    }
                    b"rPh" if in_inline_str => {
                        inline_phonetic_depth = inline_phonetic_depth.saturating_add(1);
                    }
                    b"t" if in_inline_str && inline_phonetic_depth == 0 => {
                        in_inline_t = true;
                        inline_text_start = inline_buf.len();
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(e)) => {
                if skip_row_depth > 0 {
                    buf.clear();
                    continue;
                }
                if in_sheet_data {
                    let qname = e.name();
                    match local_name(qname.as_ref().as_bytes()) {
                        // An empty row still takes its number, so a following
                        // `<row>` without `r` is counted from it.
                        b"row" => {
                            let row = row_index(&e, reader.get_ref(), last_row)?;
                            if EXCEL_ROWS.contains(&row) {
                                last_row = row;
                            }
                        }
                        // A valueless cell emits nothing but still occupies
                        // its column, so a following cell without `r` lands
                        // in the right place.
                        b"c" if !in_cell => {
                            col_cursor = empty_cell_column(&e, reader.get_ref(), col_cursor)?;
                        }
                        _ => {}
                    }
                }
            }
            Ok(event @ (Event::Text(_) | Event::CData(_) | Event::GeneralRef(_))) => {
                if skip_row_depth > 0 || skip_cell {
                    buf.clear();
                    continue;
                }
                // Only text that lands in a cell is decoded; the whitespace
                // between elements is most of a sheet's text events.
                let target = if in_value {
                    has_value = true;
                    Some(&mut cell_value)
                } else if in_formula {
                    if options.include_formulas {
                        has_formula = true;
                        Some(&mut cell_formula)
                    } else {
                        None
                    }
                } else if in_inline_t {
                    Some(&mut inline_buf)
                } else {
                    None
                };
                if let Some(target) = target {
                    push_character_data(&event, target);
                }
            }
            Ok(Event::End(e)) => {
                let qname = e.name();
                let name = local_name(qname.as_ref().as_bytes());

                if skip_row_depth > 0 {
                    if name == b"row" {
                        skip_row_depth = skip_row_depth.saturating_sub(1);
                        row_outside = false;
                    }
                    buf.clear();
                    continue;
                }
                match name {
                    b"sheetData" => in_sheet_data = false,
                    b"v" => in_value = false,
                    b"f" => in_formula = false,
                    b"t" => {
                        if in_inline_t {
                            in_inline_t = false;
                            decode_xstring_tail(&mut inline_buf, inline_text_start);
                        }
                    }
                    b"rPh" => {
                        inline_phonetic_depth = inline_phonetic_depth.saturating_sub(1);
                    }
                    b"is" => {
                        in_inline_str = false;
                        in_inline_t = false;
                    }
                    b"c" if in_cell => {
                        if skip_cell {
                            in_cell = false;
                            skip_cell = false;
                            continue;
                        }
                        let raw = if has_value {
                            Some(cell_value.as_str())
                        } else {
                            None
                        };
                        let shared_string = if inline_buf.is_empty() {
                            shared_string_index(cell_type, raw)
                        } else {
                            None
                        };
                        let value = if options.raw_values {
                            decode_cell_value_raw(
                                cell_type,
                                raw,
                                if inline_buf.is_empty() {
                                    None
                                } else {
                                    Some(&inline_buf)
                                },
                                shared_strings,
                            )
                        } else {
                            decode_cell_value_ref(
                                cell_type,
                                raw,
                                if inline_buf.is_empty() {
                                    None
                                } else {
                                    Some(&inline_buf)
                                },
                                cell_style,
                                CellDecodeContext {
                                    shared_strings,
                                    styles,
                                    date_1904,
                                    options,
                                },
                            )
                        };
                        let formula = if options.include_formulas && has_formula {
                            Some(cell_formula.as_str())
                        } else {
                            None
                        };
                        emit(CellPartsRef {
                            row: cell_row,
                            col: cell_col,
                            formula,
                            value,
                            shared_string,
                        })?;
                        in_cell = false;
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(err) => return Err(anyhow!("parse sheet: {err}")),
            _ => {}
        }
        buf.clear();
    }

    Ok(())
}

/// Rows a worksheet can address (1-based), per the Excel limits.
const EXCEL_ROWS: std::ops::RangeInclusive<u32> = 1..=1_048_576;

/// Returns the 1-based row a `<row>` element addresses.
///
/// `r` is optional: without it the row is the one after `previous`. A present
/// but unreadable `r` yields 0, which no worksheet row has.
fn row_index<R: BufRead>(
    element: &BytesStart<'_>,
    input: &LossyUtf8<R>,
    previous: u32,
) -> Result<u32> {
    for attr in element.attributes().with_checks(false).flatten() {
        if attr.key.as_ref().as_bytes() == b"r" {
            let value = attribute_value(&attr, input, element)?;
            return Ok(lexical_core::parse::<u32>(value.as_bytes()).unwrap_or(0));
        }
    }
    Ok(previous.saturating_add(1))
}

/// Returns the column cursor after a self-closing `<c/>` element.
///
/// Mirrors how a cell with content moves the cursor: to its `r` column, to 0
/// when `r` is unreadable, and one to the right when `r` is absent.
fn empty_cell_column<R: BufRead>(
    element: &BytesStart<'_>,
    input: &LossyUtf8<R>,
    col_cursor: u32,
) -> Result<u32> {
    for attr in element.attributes().with_checks(false).flatten() {
        if local_name(attr.key.as_ref().as_bytes()) == b"r" {
            let value = attribute_value(&attr, input, element)?;
            return Ok(parse_cell_ref(value.as_ref()).map_or(0, |(_, col)| col));
        }
    }
    Ok(col_cursor.saturating_add(1))
}

/// Returns the shared-string index a `t="s"` cell's `<v>` holds.
fn shared_string_index(cell_type: CellType, raw_value: Option<&str>) -> Option<usize> {
    match cell_type {
        CellType::SharedString => {
            raw_value.and_then(|v| lexical_core::parse::<usize>(v.as_bytes()).ok())
        }
        _ => None,
    }
}

#[derive(Clone, Copy)]
struct CellDecodeContext<'a> {
    shared_strings: &'a StringTable,
    styles: &'a Styles,
    date_1904: bool,
    options: &'a ParseOptions,
}

fn decode_cell_value_ref<'a>(
    cell_type: CellType,
    raw_value: Option<&'a str>,
    inline_str: Option<&'a str>,
    cell_style: Option<u32>,
    context: CellDecodeContext<'a>,
) -> CellValueRef<'a> {
    if let Some(s) = inline_str {
        return CellValueRef::String { v: s };
    }

    match cell_type {
        CellType::SharedString => {
            if let Some(v) = shared_string_index(cell_type, raw_value) {
                if let Some(s) = context.shared_strings.get(v) {
                    return CellValueRef::String { v: s };
                }
            }
            CellValueRef::Empty
        }
        CellType::Bool => {
            let v = raw_value == Some("1");
            CellValueRef::Bool { v }
        }
        CellType::Error => CellValueRef::Error {
            v: raw_value.unwrap_or(""),
        },
        CellType::Str => CellValueRef::String {
            v: raw_value.unwrap_or(""),
        },
        CellType::InlineStr => CellValueRef::String {
            v: raw_value.unwrap_or(""),
        },
        CellType::Number => {
            if let Some(raw) = raw_value {
                if let Ok(num) = lexical_core::parse::<f64>(raw.as_bytes()) {
                    if context.options.decode_dates && is_date_style(cell_style, context.styles) {
                        if let Some(iso) = excel_serial_to_iso(num, context.date_1904) {
                            return CellValueRef::Date { iso, serial: num };
                        }
                    }
                    return CellValueRef::Number { v: num };
                }
            }
            CellValueRef::Empty
        }
    }
}

fn decode_cell_value_raw<'a>(
    cell_type: CellType,
    raw_value: Option<&'a str>,
    inline_str: Option<&'a str>,
    shared_strings: &'a StringTable,
) -> CellValueRef<'a> {
    if let Some(s) = inline_str {
        return CellValueRef::String { v: s };
    }

    match cell_type {
        CellType::SharedString => {
            if let Some(v) = shared_string_index(cell_type, raw_value) {
                if let Some(s) = shared_strings.get(v) {
                    return CellValueRef::String { v: s };
                }
            }
            CellValueRef::Empty
        }
        CellType::InlineStr | CellType::Str => CellValueRef::String {
            v: raw_value.unwrap_or(""),
        },
        CellType::Bool | CellType::Error | CellType::Number => {
            if let Some(v) = raw_value {
                CellValueRef::Raw { v }
            } else {
                CellValueRef::Empty
            }
        }
    }
}

fn cell_value_ref_to_owned(value: CellValueRef<'_>) -> CellValue {
    match value {
        CellValueRef::Empty => CellValue::Empty,
        CellValueRef::Bool { v } => CellValue::Bool { v },
        CellValueRef::Number { v } => CellValue::Number { v },
        CellValueRef::String { v } => CellValue::String { v: Arc::from(v) },
        CellValueRef::Raw { v } => CellValue::Raw { v: Arc::from(v) },
        CellValueRef::Error { v } => CellValue::Error { v: Arc::from(v) },
        CellValueRef::Date { iso, serial } => CellValue::Date { iso, serial },
    }
}

fn is_date_style(style_idx: Option<u32>, styles: &Styles) -> bool {
    let idx = style_idx.unwrap_or(0) as usize;
    styles.is_date_style.get(idx).copied().unwrap_or(false)
}

fn excel_serial_to_iso(serial: f64, date_1904: bool) -> Option<String> {
    if serial.is_nan() || !serial.is_finite() {
        return None;
    }
    let mut days = serial.floor() as i64;
    let fraction = serial - serial.floor();
    let mut seconds = (fraction * 86_400.0).round() as i64;
    // A time within half a second of midnight rounds up into the next day.
    if seconds >= 86_400 {
        seconds -= 86_400;
        days = days.checked_add(1)?;
    }

    let base_date = if date_1904 {
        NaiveDate::from_ymd_opt(1904, 1, 1)?
    } else {
        // Excel 1900 date system: day 1 == 1900-01-01 with the 1900-02-29 bug.
        NaiveDate::from_ymd_opt(1899, 12, 31)?
    };

    if !date_1904 && days >= 60 {
        days -= 1; // fix Excel's fake 1900-02-29
    }

    // `try_days`: a serial far outside the calendar is not a date, and the
    // infallible constructor panics on it.
    let date = base_date.checked_add_signed(chrono::Duration::try_days(days)?)?;
    let time = NaiveTime::from_num_seconds_from_midnight_opt(u32::try_from(seconds).ok()?, 0)?;
    Some(format_iso_seconds(date, time))
}

/// Formats a date and time as `%Y-%m-%dT%H:%M:%S`.
///
/// The digits are written directly for the years a four-digit field holds,
/// which is every date a workbook normally carries; the general formatter
/// costs several times the rest of a date cell's decoding. A year outside
/// `0..=9999` is signed and widened by that formatter, so it is left to it.
fn format_iso_seconds(date: NaiveDate, time: NaiveTime) -> String {
    let year = match u32::try_from(date.year()) {
        Ok(year) if year <= 9999 => year,
        _ => {
            return NaiveDateTime::new(date, time)
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string()
        }
    };
    let mut out = String::with_capacity(19);
    push_decimal(&mut out, year, 4);
    out.push('-');
    push_decimal(&mut out, date.month(), 2);
    out.push('-');
    push_decimal(&mut out, date.day(), 2);
    out.push('T');
    push_decimal(&mut out, time.hour(), 2);
    out.push(':');
    push_decimal(&mut out, time.minute(), 2);
    out.push(':');
    push_decimal(&mut out, time.second(), 2);
    out
}

/// Appends the last `width` decimal digits of `value`, zero-padded.
fn push_decimal(out: &mut String, value: u32, width: u32) {
    for place in (0..width).rev() {
        let digit = (value / 10u32.pow(place)) % 10;
        out.push(char::from_digit(digit, 10).unwrap_or('0'));
    }
}

fn parse_cell_ref(r: &str) -> Option<(u32, u32)> {
    parse_cell_ref_bytes(r.as_bytes())
}

#[inline]
pub(crate) fn parse_cell_ref_bytes(bytes: &[u8]) -> Option<(u32, u32)> {
    let digit_idx = find_first_digit(bytes);

    let col_end = digit_idx.unwrap_or(bytes.len());
    let col = parse_col_bytes(&bytes[..col_end])?;

    let row = if let Some(start) = digit_idx {
        parse_row_bytes(&bytes[start..])?
    } else {
        0
    };

    if (1..=1_048_576).contains(&row) && (1..=16_384).contains(&col) {
        Some((row, col))
    } else {
        None
    }
}

#[inline]
fn parse_col_bytes(bytes: &[u8]) -> Option<u32> {
    let mut col = 0u32;
    let mut saw_alpha = false;
    for &b in bytes {
        if b == b'$' {
            continue;
        }
        let v = alpha_index(b)?;
        col = col.checked_mul(26)?.checked_add(v)?;
        saw_alpha = true;
    }
    saw_alpha.then_some(col)
}

#[inline]
fn parse_row_bytes(bytes: &[u8]) -> Option<u32> {
    let mut row = 0u32;
    let mut saw_digit = false;
    for &b in bytes {
        if b == b'$' {
            continue;
        }
        if b.is_ascii_digit() {
            row = row.checked_mul(10)?.checked_add((b - b'0') as u32)?;
            saw_digit = true;
        } else {
            return None;
        }
    }
    saw_digit.then_some(row)
}

#[inline]
fn alpha_index(b: u8) -> Option<u32> {
    let upper = b & 0xDF;
    if upper.is_ascii_uppercase() {
        Some((upper - b'A' + 1) as u32)
    } else {
        None
    }
}

/// Returns the index of the first ASCII digit (`0`-`9`) in `bytes`.
///
/// This is the split point of a cell reference: in `AB12` the column letters
/// end and the row number starts at index 2. It is the one copy of the
/// vectorized scan that both the sheet parser and the Flight server's range
/// queries use. Inputs of 16 bytes or more are scanned 16 bytes at a time
/// with NEON on aarch64 and SSE2 on x86 (32 at a time with AVX2 when the CPU
/// has it); shorter inputs, and the tail of longer ones, are scanned byte by
/// byte. The result is the same as
/// `bytes.iter().position(u8::is_ascii_digit)` for every input.
///
/// Returns `None` when `bytes` holds no ASCII digit.
///
/// # Examples
///
/// ```
/// use xlsx_fast_core::find_first_digit;
///
/// assert_eq!(find_first_digit(b"AB12"), Some(2));
/// assert_eq!(find_first_digit(b"$A$1"), Some(3));
/// assert_eq!(find_first_digit(b"ABC"), None);
/// ```
#[inline]
pub fn find_first_digit(bytes: &[u8]) -> Option<usize> {
    #[cfg(target_arch = "aarch64")]
    {
        if bytes.len() >= 16 {
            // SAFETY: `find_first_digit_neon` needs a CPU with NEON, and NEON
            // is part of the aarch64 baseline this block is compiled for. It
            // accepts any slice.
            unsafe {
                if let Some(idx) = find_first_digit_neon(bytes) {
                    return Some(idx);
                }
            }
        }
    }
    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    {
        if bytes.len() >= 32 && std::is_x86_feature_detected!("avx2") {
            // SAFETY: `find_first_digit_avx2` needs a CPU with AVX2, which
            // `is_x86_feature_detected!("avx2")` has just confirmed at run
            // time. It accepts any slice.
            unsafe {
                if let Some(idx) = find_first_digit_avx2(bytes) {
                    return Some(idx);
                }
            }
        }
        if bytes.len() >= 16
            && (cfg!(target_arch = "x86_64") || std::is_x86_feature_detected!("sse2"))
        {
            // SAFETY: `find_first_digit_sse2` needs a CPU with SSE2. SSE2 is
            // part of the x86_64 baseline, and on 32-bit x86 the condition
            // above has detected it at run time. It accepts any slice.
            unsafe {
                if let Some(idx) = find_first_digit_sse2(bytes) {
                    return Some(idx);
                }
            }
        }
    }

    for (idx, &b) in bytes.iter().enumerate() {
        if b.is_ascii_digit() {
            return Some(idx);
        }
    }
    None
}

/// `find_first_digit` for aarch64, 16 bytes at a time with NEON.
///
/// # Safety
///
/// The CPU must support NEON. Every aarch64 CPU Rust targets does; the
/// function is `unsafe` only because `#[target_feature]` makes it so. Any
/// `bytes` is accepted: no read leaves the slice.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn find_first_digit_neon(bytes: &[u8]) -> Option<usize> {
    use std::arch::aarch64 as arch;

    let mut i = 0usize;
    let len = bytes.len();
    let ptr = bytes.as_ptr();
    let lower = arch::vdupq_n_u8(b'0');
    let upper = arch::vdupq_n_u8(b'9');

    while i + 16 <= len {
        // SAFETY: the loop condition `i + 16 <= len` keeps the 16 bytes
        // `ptr + i .. ptr + i + 16` inside `bytes`, so both the offset and the
        // unaligned 16-byte load stay in bounds. `vld1q_u8` has no alignment
        // requirement.
        let chunk = unsafe { arch::vld1q_u8(ptr.add(i)) };
        let ge0 = arch::vcgeq_u8(chunk, lower);
        let le9 = arch::vcleq_u8(chunk, upper);
        let mask = arch::vandq_u8(ge0, le9);
        if arch::vmaxvq_u8(mask) != 0 {
            for j in 0..16 {
                // SAFETY: `j < 16` and `i + 16 <= len` from the loop
                // condition, so `i + j < len` is inside `bytes`.
                let b = unsafe { *ptr.add(i + j) };
                if b.is_ascii_digit() {
                    return Some(i + j);
                }
            }
        }
        i += 16;
    }

    while i < len {
        // SAFETY: the loop condition `i < len` keeps `ptr + i` inside `bytes`.
        let b = unsafe { *ptr.add(i) };
        if b.is_ascii_digit() {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// `find_first_digit` for x86, 16 bytes at a time with SSE2.
///
/// # Safety
///
/// The CPU must support SSE2. SSE2 is part of the x86_64 baseline; on 32-bit x86 the caller
/// must detect it. Any `bytes` is accepted: no read leaves the
/// slice.
#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
#[target_feature(enable = "sse2")]
unsafe fn find_first_digit_sse2(bytes: &[u8]) -> Option<usize> {
    #[cfg(target_arch = "x86")]
    use std::arch::x86 as arch;
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64 as arch;

    let mut i = 0usize;
    let len = bytes.len();
    let ptr = bytes.as_ptr();
    let lower = arch::_mm_set1_epi8((b'0' - 1) as i8);
    let upper = arch::_mm_set1_epi8((b'9' + 1) as i8);

    while i + 16 <= len {
        // SAFETY: the loop condition `i + 16 <= len` keeps the 16 bytes
        // `ptr + i .. ptr + i + 16` inside `bytes`, so both the offset and
        // the load stay in bounds. `_mm_loadu_si128` is the unaligned load, so
        // the cast pointer needs no alignment.
        let chunk = unsafe { arch::_mm_loadu_si128(ptr.add(i) as *const arch::__m128i) };
        let ge0 = arch::_mm_cmpgt_epi8(chunk, lower);
        let le9 = arch::_mm_cmplt_epi8(chunk, upper);
        let mask = arch::_mm_and_si128(ge0, le9);
        let bits = arch::_mm_movemask_epi8(mask) as u32;
        if bits != 0 {
            let offset = bits.trailing_zeros() as usize;
            return Some(i + offset);
        }
        i += 16;
    }

    while i < len {
        // SAFETY: the loop condition `i < len` keeps `ptr + i` inside `bytes`.
        let b = unsafe { *ptr.add(i) };
        if b.is_ascii_digit() {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// `find_first_digit` for x86, 32 bytes at a time with AVX2.
///
/// # Safety
///
/// The CPU must support AVX2, which the caller must detect at run
/// time (`is_x86_feature_detected!("avx2")`). Any `bytes` is accepted: no read leaves the
/// slice.
#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
#[target_feature(enable = "avx2")]
unsafe fn find_first_digit_avx2(bytes: &[u8]) -> Option<usize> {
    #[cfg(target_arch = "x86")]
    use std::arch::x86 as arch;
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64 as arch;

    let mut i = 0usize;
    let len = bytes.len();
    let ptr = bytes.as_ptr();
    let lower = arch::_mm256_set1_epi8((b'0' - 1) as i8);
    let upper = arch::_mm256_set1_epi8((b'9' + 1) as i8);

    while i + 32 <= len {
        // SAFETY: the loop condition `i + 32 <= len` keeps the 32 bytes
        // `ptr + i .. ptr + i + 32` inside `bytes`, so both the offset and
        // the load stay in bounds. `_mm256_loadu_si256` is the unaligned load, so
        // the cast pointer needs no alignment.
        let chunk = unsafe { arch::_mm256_loadu_si256(ptr.add(i) as *const arch::__m256i) };
        let ge0 = arch::_mm256_cmpgt_epi8(chunk, lower);
        let le9 = arch::_mm256_cmpgt_epi8(upper, chunk);
        let mask = arch::_mm256_and_si256(ge0, le9);
        let bits = arch::_mm256_movemask_epi8(mask) as u32;
        if bits != 0 {
            let offset = bits.trailing_zeros() as usize;
            return Some(i + offset);
        }
        i += 32;
    }

    while i < len {
        // SAFETY: the loop condition `i < len` keeps `ptr + i` inside `bytes`.
        let b = unsafe { *ptr.add(i) };
        if b.is_ascii_digit() {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{find_first_digit, format_has_date_tokens, parse_cell_ref_bytes};

    /// The hand-written date formatter must agree with chrono's for every
    /// year, including the ones it hands back to chrono.
    #[test]
    fn format_iso_seconds_matches_the_general_formatter() {
        use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
        let times = [(0, 0, 0), (0, 0, 9), (9, 5, 59), (12, 30, 0), (23, 59, 59)];
        let mut checked = 0;
        for year in (-3..=3)
            .chain(95..=105)
            .chain(995..=1005)
            .chain(1899..=2105)
            .chain(9995..=10_003)
        {
            for (month, day) in [(1, 1), (2, 28), (2, 29), (7, 4), (10, 9), (12, 31)] {
                let Some(date) = NaiveDate::from_ymd_opt(year, month, day) else {
                    continue;
                };
                for (hour, minute, second) in times {
                    let time = NaiveTime::from_hms_opt(hour, minute, second).unwrap();
                    assert_eq!(
                        super::format_iso_seconds(date, time),
                        NaiveDateTime::new(date, time)
                            .format("%Y-%m-%dT%H:%M:%S")
                            .to_string()
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 5_000);
    }
    use ooxml_archive::{parse_root_rels, resolve_part};

    /// The vectorized scan must agree with a byte-by-byte scan whatever the
    /// length, wherever the digit is, and whatever surrounds it. The filler
    /// bytes include the neighbours of `0`-`9` in ASCII (`/` and `:`) and the
    /// bytes that differ from a digit only in the high bit.
    #[test]
    fn find_first_digit_matches_a_byte_by_byte_scan() {
        let fillers = [b'A', b'$', b'/', b':', 0x00, 0x7F, 0xB0, 0xB9, 0xFF];
        let mut checked = 0u32;
        for len in 0..=80usize {
            for &filler in &fillers {
                let mut bytes = vec![filler; len];
                assert_eq!(
                    find_first_digit(&bytes),
                    None,
                    "len {len} filler {filler:#x}"
                );
                for position in 0..len {
                    for digit in *b"059" {
                        bytes[position] = digit;
                        assert_eq!(
                            find_first_digit(&bytes),
                            bytes.iter().position(u8::is_ascii_digit),
                            "len {len} filler {filler:#x} digit at {position}"
                        );
                        // A later digit never hides an earlier one.
                        let saved = bytes[len - 1];
                        bytes[len - 1] = b'7';
                        assert_eq!(
                            find_first_digit(&bytes),
                            bytes.iter().position(u8::is_ascii_digit)
                        );
                        bytes[len - 1] = saved;
                        bytes[position] = filler;
                        checked += 1;
                    }
                }
            }
        }
        assert!(checked > 80_000, "checked {checked} inputs");
    }

    /// Calls each vectorized scan directly. `find_first_digit` falls back to
    /// a scalar scan when the vector function returns `None`, so a vector
    /// path that missed a digit would be invisible through it. Small enough
    /// for Miri: `cargo +nightly miri test -p xlsx-fast-core --lib vector_digit_scans`.
    #[test]
    fn vector_digit_scans_find_the_digit_at_every_position() {
        type Scan = (&'static str, unsafe fn(&[u8]) -> Option<usize>);
        let mut scans: Vec<Scan> = Vec::new();
        #[cfg(target_arch = "aarch64")]
        scans.push(("neon", super::find_first_digit_neon));
        #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
        {
            // The same conditions under which `find_first_digit` calls them.
            if cfg!(target_arch = "x86_64") || std::is_x86_feature_detected!("sse2") {
                scans.push(("sse2", super::find_first_digit_sse2));
            }
            if std::is_x86_feature_detected!("avx2") {
                scans.push(("avx2", super::find_first_digit_avx2));
            }
        }
        for (name, scan) in scans {
            // `/` and `:` are the neighbours of the digits in ASCII; 0xB5 is
            // a digit with the high bit set.
            for filler in [b'/', b':', 0xB5] {
                for len in 0..=70usize {
                    let mut bytes = vec![filler; len];
                    // SAFETY: each scan needs only its CPU feature, and
                    // `scans` holds a function only when that feature is
                    // present (NEON is baseline on aarch64; SSE2 and AVX2 were
                    // detected above). Every scan accepts any slice.
                    assert_eq!(unsafe { scan(&bytes) }, None, "{name}: len {len}");
                    for position in 0..len {
                        bytes[position] = b"059"[position % 3];
                        // SAFETY: as above: the feature is present and any
                        // slice is accepted.
                        let found = unsafe { scan(&bytes) };
                        assert_eq!(found, Some(position), "{name}: len {len}");
                        bytes[position] = filler;
                    }
                }
            }
        }
    }

    #[test]
    fn parse_cell_ref_basic() {
        assert_eq!(parse_cell_ref_bytes(b"A1"), Some((1, 1)));
        assert_eq!(parse_cell_ref_bytes(b"Z99"), Some((99, 26)));
        assert_eq!(parse_cell_ref_bytes(b"AA10"), Some((10, 27)));
        assert_eq!(
            parse_cell_ref_bytes(b"XFD1048576"),
            Some((1_048_576, 16_384))
        );
    }

    #[test]
    fn parse_cell_ref_dollar() {
        assert_eq!(parse_cell_ref_bytes(b"$A$1"), Some((1, 1)));
        assert_eq!(parse_cell_ref_bytes(b"A$1"), Some((1, 1)));
        assert_eq!(parse_cell_ref_bytes(b"$AA20"), Some((20, 27)));
    }

    #[test]
    fn parse_cell_ref_rejects_coordinates_outside_excel_limits() {
        assert_eq!(parse_cell_ref_bytes(b"XFE1"), None);
        assert_eq!(parse_cell_ref_bytes(b"A1048577"), None);
        assert_eq!(parse_cell_ref_bytes(b"ZZZZZZZZZZ1"), None);
    }

    #[test]
    fn parse_root_rels_finds_workbook_part() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship
    Id="rId1"
    Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument"
    Target="/xl/workbook.xml"/>
</Relationships>"#;

        assert_eq!(parse_root_rels(xml).as_deref(), Some("xl/workbook.xml"));
    }

    #[test]
    fn resolve_part_handles_parent_segments() {
        assert_eq!(
            resolve_part("xl/worksheets", "../sharedStrings.xml"),
            "xl/sharedStrings.xml"
        );
    }

    #[test]
    fn format_has_date_tokens_ignores_literals() {
        assert!(!format_has_date_tokens("\"day\" 0.00"));
        assert!(format_has_date_tokens("yyyy-mm-dd"));
    }
}
