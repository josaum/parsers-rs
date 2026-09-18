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

use anyhow::{anyhow, Context, Result};
use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
#[cfg(feature = "arrow")]
use ooxml_archive::SharedInflateBudget;
use ooxml_archive::{
    parse_relationships, part_dir, rels_path_for_part, resolve_part, OoxmlArchive, PartReader,
};
use quick_xml::events::Event;
use quick_xml::Reader;
use serde::Serialize;
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
/// Arrow RecordBatch streaming output (optional feature).
///
/// Exports spreadsheet data as Arrow Record Batch files, suitable for
/// columnar ETL pipelines and analytics systems. Used by [`xlsx-fast-flight`]
/// (flight server), [`xlsx-fast-py`] (Python bindings), and integration
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
                parse_shared_strings_reader(reader)?
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
            let xml_reader = Reader::from_reader(reader);
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
            parse_shared_strings_reader(reader)?
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

    for sheet in workbook_info.sheets {
        let target = workbook_rels
            .by_id
            .get(&sheet.rel_id)
            .ok_or_else(|| anyhow!("missing relationship for sheet {}", sheet.name))?;
        let sheet_path = resolve_part(&workbook_dir, target);
        let reader = open_part_reader(&archive, &sheet_path, options.max_inflate)
            .with_context(|| format!("read sheet: {sheet_path}"))?;
        let mut cells = Vec::new();
        let xml_reader = Reader::from_reader(reader);
        parse_sheet_reader_with(
            xml_reader,
            &shared_strings,
            &styles,
            workbook_info.date_1904,
            &options,
            None,
            |parts| {
                cells.push(Cell {
                    row: parts.row,
                    col: parts.col,
                    formula: parts.formula.map(str::to_string),
                    value: cell_value_ref_to_owned(parts.value),
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
            parse_shared_strings_reader(reader)?
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
        let xml_reader = Reader::from_reader(reader);
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
    let mut reader = Reader::from_reader(Cursor::new(xml));
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut info = WorkbookInfo::default();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let qname = e.name();
                let name = local_name(qname.as_ref());
                if name == b"workbookPr" {
                    for attr in e.attributes().flatten() {
                        if local_name(attr.key.as_ref()) == b"date1904" {
                            let value = attr
                                .decode_and_unescape_value(reader.decoder())?
                                .to_string();
                            info.date_1904 = value == "1" || value.eq_ignore_ascii_case("true");
                        }
                    }
                } else if name == b"sheet" {
                    let mut sheet = SheetInfo::default();
                    for attr in e.attributes().flatten() {
                        let key_raw = attr.key;
                        let key = local_name(key_raw.as_ref());
                        if key == b"name" {
                            sheet.name = attr
                                .decode_and_unescape_value(reader.decoder())?
                                .to_string();
                        } else if key == b"sheetId" {
                            let value = attr.decode_and_unescape_value(reader.decoder())?;
                            sheet.sheet_id =
                                lexical_core::parse::<u32>(value.as_bytes()).unwrap_or(0);
                        } else if key == b"r:id" || key == b"id" {
                            sheet.rel_id = attr
                                .decode_and_unescape_value(reader.decoder())?
                                .to_string();
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

fn parse_shared_strings_reader<R: BufRead>(reader: R) -> Result<StringTable> {
    let mut reader = Reader::from_reader(reader);
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut strings = StringTable::default();

    let mut in_si = false;
    let mut in_t = false;
    let mut current = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let qname = e.name();
                let name = local_name(qname.as_ref());

                if name == b"si" {
                    in_si = true;
                    current.clear();
                } else if in_si && name == b"t" {
                    in_t = true;
                }
            }
            Ok(Event::Text(e)) => {
                if in_si && in_t {
                    let text = e.unescape()?;
                    current.push_str(text.as_ref());
                }
            }
            Ok(Event::End(e)) => {
                let qname = e.name();
                let name = local_name(qname.as_ref());

                if name == b"t" {
                    in_t = false;
                } else if name == b"si" {
                    in_si = false;
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
    let mut reader = Reader::from_reader(Cursor::new(xml));
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();

    let mut num_fmts: HashMap<u32, String> = HashMap::new();
    let mut is_date_style = Vec::new();
    let mut in_cell_xfs = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let qname = e.name();
                let name = local_name(qname.as_ref());

                if name == b"numFmt" {
                    let mut id = None;
                    let mut code = None;
                    for attr in e.attributes().flatten() {
                        let key_raw = attr.key;
                        let key = local_name(key_raw.as_ref());
                        if key == b"numFmtId" {
                            let value = attr.decode_and_unescape_value(reader.decoder())?;
                            id = lexical_core::parse::<u32>(value.as_bytes()).ok();
                        } else if key == b"formatCode" {
                            code = Some(
                                attr.decode_and_unescape_value(reader.decoder())?
                                    .to_string(),
                            );
                        }
                    }
                    if let (Some(id), Some(code)) = (id, code) {
                        num_fmts.insert(id, code);
                    }
                } else if name == b"cellXfs" {
                    in_cell_xfs = true;
                } else if in_cell_xfs && name == b"xf" {
                    let mut num_fmt_id = None;
                    for attr in e.attributes().flatten() {
                        if local_name(attr.key.as_ref()) == b"numFmtId" {
                            let value = attr.decode_and_unescape_value(reader.decoder())?;
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
                if local_name(e.name().as_ref()) == b"cellXfs" {
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
    mut reader: Reader<R>,
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
    reader.config_mut().trim_text(false);
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
    let mut inline_buf = String::new();
    let mut skip_cell = false;
    let mut skip_row_depth: u32 = 0;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let qname = e.name();
                let name = local_name(qname.as_ref());

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
                        current_row = 0;
                        col_cursor = 0;
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == b"r" {
                                let value = attr.decode_and_unescape_value(reader.decoder())?;
                                current_row =
                                    lexical_core::parse::<u32>(value.as_bytes()).unwrap_or(0);
                            }
                        }
                        if !(1..=1_048_576).contains(&current_row) {
                            row_outside = true;
                            skip_row_depth = 1;
                        } else if has_ranges {
                            row_outside = current_row < row_min || current_row > row_max;
                        } else {
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
                        inline_buf.clear();
                        let mut cell_ref_row: u32 = 0;
                        let mut cell_ref_col: u32 = 0;
                        let mut has_cell_ref = false;
                        let mut invalid_cell_ref = false;
                        for attr in e.attributes().flatten() {
                            let key_raw = attr.key;
                            let key = local_name(key_raw.as_ref());
                            if key == b"r" {
                                let value = attr.decode_and_unescape_value(reader.decoder())?;
                                if let Some((row, col)) = parse_cell_ref(value.as_ref()) {
                                    cell_ref_row = row;
                                    cell_ref_col = col;
                                    has_cell_ref = true;
                                } else {
                                    invalid_cell_ref = true;
                                }
                            } else if key == b"t" {
                                let value = attr.decode_and_unescape_value(reader.decoder())?;
                                cell_type = match value.as_ref() {
                                    "s" => CellType::SharedString,
                                    "b" => CellType::Bool,
                                    "e" => CellType::Error,
                                    "str" => CellType::Str,
                                    "inlineStr" => CellType::InlineStr,
                                    _ => CellType::Number,
                                };
                            } else if key == b"s" {
                                let value = attr.decode_and_unescape_value(reader.decoder())?;
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
                                col_cursor + 1
                            };
                        } else {
                            cell_row = current_row;
                            cell_col = col_cursor + 1;
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
                    }
                    b"t" if in_inline_str => {
                        in_inline_t = true;
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(_)) => {
                if skip_row_depth > 0 {
                    buf.clear();
                    continue;
                }
            }
            Ok(Event::Text(e)) => {
                if skip_row_depth > 0 || skip_cell {
                    buf.clear();
                    continue;
                }
                let text = e.unescape()?;
                if in_value {
                    cell_value.push_str(text.as_ref());
                    has_value = true;
                } else if in_formula {
                    if options.include_formulas {
                        cell_formula.push_str(text.as_ref());
                        has_formula = true;
                    }
                } else if in_inline_t {
                    inline_buf.push_str(text.as_ref());
                }
            }
            Ok(Event::End(e)) => {
                let qname = e.name();
                let name = local_name(qname.as_ref());

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
                    b"t" => in_inline_t = false,
                    b"is" => in_inline_str = false,
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
            if let Some(v) = raw_value.and_then(|v| lexical_core::parse::<usize>(v.as_bytes()).ok())
            {
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
            if let Some(v) = raw_value.and_then(|v| lexical_core::parse::<usize>(v.as_bytes()).ok())
            {
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

    let base_date = if date_1904 {
        NaiveDate::from_ymd_opt(1904, 1, 1)?
    } else {
        // Excel 1900 date system: day 1 == 1900-01-01 with the 1900-02-29 bug.
        NaiveDate::from_ymd_opt(1899, 12, 31)?
    };

    if !date_1904 && days >= 60 {
        days -= 1; // fix Excel's fake 1900-02-29
    }

    let date = base_date.checked_add_signed(chrono::Duration::days(days))?;
    let seconds = (fraction * 86_400.0).round() as i64;
    let time =
        NaiveTime::from_num_seconds_from_midnight_opt((seconds.rem_euclid(86_400)) as u32, 0)?;
    let dt = NaiveDateTime::new(date, time);
    Some(dt.format("%Y-%m-%dT%H:%M:%S").to_string())
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

#[inline]
fn find_first_digit(bytes: &[u8]) -> Option<usize> {
    #[cfg(target_arch = "aarch64")]
    {
        if bytes.len() >= 16 {
            // SAFETY: NEON is baseline on aarch64.
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
            // SAFETY: we gate on AVX2 support.
            unsafe {
                if let Some(idx) = find_first_digit_avx2(bytes) {
                    return Some(idx);
                }
            }
        }
        if bytes.len() >= 16
            && (cfg!(target_arch = "x86_64") || std::is_x86_feature_detected!("sse2"))
        {
            // SAFETY: we gate on SSE2 support.
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
        let chunk = arch::vld1q_u8(ptr.add(i));
        let ge0 = arch::vcgeq_u8(chunk, lower);
        let le9 = arch::vcleq_u8(chunk, upper);
        let mask = arch::vandq_u8(ge0, le9);
        if arch::vmaxvq_u8(mask) != 0 {
            for j in 0..16 {
                let b = *ptr.add(i + j);
                if b.is_ascii_digit() {
                    return Some(i + j);
                }
            }
        }
        i += 16;
    }

    while i < len {
        let b = *ptr.add(i);
        if b.is_ascii_digit() {
            return Some(i);
        }
        i += 1;
    }
    None
}

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
        let chunk = arch::_mm_loadu_si128(ptr.add(i) as *const arch::__m128i);
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
        let b = *ptr.add(i);
        if b.is_ascii_digit() {
            return Some(i);
        }
        i += 1;
    }
    None
}

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
        let chunk = arch::_mm256_loadu_si256(ptr.add(i) as *const arch::__m256i);
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
        let b = *ptr.add(i);
        if b.is_ascii_digit() {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{format_has_date_tokens, parse_cell_ref_bytes};
    use ooxml_archive::{parse_root_rels, resolve_part};

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
