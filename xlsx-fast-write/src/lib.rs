//! # xlsx-fast-write
//!
//! Sovereign XLSX writer — generates `.xlsx` files without external writer
//! dependencies (no `rust_xlsxwriter`, no `openpyxl`). Output is a valid
//! OOXML package written directly to a ZIP archive.
//!
//! ```rust,no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use xlsx_fast_write::{Workbook, CellValue, Format};
//!
//! let mut wb = Workbook::new();
//! let header = Format::new().bold().background_color(0x4472C4).font_color(0xFFFFFF);
//! let sheet = wb.add_sheet("Summary");
//! sheet.set_column_width(0, 12.0);
//! sheet.write_with_format(0, 0, "Page", &header);
//! sheet.write(1, 0, CellValue::Number(1.0));
//! wb.save("output.xlsx")?;
//! # Ok(())
//! # }
//! ```
//!
//! ## What the writer does with input Excel cannot hold
//!
//! Every package this crate returns opens in Excel. Input that cannot be
//! written faithfully is either made valid in a documented way or refused:
//!
//! - **Sheet names** are made valid by [`Workbook::add_sheet`] (31 characters,
//!   no `[ ] : * ? / \`, unique ignoring case); [`Sheet::name`] returns the
//!   name actually used.
//! - **A workbook with no sheet** is written with one empty sheet.
//! - **A cell outside the grid** (1,048,576 rows, 16,384 columns), **text
//!   over 32,767 characters** and **a formula over 8,192** make
//!   [`Workbook::write_to`] return an error before anything is written.
//! - **`NaN` and infinite numbers** are written as the error value `#NUM!`.
//! - **Column widths** that are not finite are left out; others are brought
//!   into 0 to 255.
//! - **Dates** are written with [`Sheet::write_date`] as a serial number with
//!   a date format.
//!
//! ## Formula injection
//!
//! Text is never turned into a formula. [`Sheet::write`] and
//! [`Sheet::write_with_format`] write every string as an inline text cell,
//! including one that starts with `=`, `+`, `-` or `@`, so text from an
//! untrusted source cannot run as a formula when the workbook is opened. The
//! only ways to create a formula are [`Sheet::write_formula`] and the JSON
//! spec cell type `"f"`; do not pass untrusted text to either. The text is
//! stored unchanged (no leading apostrophe is added), so a program that later
//! exports these cells to CSV must do its own escaping.

#![deny(missing_docs)]

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::io::{BufWriter, Write};
use std::path::Path;
use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

/// A cell value written into a worksheet.
#[derive(Debug, Clone)]
pub enum CellValue {
    /// Inline string (UTF-8).
    String(String),
    /// IEEE-754 number (Excel general numeric).
    Number(f64),
    /// Boolean cell.
    Bool(bool),
    /// Explicit empty (skipped on write).
    Empty,
}

impl From<&str> for CellValue {
    fn from(s: &str) -> Self {
        CellValue::String(s.into())
    }
}
impl From<String> for CellValue {
    fn from(s: String) -> Self {
        CellValue::String(s)
    }
}
impl From<f64> for CellValue {
    fn from(v: f64) -> Self {
        CellValue::Number(v)
    }
}
impl From<i32> for CellValue {
    fn from(v: i32) -> Self {
        CellValue::Number(v as f64)
    }
}
impl From<usize> for CellValue {
    fn from(v: usize) -> Self {
        CellValue::Number(v as f64)
    }
}
impl From<bool> for CellValue {
    fn from(v: bool) -> Self {
        CellValue::Bool(v)
    }
}

/// Cell format (builder pattern).
#[derive(Debug, Clone, Default)]
pub struct Format {
    bold: bool,
    background_color: Option<u32>,
    font_color: Option<u32>,
    wrap: bool,
    zebra: bool,
}

impl Format {
    /// Create a default (unstyled) format.
    pub fn new() -> Self {
        Self::default()
    }
    /// Enable bold text.
    pub fn bold(mut self) -> Self {
        self.bold = true;
        self
    }
    /// Set solid background fill as `0xRRGGBB`.
    pub fn background_color(mut self, rgb: u32) -> Self {
        self.background_color = Some(rgb);
        self
    }
    /// Set font color as `0xRRGGBB`.
    pub fn font_color(mut self, rgb: u32) -> Self {
        self.font_color = Some(rgb);
        self
    }
    /// Wrap long text and top-align it.
    pub fn wrap(mut self) -> Self {
        self.wrap = true;
        self
    }
    /// Shade the cell with the light "zebra" fill.
    pub fn zebra(mut self) -> Self {
        self.zebra = true;
        self
    }

    /// Map this format onto the fixed `styles.xml` cellXfs table.
    ///
    /// 0 = default, 1 = bold, 2 = header (bold + fix fill + white font),
    /// 3 = wrap, 4 = zebra, 5 = wrap + zebra.
    fn style_index(&self) -> u8 {
        if self.bold && self.background_color.is_some() && self.font_color.is_some() {
            2
        } else if self.bold {
            1
        } else if self.wrap && self.zebra {
            5
        } else if self.wrap {
            3
        } else if self.zebra {
            4
        } else {
            0
        }
    }
}

/// Whether a stored string is cell text or a formula.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CellKind {
    /// The value is written as it is.
    Plain,
    /// The string value is a formula, written in an `<f>` element.
    Formula,
}

/// Style index of a cell shown as a date (built-in number format 14).
const STYLE_DATE: u8 = 6;
/// Style index of a cell shown as a date and time (built-in number format 22).
const STYLE_DATE_TIME: u8 = 7;

/// A worksheet.
pub struct Sheet {
    name: String,
    /// Cells keyed by zero-based `(row, col)`. Style index is `None` (default),
    /// `Some(1)` (bold), or `Some(2)` (header: bold + fill + white font).
    cells: BTreeMap<(u32, u16), (CellValue, Option<u8>, CellKind)>,
    column_widths: BTreeMap<u16, f64>,
    freeze_rows: u32,
}

impl Sheet {
    /// Write a cell at zero-based `(row, col)` without a named format.
    pub fn write(&mut self, row: u32, col: u16, value: impl Into<CellValue>) {
        let v = value.into();
        if !matches!(v, CellValue::Empty) {
            self.cells.insert((row, col), (v, None, CellKind::Plain));
        }
    }
    /// Write a cell with a format marker (bold / header styles in the styles.xml table).
    pub fn write_with_format(
        &mut self,
        row: u32,
        col: u16,
        value: impl Into<CellValue>,
        fmt: &Format,
    ) {
        let v = value.into();
        if !matches!(v, CellValue::Empty) {
            self.cells
                .insert((row, col), (v, Some(fmt.style_index()), CellKind::Plain));
        }
    }
    /// Write a formula at zero-based `(row, col)`.
    ///
    /// This is the only way to create a formula: [`Sheet::write`] and
    /// [`Sheet::write_with_format`] always write text, whatever it starts
    /// with. One leading `=` is accepted and removed. Never pass text from an
    /// untrusted source here. An empty formula writes nothing.
    pub fn write_formula(&mut self, row: u32, col: u16, formula: impl Into<String>) {
        let formula: String = formula.into();
        let formula = formula.strip_prefix('=').unwrap_or(&formula);
        if !formula.is_empty() {
            self.cells.insert(
                (row, col),
                (
                    CellValue::String(formula.to_string()),
                    None,
                    CellKind::Formula,
                ),
            );
        }
    }
    /// Write a date, or a date and time, at zero-based `(row, col)`.
    ///
    /// `iso` is `YYYY-MM-DD`, optionally followed by `T` or a space and
    /// `HH:MM`, `HH:MM:SS` or `HH:MM:SS.fff`, and an optional `Z`. No time
    /// zone conversion is done. The cell holds Excel's serial number with a
    /// date format, so readers see a date.
    ///
    /// Returns `false` and writes nothing when `iso` is not such a date or
    /// lies outside 1900-03-01 to 9999-12-31, the range in which Excel's
    /// serial numbers are unambiguous.
    pub fn write_date(&mut self, row: u32, col: u16, iso: &str) -> bool {
        match parse_iso_date(iso) {
            Some(serial) => {
                self.write_date_serial(row, col, serial);
                true
            }
            None => false,
        }
    }
    /// Stores `serial` with the date style, or the date-time style when it
    /// has a time of day.
    fn write_date_serial(&mut self, row: u32, col: u16, serial: f64) {
        let style = if serial.fract() == 0.0 {
            STYLE_DATE
        } else {
            STYLE_DATE_TIME
        };
        self.cells.insert(
            (row, col),
            (CellValue::Number(serial), Some(style), CellKind::Plain),
        );
    }
    /// Set Excel column width (character units) for a zero-based column.
    pub fn set_column_width(&mut self, col: u16, width: f64) {
        self.column_widths.insert(col, width);
    }
    /// Freeze the top `rows` rows so the header stays visible while scrolling.
    pub fn set_freeze_rows(&mut self, rows: u32) {
        self.freeze_rows = rows;
    }
    /// Sheet tab name.
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// Rows in a worksheet: Excel's grid ends at row 1,048,576.
const MAX_ROWS: u32 = 1_048_576;
/// Columns in a worksheet: Excel's grid ends at column `XFD` (16,384).
const MAX_COLS: u16 = 16_384;
/// Most characters (UTF-16 code units) one cell's text may hold.
const MAX_CELL_CHARS: usize = 32_767;
/// Most characters (UTF-16 code units) a formula may hold.
const MAX_FORMULA_CHARS: usize = 8_192;
/// Most characters (UTF-16 code units) a sheet name may hold.
const MAX_SHEET_NAME_CHARS: usize = 31;
/// Widest column Excel accepts, in character units.
const MAX_COLUMN_WIDTH: f64 = 255.0;

/// A workbook.
pub struct Workbook {
    sheets: Vec<Sheet>,
    /// Lower-cased sheet names in use, each with the next numeric suffix to
    /// try when the same name is asked for again.
    taken_names: HashMap<String, u32>,
}

/// Cuts `name` to at most `limit` UTF-16 code units without splitting a
/// character.
fn truncate_utf16(name: &str, limit: usize) -> &str {
    let mut units = 0;
    for (index, ch) in name.char_indices() {
        units += ch.len_utf16();
        if units > limit {
            return &name[..index];
        }
    }
    name
}

/// Makes `name` one Excel accepts, apart from uniqueness.
///
/// `position` is the one-based position of the sheet, used to name a sheet
/// whose name is empty.
fn valid_sheet_name(name: &str, position: usize) -> String {
    let mut valid: String = truncate_utf16(name, MAX_SHEET_NAME_CHARS)
        .chars()
        .map(|ch| match ch {
            '[' | ']' | ':' | '*' | '?' | '/' | '\\' => '_',
            other => other,
        })
        .collect();
    if valid.starts_with('\'') {
        valid.replace_range(..1, "_");
    }
    if valid.ends_with('\'') {
        valid.replace_range(valid.len() - 1.., "_");
    }
    if valid.is_empty() {
        valid = format!("Sheet{position}");
    }
    if valid.eq_ignore_ascii_case("history") {
        valid.push('_');
    }
    valid
}

impl Workbook {
    /// Create an empty workbook.
    pub fn new() -> Self {
        Self {
            sheets: Vec::new(),
            taken_names: HashMap::new(),
        }
    }
    /// Append a worksheet and return a mutable handle to it.
    ///
    /// Excel refuses a workbook with a sheet name it does not accept, so the
    /// name is made valid here and [`Sheet::name`] returns the name actually
    /// used: it is cut to 31 characters; each of `[ ] : * ? / \` and an
    /// apostrophe at either end becomes `_`; an empty name becomes `SheetN`;
    /// the reserved name `History` gets a trailing `_`; and a name already in
    /// use (ignoring case) gets a suffix such as ` (2)`.
    pub fn add_sheet(&mut self, name: impl Into<String>) -> &mut Sheet {
        let name = self.unique_sheet_name(&name.into());
        self.sheets.push(Sheet {
            name,
            cells: BTreeMap::new(),
            column_widths: BTreeMap::new(),
            freeze_rows: 0,
        });
        // Just pushed — last_mut cannot fail; avoid unwrap in public API.
        match self.sheets.last_mut() {
            Some(sheet) => sheet,
            None => unreachable!("sheet just pushed"),
        }
    }

    /// Returns a valid sheet name no other sheet uses, and records it.
    fn unique_sheet_name(&mut self, requested: &str) -> String {
        let base = valid_sheet_name(requested, self.sheets.len() + 1);
        let mut name = base.clone();
        loop {
            if let std::collections::hash_map::Entry::Vacant(slot) =
                self.taken_names.entry(name.to_lowercase())
            {
                slot.insert(2);
                return name;
            }
            // The counter is kept per requested name, so asking for the same
            // name many times does not retry every earlier suffix.
            let counter = self.taken_names.entry(base.to_lowercase()).or_insert(2);
            let suffix = format!(" ({counter})");
            *counter = counter.saturating_add(1);
            let room = MAX_SHEET_NAME_CHARS.saturating_sub(suffix.len());
            name = format!("{}{suffix}", truncate_utf16(&base, room));
        }
    }

    /// Checks every cell against Excel's limits before anything is written.
    fn check_limits(sheets: &[Sheet]) -> Result<(), Box<dyn std::error::Error>> {
        for sheet in sheets {
            for (&(row, col), (value, _, kind)) in &sheet.cells {
                if row >= MAX_ROWS || col >= MAX_COLS {
                    return Err(format!(
                        "sheet {:?}: the cell at row {}, column {} is outside Excel's grid \
                         of {MAX_ROWS} rows and {MAX_COLS} columns",
                        sheet.name,
                        u64::from(row) + 1,
                        u32::from(col) + 1
                    )
                    .into());
                }
                let CellValue::String(text) = value else {
                    continue;
                };
                let (limit, what) = match kind {
                    CellKind::Plain => (MAX_CELL_CHARS, "text"),
                    CellKind::Formula => (MAX_FORMULA_CHARS, "formula"),
                };
                // UTF-8 never uses fewer bytes than UTF-16 uses code units, so
                // only a text longer than `limit` bytes needs counting.
                if text.len() > limit {
                    let chars = text.encode_utf16().count();
                    if chars > limit {
                        return Err(format!(
                            "sheet {:?}: the {what} in cell {}{} is {chars} characters long; \
                             Excel's limit is {limit}",
                            sheet.name,
                            col_letter(col),
                            row + 1
                        )
                        .into());
                    }
                }
            }
        }
        Ok(())
    }

    /// Borrow a sheet by zero-based index.
    pub fn sheet_mut(&mut self, index: usize) -> Option<&mut Sheet> {
        self.sheets.get_mut(index)
    }

    /// Number of sheets currently in the workbook.
    pub fn sheet_count(&self) -> usize {
        self.sheets.len()
    }
    /// Write the workbook to an `.xlsx` path.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), Box<dyn std::error::Error>> {
        let f = std::fs::File::create(path.as_ref())?;
        self.write_to(BufWriter::new(f))?;
        Ok(())
    }
    /// Write the workbook ZIP package to any seekable writer.
    ///
    /// # Errors
    ///
    /// Besides I/O errors, returns an error and writes nothing when a cell
    /// lies outside Excel's grid (1,048,576 rows, 16,384 columns), when a
    /// cell's text is longer than 32,767 characters or a formula longer than
    /// 8,192. Excel reports such a file as damaged or silently cuts the
    /// text, so the caller is told instead.
    ///
    /// A workbook with no sheet is written with one empty sheet, `Sheet1`,
    /// because Excel cannot open a workbook that has none.
    pub fn write_to<W: Write + std::io::Seek>(
        &self,
        writer: W,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let placeholder;
        let sheets: &[Sheet] = if self.sheets.is_empty() {
            placeholder = [Sheet {
                name: "Sheet1".to_string(),
                cells: BTreeMap::new(),
                column_widths: BTreeMap::new(),
                freeze_rows: 0,
            }];
            &placeholder
        } else {
            &self.sheets
        };
        Self::check_limits(sheets)?;
        let mut zip = ZipWriter::new(writer);
        let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        let nsheets = sheets.len();

        // [Content_Types].xml
        zip.start_file("[Content_Types].xml", opts)?;
        let mut ct = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\n<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\n<Default Extension=\"xml\" ContentType=\"application/xml\"/>\n<Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/>\n<Override PartName=\"/xl/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml\"/>\n");
        for i in 0..nsheets {
            ct.push_str(&format!("<Override PartName=\"/xl/worksheets/sheet{}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>\n", i + 1));
        }
        ct.push_str("<Override PartName=\"/docProps/core.xml\" ContentType=\"application/vnd.openxmlformats-package.core-properties+xml\"/>\n<Override PartName=\"/docProps/app.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.extended-properties+xml\"/>\n</Types>");
        zip.write_all(ct.as_bytes())?;

        // _rels/.rels
        zip.start_file("_rels/.rels", opts)?;
        zip.write_all(b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\n<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"xl/workbook.xml\"/>\n<Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties\" Target=\"docProps/core.xml\"/>\n<Relationship Id=\"rId3\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties\" Target=\"docProps/app.xml\"/>\n</Relationships>")?;

        // docProps/core.xml
        zip.start_file("docProps/core.xml", opts)?;
        zip.write_all(b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<cp:coreProperties xmlns:cp=\"http://schemas.openxmlformats.org/package/2006/metadata/core-properties\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\"><dc:creator>xlsx-fast-write</dc:creator></cp:coreProperties>")?;

        // docProps/app.xml
        zip.start_file("docProps/app.xml", opts)?;
        zip.write_all(b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Properties xmlns=\"http://schemas.openxmlformats.org/officeDocument/2006/extended-properties\"><Application>xlsx-fast-write</Application></Properties>")?;

        // xl/workbook.xml
        zip.start_file("xl/workbook.xml", opts)?;
        let mut wb_xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><sheets>");
        for (i, s) in sheets.iter().enumerate() {
            wb_xml.push_str(&format!(
                "<sheet name=\"{}\" sheetId=\"{}\" r:id=\"rId{}\"/>",
                xa(&s.name),
                i + 1,
                i + 1
            ));
        }
        wb_xml.push_str("</sheets></workbook>");
        zip.write_all(wb_xml.as_bytes())?;

        // xl/_rels/workbook.xml.rels
        zip.start_file("xl/_rels/workbook.xml.rels", opts)?;
        let mut rels = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">");
        for i in 0..nsheets {
            rels.push_str(&format!("<Relationship Id=\"rId{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet{}.xml\"/>", i + 1, i + 1));
        }
        rels.push_str(&format!("<Relationship Id=\"rId{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/>", nsheets + 1));
        rels.push_str("</Relationships>");
        zip.write_all(rels.as_bytes())?;

        // xl/styles.xml — fixed: index 0 = default, index 1 = bold, index 2 = header (bold+bg+font)
        zip.start_file("xl/styles.xml", opts)?;
        zip.write_all(b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<styleSheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">\n<fonts count=\"3\">\n<font><sz val=\"11\"/><name val=\"Calibri\"/></font>\n<font><b/><sz val=\"11\"/><name val=\"Calibri\"/></font>\n<font><b/><sz val=\"11\"/><color rgb=\"FFFFFFFF\"/><name val=\"Calibri\"/></font>\n</fonts>\n<fills count=\"4\">\n<fill><patternFill patternType=\"none\"/></fill>\n<fill><patternFill patternType=\"none\"/></fill>\n<fill><patternFill patternType=\"solid\"><fgColor rgb=\"FF4472C4\"/></patternFill></fill>\n<fill><patternFill patternType=\"solid\"><fgColor rgb=\"FFF2F2F2\"/></patternFill></fill>\n</fills>\n<borders count=\"1\"><border/></borders>\n<cellStyleXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\"/></cellStyleXfs>\n<cellXfs count=\"8\">\n<xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\"/>\n<xf numFmtId=\"0\" fontId=\"1\" fillId=\"0\" borderId=\"0\" xfId=\"0\" applyFont=\"1\"/>\n<xf numFmtId=\"0\" fontId=\"2\" fillId=\"2\" borderId=\"0\" xfId=\"0\" applyFont=\"1\" applyFill=\"1\" applyAlignment=\"1\"><alignment vertical=\"center\" wrapText=\"1\"/></xf>\n<xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\" applyAlignment=\"1\"><alignment vertical=\"top\" wrapText=\"1\"/></xf>\n<xf numFmtId=\"0\" fontId=\"0\" fillId=\"3\" borderId=\"0\" xfId=\"0\" applyFill=\"1\"/>\n<xf numFmtId=\"0\" fontId=\"0\" fillId=\"3\" borderId=\"0\" xfId=\"0\" applyFill=\"1\" applyAlignment=\"1\"><alignment vertical=\"top\" wrapText=\"1\"/></xf>\n<xf numFmtId=\"14\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\" applyNumberFormat=\"1\"/>\n<xf numFmtId=\"22\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\" applyNumberFormat=\"1\"/>\n</cellXfs>\n</styleSheet>")?;

        // xl/worksheets/sheetN.xml
        for (i, sheet) in sheets.iter().enumerate() {
            zip.start_file(format!("xl/worksheets/sheet{}.xml", i + 1), opts)?;
            let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">");

            // A pane needs at least one row below it; a larger value has no
            // meaning in the grid and is left out.
            if sheet.freeze_rows > 0 && sheet.freeze_rows < MAX_ROWS {
                let n = sheet.freeze_rows;
                xml.push_str(&format!(
                    "<sheetViews><sheetView workbookViewId=\"0\"><pane ySplit=\"{n}\" topLeftCell=\"A{}\" activePane=\"bottomLeft\" state=\"frozen\"/><selection pane=\"bottomLeft\" activeCell=\"A{}\" sqref=\"A{}\"/></sheetView></sheetViews>",
                    n + 1,
                    n + 1,
                    n + 1
                ));
            }

            // A width that is not a number, or for a column outside the grid,
            // is left out; the rest are brought into Excel's 0 to 255.
            let widths: Vec<(u16, f64)> = sheet
                .column_widths
                .iter()
                .filter(|(col, width)| **col < MAX_COLS && width.is_finite())
                .map(|(col, width)| (*col, width.clamp(0.0, MAX_COLUMN_WIDTH)))
                .collect();
            if !widths.is_empty() {
                xml.push_str("<cols>");
                for (col, width) in widths {
                    xml.push_str(&format!(
                        "<col min=\"{}\" max=\"{}\" width=\"{}\" customWidth=\"1\"/>",
                        col + 1,
                        col + 1,
                        width
                    ));
                }
                xml.push_str("</cols>");
            }

            xml.push_str("<sheetData>");
            let mut current_row: Option<u32> = None;
            for (&(row, col), (value, style_idx, kind)) in &sheet.cells {
                if current_row != Some(row) {
                    if current_row.is_some() {
                        xml.push_str("</row>");
                    }
                    write!(xml, "<row r=\"{}\">", row + 1)?;
                    current_row = Some(row);
                }
                write!(xml, "<c r=\"{}{}\"", col_letter(col), row + 1)?;
                if let Some(idx) = style_idx {
                    write!(xml, " s=\"{idx}\"")?;
                }
                match value {
                    CellValue::String(s) if *kind == CellKind::Formula => {
                        write!(xml, "><f>{}</f></c>", xe(s))?;
                    }
                    CellValue::String(s) => {
                        write!(
                            xml,
                            " t=\"inlineStr\"><is><t{}>{}</t></is></c>",
                            space_attr(s),
                            xe(s)
                        )?;
                    }
                    // SpreadsheetML has no NaN or infinity; Excel shows a
                    // result it cannot represent as `#NUM!`.
                    CellValue::Number(n) if !n.is_finite() => {
                        xml.push_str(" t=\"e\"><v>#NUM!</v></c>");
                    }
                    CellValue::Number(n) => {
                        write!(xml, "><v>{n}</v></c>")?;
                    }
                    CellValue::Bool(b) => {
                        write!(xml, " t=\"b\"><v>{}</v></c>", u8::from(*b))?;
                    }
                    CellValue::Empty => xml.push_str("/>"),
                }
            }
            if current_row.is_some() {
                xml.push_str("</row>");
            }
            xml.push_str("</sheetData></worksheet>");
            zip.write_all(xml.as_bytes())?;
        }

        zip.finish()?;
        Ok(())
    }
}

impl Default for Workbook {
    fn default() -> Self {
        Self::new()
    }
}

// ─── JSON-spec convenience API (for FFI bindings) ────────────────────────────

/// A JSON-serializable workbook specification. This is the interchange
/// format used by the Python (`xlsx_fast_write`) and Node (`xlsx-fast-write-node`)
/// bindings so callers can describe an entire workbook as one JSON payload.
///
/// ```json
/// {
///   "sheets": [
///     {
///       "name": "Summary",
///       "column_widths": { "0": 12.0, "1": 20.0 },
///       "rows": [
///         [ {"t":"s","v":"Page","fmt":"header"}, {"t":"s","v":"Type","fmt":"header"} ],
///         [ {"t":"n","v":1}, {"t":"s","v":"Digital"} ]
///       ]
///     }
///   ]
/// }
/// ```
#[derive(Debug, serde::Deserialize)]
pub struct WorkbookSpec {
    /// Worksheets in order.
    pub sheets: Vec<SheetSpec>,
}

/// A JSON-serializable worksheet specification.
#[derive(Debug, serde::Deserialize)]
pub struct SheetSpec {
    /// Sheet name.
    pub name: String,
    /// Optional column widths keyed by 0-based column index (as string keys).
    #[serde(default)]
    pub column_widths: std::collections::HashMap<String, f64>,
    /// Rows of cells, in row-major order (row 0 first).
    #[serde(default)]
    pub rows: Vec<Vec<CellSpec>>,
}

/// A JSON-serializable cell specification.
///
/// `t` is the cell type: `"s"` (string), `"n"` (number), `"b"` (bool),
/// `"d"` (date: an ISO `YYYY-MM-DD[THH:MM[:SS]]` string or an Excel serial
/// number) or `"f"` (formula). A value that does not fit the declared type is
/// written as what it is: text stays text, `null` leaves the cell empty.
///
/// Only `"f"` creates a formula. Text under any other type is written as
/// text even when it starts with `=`, `+`, `-` or `@`.
/// `fmt` optionally names a format: `"bold"` or `"header"`.
#[derive(Debug, serde::Deserialize)]
pub struct CellSpec {
    /// Cell type discriminator: `s`, `n`, `b`, `d` or `f`.
    #[serde(default = "default_cell_type")]
    pub t: String,
    /// The value (string, number, or bool as JSON).
    pub v: serde_json::Value,
    /// Optional format name: `bold` or `header`.
    #[serde(default)]
    pub fmt: Option<String>,
}

fn default_cell_type() -> String {
    "s".to_string()
}

impl Workbook {
    /// Build a workbook from a [`WorkbookSpec`].
    pub fn from_spec(spec: WorkbookSpec) -> Self {
        let mut wb = Workbook::new();
        for sheet_spec in spec.sheets {
            let sheet = wb.add_sheet(sheet_spec.name);
            for (col_str, width) in &sheet_spec.column_widths {
                if let Ok(col) = col_str.parse::<u16>() {
                    sheet.set_column_width(col, *width);
                }
            }
            for (row_idx, row) in sheet_spec.rows.iter().enumerate() {
                // A position past the integer type is past Excel's grid too.
                // Saturating keeps it there, so writing reports it instead
                // of wrapping around onto column A.
                let row_idx = u32::try_from(row_idx).unwrap_or(u32::MAX);
                for (col_idx, cell) in row.iter().enumerate() {
                    let col_idx = u16::try_from(col_idx).unwrap_or(u16::MAX);
                    use serde_json::Value;
                    // The declared type applies when the value fits it.
                    match (cell.t.as_str(), &cell.v) {
                        ("f", Value::String(formula)) => {
                            sheet.write_formula(row_idx, col_idx, formula.as_str());
                            continue;
                        }
                        ("d", Value::String(iso)) if sheet.write_date(row_idx, col_idx, iso) => {
                            continue;
                        }
                        ("d", Value::Number(serial)) => {
                            if let Some(serial) = serial.as_f64() {
                                sheet.write_date_serial(row_idx, col_idx, serial);
                                continue;
                            }
                        }
                        _ => {}
                    }
                    let numeric_text = match (cell.t.as_str(), &cell.v) {
                        ("n", Value::String(text)) => text
                            .trim()
                            .parse::<f64>()
                            .ok()
                            .filter(|number| number.is_finite()),
                        _ => None,
                    };
                    // Otherwise the value is written as what it is, so a
                    // mismatch never turns into a different value (a number
                    // given as text used to become 0).
                    let value = match (&cell.v, numeric_text) {
                        (_, Some(number)) => CellValue::Number(number),
                        (Value::Null, _) => CellValue::Empty,
                        (Value::Bool(flag), _) => CellValue::Bool(*flag),
                        (Value::Number(number), _) if cell.t == "s" => {
                            CellValue::String(number.to_string())
                        }
                        (Value::Number(number), _) => match number.as_f64() {
                            Some(number) => CellValue::Number(number),
                            None => CellValue::String(number.to_string()),
                        },
                        (Value::String(text), _) => CellValue::String(text.clone()),
                        (other, _) => CellValue::String(other.to_string()),
                    };
                    let fmt = match cell.fmt.as_deref() {
                        Some("header") => Format::new()
                            .bold()
                            .background_color(0x4472C4)
                            .font_color(0xFFFFFF),
                        Some(_) => Format::new().bold(),
                        None => {
                            sheet.write(row_idx, col_idx, value);
                            continue;
                        }
                    };
                    sheet.write_with_format(row_idx, col_idx, value, &fmt);
                }
            }
        }
        wb
    }

    /// Parse a JSON workbook spec and write the resulting `.xlsx` to `path`.
    pub fn write_json_spec(
        json: &str,
        path: impl AsRef<Path>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let spec: WorkbookSpec = serde_json::from_str(json)?;
        let wb = Workbook::from_spec(spec);
        wb.save(path)?;
        Ok(())
    }

    /// Parse a JSON workbook spec and return the `.xlsx` bytes.
    pub fn write_json_spec_to_bytes(json: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let spec: WorkbookSpec = serde_json::from_str(json)?;
        let wb = Workbook::from_spec(spec);
        let mut buf = std::io::Cursor::new(Vec::new());
        wb.write_to(&mut buf)?;
        Ok(buf.into_inner())
    }
}

/// Days from 1970-01-01 to the given proleptic Gregorian date.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Returns Excel's serial number (1900 date system) for a calendar date.
///
/// Returns `None` when the date does not exist or lies outside 1900-03-01 to
/// 9999-12-31. Earlier dates are refused because Excel counts a 29 February
/// 1900 that never happened, so their serial numbers are ambiguous.
pub fn date_serial(year: i32, month: u32, day: u32) -> Option<f64> {
    if !(1900..=9999).contains(&year) || !(1..=12).contains(&month) || day == 0 {
        return None;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let days_in_month = match month {
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => 31,
    };
    if day > days_in_month || (year == 1900 && month < 3) {
        return None;
    }
    let days = days_from_civil(i64::from(year), i64::from(month), i64::from(day))
        - days_from_civil(1899, 12, 30);
    Some(days as f64)
}

/// Parses the date forms [`Sheet::write_date`] documents into a serial number.
fn parse_iso_date(iso: &str) -> Option<f64> {
    let iso = iso.trim();
    let iso = iso.strip_suffix('Z').unwrap_or(iso);
    let (date, time) = match iso.find(['T', ' ']) {
        Some(index) => (&iso[..index], Some(&iso[index + 1..])),
        None => (iso, None),
    };
    let digits = |text: &str, len: usize| -> Option<u32> {
        (text.len() == len && text.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| text.parse().ok())
            .flatten()
    };
    let mut parts = date.split('-');
    let year = digits(parts.next()?, 4)?;
    let month = digits(parts.next()?, 2)?;
    let day = digits(parts.next()?, 2)?;
    if parts.next().is_some() {
        return None;
    }
    let mut serial = date_serial(i32::try_from(year).ok()?, month, day)?;
    if let Some(time) = time {
        let mut parts = time.split(':');
        let hour = digits(parts.next()?, 2)?;
        let minute = digits(parts.next()?, 2)?;
        let second: f64 = match parts.next() {
            Some(text) => {
                let whole = text.split('.').next()?;
                digits(whole, 2)?;
                text.parse().ok()?
            }
            None => 0.0,
        };
        if parts.next().is_some() || hour > 23 || minute > 59 || !(0.0..60.0).contains(&second) {
            return None;
        }
        serial += (f64::from(hour) * 3600.0 + f64::from(minute) * 60.0 + second) / 86_400.0;
    }
    Some(serial)
}

fn col_letter(col: u16) -> String {
    let mut n = u32::from(col) + 1;
    let mut s = String::new();
    while n > 0 {
        n -= 1;
        s.insert(0, (b'A' + (n % 26) as u8) as char);
        n /= 26;
    }
    s
}

/// Returns whether XML 1.0 cannot carry `ch` as a literal character.
///
/// A carriage return is legal XML, but every XML reader turns a literal one
/// into a line feed, so it is treated like the characters that are forbidden.
fn needs_xstring_escape(ch: char) -> bool {
    matches!(ch, '\u{0}'..='\u{8}' | '\u{b}' | '\u{c}' | '\r' | '\u{e}'..='\u{1f}' | '\u{fffe}' | '\u{ffff}')
}

/// Returns whether the underscore `rest` starts with must be protected.
///
/// A reader decodes `_xHHHH_` as an escape. The closing underscore can come
/// from the caller's text, or from this writer: a character that is itself
/// written as an escape (a carriage return becomes `_x000D_`) puts an
/// underscore right after `_xHHHH`. Both cases are protected.
fn looks_like_xstring_escape(rest: &str) -> bool {
    let bytes = rest.as_bytes();
    let prefix = bytes.len() > 6
        && bytes[0] == b'_'
        && bytes[1] == b'x'
        && bytes[2..6].iter().all(u8::is_ascii_hexdigit);
    // The six prefix bytes are ASCII, so index 6 is a character boundary.
    prefix
        && rest[6..]
            .chars()
            .next()
            .is_some_and(|next| next == '_' || needs_xstring_escape(next))
}

/// Escapes cell text for the content of a `<t>` element.
///
/// Markup characters become entities. A character XML 1.0 cannot carry is
/// written the way Excel writes it, as `_xHHHH_` (ECMA-376 `ST_Xstring`), and
/// an underscore that would start such an escape in the caller's own text is
/// written `_x005F_` so readers give the text back unchanged.
///
/// Compatibility: Excel and xlsx-fast-core decode `_xHHHH_` in string values,
/// but openpyxl returns these escapes literally. A consumer reading these
/// workbooks with openpyxl sees `_x000D_` for a carriage return and
/// `_x005F_x0041_` for the literal text `_x0041_`.
fn xe(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for (index, ch) in s.char_indices() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '_' if looks_like_xstring_escape(&s[index..]) => out.push_str("_x005F_"),
            c if needs_xstring_escape(c) => {
                out.push_str(&format!("_x{:04X}_", u32::from(c)));
            }
            c => out.push(c),
        }
    }
    out
}

/// Returns the attribute that keeps the whitespace at the ends of `<t>` text.
///
/// Without `xml:space="preserve"` a reader may drop leading and trailing
/// whitespace. Text without any gets no attribute, so its bytes do not change.
fn space_attr(s: &str) -> &'static str {
    let edge = |ch: Option<char>| matches!(ch, Some(' ' | '\t' | '\n'));
    if edge(s.chars().next()) || edge(s.chars().next_back()) {
        " xml:space=\"preserve\""
    } else {
        ""
    }
}

/// Escapes a value written between double quotes in an attribute.
///
/// Besides the markup characters and the quote, a tab, line feed or carriage
/// return is written as a character reference: typed literally, XML
/// attribute-value normalization turns each into a space. A character XML
/// 1.0 cannot carry at all becomes U+FFFD, because attributes have no
/// `_xHHHH_` form.
fn xa(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\t' => out.push_str("&#9;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            c if needs_xstring_escape(c) => out.push(char::REPLACEMENT_CHARACTER),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_col_letter() {
        assert_eq!(col_letter(0), "A");
        assert_eq!(col_letter(25), "Z");
        assert_eq!(col_letter(26), "AA");
    }

    #[test]
    fn add_sheet_returns_mutable_handle_and_count() {
        // WHY: public API must not rely on unwrap after push; callers need
        // indexed access for multi-sheet builders without re-borrowing issues.
        let mut wb = Workbook::new();
        assert_eq!(wb.sheet_count(), 0);
        wb.add_sheet("A").write(0, 0, "x");
        wb.add_sheet("B").write(0, 0, "y");
        assert_eq!(wb.sheet_count(), 2);
        assert_eq!(wb.sheet_mut(0).map(|s| s.name()), Some("A"));
        assert!(wb.sheet_mut(99).is_none());
    }

    #[test]
    fn test_write_simple() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut wb = Workbook::new();
        let s = wb.add_sheet("Test");
        s.write(0, 0, "Hello");
        s.write(1, 0, CellValue::Number(42.0));
        wb.save(tmp.path()).unwrap();
        assert!(tmp.path().exists());
    }

    #[test]
    fn test_json_spec_round_trip() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let json = r#"{"sheets":[{"name":"S","column_widths":{"0":10.0},"rows":[[{"t":"s","v":"Name","fmt":"header"},{"t":"s","v":"Value"}],[{"t":"s","v":"Alpha"},{"t":"n","v":42}]]}]}"#;
        Workbook::write_json_spec(json, tmp.path()).unwrap();
        let rb = xlsx_fast_core::parse_xlsx(tmp.path(), xlsx_fast_core::ParseOptions::default())
            .unwrap();
        assert_eq!(rb.sheets.len(), 1);
        assert_eq!(rb.sheets[0].name, "S");
        assert!(rb.sheets[0].cells.len() >= 4);
    }

    #[test]
    fn test_round_trip() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut wb = Workbook::new();
        let s = wb.add_sheet("RT");
        s.write(0, 0, "Name");
        s.write(0, 1, "Value");
        s.write(1, 0, "Alpha");
        s.write(1, 1, CellValue::Number(100.0));
        wb.save(tmp.path()).unwrap();

        let read_back =
            xlsx_fast_core::parse_xlsx(tmp.path(), xlsx_fast_core::ParseOptions::default())
                .unwrap();
        assert_eq!(read_back.sheets.len(), 1);
        assert_eq!(read_back.sheets[0].name, "RT");
        assert!(read_back.sheets[0].cells.len() >= 4);
        // Numeric cells must round-trip through our own reader: a number written
        // without its <v> wrapper parses back as empty, so counting cells alone
        // hides the loss.
        let at = |row: u32, col: u32| {
            read_back.sheets[0]
                .cells
                .iter()
                .find(|c| c.row == row && c.col == col)
        };
        match at(2, 2).map(|c| c.value.clone()) {
            Some(xlsx_fast_core::CellValue::Number { v }) => assert_eq!(v, 100.0),
            other => panic!("expected 100.0 at (row 2, col 2), got {other:?}"),
        }
        match at(2, 1).map(|c| c.value.clone()) {
            Some(xlsx_fast_core::CellValue::String { v }) => assert_eq!(&*v, "Alpha"),
            other => panic!("expected \"Alpha\" at (row 2, col 1), got {other:?}"),
        }
    }

    /// WHY: exports need a sticky header, wrapped long cells and zebra striping
    /// to be readable. A frozen pane (`state="frozen"`) is a worksheet feature
    /// separate from styles, so it needs its own assertion.
    #[test]
    fn freeze_and_style_markup_is_emitted() {
        use std::io::Read;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut wb = Workbook::new();
        let s = wb.add_sheet("F");
        s.write_with_format(
            0,
            0,
            "Header",
            &Format::new()
                .bold()
                .background_color(0x4472C4)
                .font_color(0xFFFFFF),
        );
        s.write_with_format(1, 0, "long text", &Format::new().wrap().zebra());
        s.set_freeze_rows(1);
        s.set_column_width(0, 30.0);
        wb.save(tmp.path()).unwrap();

        let mut ar = zip::ZipArchive::new(std::fs::File::open(tmp.path()).unwrap()).unwrap();
        let mut sheet = String::new();
        ar.by_name("xl/worksheets/sheet1.xml")
            .unwrap()
            .read_to_string(&mut sheet)
            .unwrap();
        assert!(sheet.contains("state=\"frozen\""), "expected a frozen pane");
        assert!(
            sheet.contains("<col min=\"1\" max=\"1\" width=\"30\""),
            "expected column width"
        );

        let mut styles = String::new();
        ar.by_name("xl/styles.xml")
            .unwrap()
            .read_to_string(&mut styles)
            .unwrap();
        assert!(styles.contains("wrapText=\"1\""), "expected wrap alignment");
        assert!(styles.contains("FFF2F2F2"), "expected zebra fill");
    }

    /// WHY: JSON-spec docs advertise `"fmt":"header"` for the blue header
    /// style. Mapping every fmt to bold-only silently drops the fill that
    /// Python/Node bindings document and that operators expect in exports.
    #[test]
    fn json_spec_header_fmt_emits_style_index_2() {
        let json = r#"{"sheets":[{"name":"S","rows":[[{"t":"s","v":"Page","fmt":"header"}]]}]}"#;
        let bytes = Workbook::write_json_spec_to_bytes(json).unwrap();
        let sheet_xml = sheet1_xml(&bytes);
        assert!(
            sheet_xml.contains(r#"s="2""#),
            "fmt=header must map to cellXfs index 2; sheet xml:\n{sheet_xml}"
        );
    }

    /// WHY: bold-only Format (totals rows in pdf2excel) must stay on xf 1 so
    /// header fill is not applied to non-header emphasis cells.
    #[test]
    fn write_with_format_bold_emits_style_index_1() {
        let mut wb = Workbook::new();
        let bold = Format::new().bold();
        wb.add_sheet("S").write_with_format(0, 0, "Total", &bold);
        let mut buf = std::io::Cursor::new(Vec::new());
        wb.write_to(&mut buf).unwrap();
        let sheet_xml = sheet1_xml(buf.get_ref());
        assert!(
            sheet_xml.contains(r#"s="1""#),
            "bold-only Format must map to cellXfs index 1; sheet xml:\n{sheet_xml}"
        );
        assert!(
            !sheet_xml.contains(r#"s="2""#),
            "bold-only must not use header index 2; sheet xml:\n{sheet_xml}"
        );
    }

    /// WHY: gateway `pdf2excel` writes header cells with bold+fill+white font.
    /// styles.xml already defines xf index 2 for that header look; ignoring
    /// Format fields and always emitting `s="1"` (bold-only) silently drops
    /// the header fill that operators rely on in exported workbooks.
    #[test]
    fn write_with_format_header_emits_style_index_2() {
        let mut wb = Workbook::new();
        let header = Format::new()
            .bold()
            .background_color(0x4472C4)
            .font_color(0xFFFFFF);
        wb.add_sheet("S").write_with_format(0, 0, "Page", &header);
        let mut buf = std::io::Cursor::new(Vec::new());
        wb.write_to(&mut buf).unwrap();
        let sheet_xml = sheet1_xml(buf.get_ref());
        assert!(
            sheet_xml.contains(r#"s="2""#),
            "header Format must map to cellXfs index 2; sheet xml:\n{sheet_xml}"
        );
        assert!(
            !sheet_xml.contains(r#"s="1""#),
            "header must not fall back to bold-only index 1; sheet xml:\n{sheet_xml}"
        );
    }

    /// WHY: Excel cell addresses are 1-based. Emitting `A0` for row 0 makes
    /// the package invalid for Excel / LibreOffice and breaks any consumer
    /// that joins on `r="A1"` (gateway exports, round-trip parsers).
    #[test]
    fn cell_refs_use_one_based_row_numbers() {
        let mut wb = Workbook::new();
        wb.add_sheet("S").write(0, 0, "Page");
        let mut buf = std::io::Cursor::new(Vec::new());
        wb.write_to(&mut buf).unwrap();
        let sheet_xml = sheet1_xml(buf.get_ref());
        assert!(
            sheet_xml.contains(r#"r="A1""#),
            "row 0 must serialize as A1; sheet xml:\n{sheet_xml}"
        );
        assert!(
            !sheet_xml.contains(r#"r="A0""#),
            "must not emit zero-based A0; sheet xml:\n{sheet_xml}"
        );
    }

    /// WHY: the bytes this writer emits for markup characters are a contract
    /// with every reader. They must not move when a dependency does, and what
    /// is written must come back unchanged through our own reader, which sees
    /// each `&amp;` as a reference event separate from the text around it.
    #[test]
    fn markup_characters_are_escaped_the_same_way_and_read_back() {
        let name = "A&B <c> 'q' \u{e9}";
        let texts = [
            "a & b",
            "<tag> x > y",
            "\"double\" 'single'",
            "&amp; &#233; &foo;",
            " lead & trail ",
            "tab\there",
            "caf\u{e9} \u{1F600}",
        ];
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet(name);
        for (row, text) in (0u32..).zip(texts) {
            sheet.write(row, 0, text);
        }
        let mut buf = std::io::Cursor::new(Vec::new());
        wb.write_to(&mut buf).unwrap();
        let bytes = buf.into_inner();

        let workbook_xml = part_xml(&bytes, "xl/workbook.xml");
        assert!(
            workbook_xml.contains("<sheet name=\"A&amp;B &lt;c&gt; 'q' \u{e9}\" sheetId=\"1\""),
            "workbook xml:\n{workbook_xml}"
        );
        let sheet_xml = sheet1_xml(&bytes);
        for escaped in [
            "<t>a &amp; b</t>",
            "<t>&lt;tag&gt; x &gt; y</t>",
            "<t>\"double\" 'single'</t>",
            "<t>&amp;amp; &amp;#233; &amp;foo;</t>",
            "<t xml:space=\"preserve\"> lead &amp; trail </t>",
            "<t>tab\there</t>",
            "<t>caf\u{e9} \u{1F600}</t>",
        ] {
            assert!(
                sheet_xml.contains(escaped),
                "missing {escaped:?}; sheet xml:\n{sheet_xml}"
            );
        }

        let read_back =
            xlsx_fast_core::parse_xlsx_bytes(bytes, xlsx_fast_core::ParseOptions::default())
                .unwrap();
        assert_eq!(read_back.sheets[0].name, name);
        let read: Vec<String> = read_back.sheets[0]
            .cells
            .iter()
            .map(|cell| match &cell.value {
                xlsx_fast_core::CellValue::String { v } => v.to_string(),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(read, texts);
    }

    fn round_trip(name: &str, texts: &[&str]) -> (Vec<u8>, String, Vec<String>) {
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet(name);
        for (row, text) in (0u32..).zip(texts) {
            sheet.write(row, 0, *text);
        }
        let mut buf = std::io::Cursor::new(Vec::new());
        wb.write_to(&mut buf).unwrap();
        let bytes = buf.into_inner();
        let read_back = xlsx_fast_core::parse_xlsx_bytes(
            bytes.clone(),
            xlsx_fast_core::ParseOptions::default(),
        )
        .unwrap();
        let read = read_back.sheets[0]
            .cells
            .iter()
            .map(|cell| match &cell.value {
                xlsx_fast_core::CellValue::String { v } => v.to_string(),
                other => format!("{other:?}"),
            })
            .collect();
        (bytes, read_back.sheets[0].name.clone(), read)
    }

    /// WHY: a sheet name is written into an attribute. An unescaped double
    /// quote ends the attribute early and the workbook no longer opens; a
    /// literal tab or line break would be read back as a space.
    #[test]
    fn a_sheet_name_with_quotes_and_whitespace_reads_back() {
        let name = "say \"hi\" & 'bye' <x>\ttab\nline";
        let (bytes, read_name, _) = round_trip(name, &["x"]);
        assert_eq!(read_name, name);
        let workbook_xml = part_xml(&bytes, "xl/workbook.xml");
        assert!(
            workbook_xml
                .contains("name=\"say &quot;hi&quot; &amp; 'bye' &lt;x&gt;&#9;tab&#10;line\""),
            "workbook xml:\n{workbook_xml}"
        );
    }

    /// WHY: XML 1.0 cannot carry most control characters, so Excel writes
    /// them as `_xHHHH_`; text that already looks like such an escape must be
    /// protected or a reader turns it into another character.
    #[test]
    fn control_characters_and_escape_lookalikes_read_back() {
        let texts = [
            "ctl\u{1}\u{8}\u{b}\u{c}\u{1f}end",
            "cr\rlf\ncrlf\r\n",
            "literal _x0041_ and _x000D_ stay",
            "_x005F_x0041_",
            "not escapes: _x41_ _xZZZZ_ _x0041 a_b",
            "\u{fffe}\u{ffff}",
            "nul\u{0}",
        ];
        let (bytes, _, read) = round_trip("S", &texts);
        assert_eq!(read, texts);
        let sheet_xml = sheet1_xml(&bytes);
        assert!(sheet_xml.contains("<t>ctl_x0001__x0008__x000B__x000C__x001F_end</t>"));
        assert!(sheet_xml.contains("<t>literal _x005F_x0041_ and _x005F_x000D_ stay</t>"));
        assert!(sheet_xml.contains("<t>not escapes: _x41_ _xZZZZ_ _x0041 a_b</t>"));
        assert!(
            !sheet_xml
                .bytes()
                .any(|b| b < 0x20 && b != b'\n' && b != b'\t'),
            "a forbidden control character was written raw"
        );
    }

    /// WHY: the character after `_xHHHH` decides whether a reader sees an
    /// escape, and the writer may itself turn that character into something
    /// that starts with `_` (a control character becomes `_x000D_`). Every
    /// combination of prefix and follower must read back as written.
    #[test]
    fn an_escape_prefix_followed_by_any_character_reads_back() {
        let prefixes = [
            "_x0041", "_x000D", "_x000d", "_x005F", "_xFFFF", "_xD83D", "_x0000", "_xabcd",
            "_xZZZZ", "_x004", "_X0041",
        ];
        let followers = [
            "", "_", "__", "\r", "\n", "\t", "\u{0}", "\u{1}", "\u{1f}", "\u{fffe}", "A", "x", "0",
            "_x0042_", "_x0042", "\r_", "_\r", "_x", "&", " ",
        ];
        let mut texts = Vec::new();
        for prefix in prefixes {
            for follower in followers {
                texts.push(format!("{prefix}{follower}"));
                texts.push(format!("a{prefix}{follower}b"));
                texts.push(format!("{prefix}{prefix}{follower}"));
            }
        }
        let borrowed: Vec<&str> = texts.iter().map(String::as_str).collect();
        let (_, _, read) = round_trip("S", &borrowed);
        assert_eq!(read.len(), texts.len());
        for (written, read) in texts.iter().zip(&read) {
            assert_eq!(read, written, "written {written:?}");
        }
    }

    /// WHY: without `xml:space="preserve"` a reader is free to drop the
    /// whitespace at either end of a text element, and Excel does.
    #[test]
    fn leading_and_trailing_whitespace_is_marked_to_be_preserved() {
        let texts = [" lead", "trail ", "\ttab", "line\n", "  ", "in side"];
        let (bytes, _, read) = round_trip("S", &texts);
        assert_eq!(read, texts);
        let sheet_xml = sheet1_xml(&bytes);
        assert!(sheet_xml.contains("<t xml:space=\"preserve\"> lead</t>"));
        assert!(sheet_xml.contains("<t xml:space=\"preserve\">trail </t>"));
        assert!(sheet_xml.contains("<t xml:space=\"preserve\">  </t>"));
        assert!(sheet_xml.contains("<t>in side</t>"));
        assert_eq!(sheet_xml.matches("xml:space").count(), 5);
    }

    fn sheet1_xml(xlsx_bytes: &[u8]) -> String {
        part_xml(xlsx_bytes, "xl/worksheets/sheet1.xml")
    }

    fn part_xml(xlsx_bytes: &[u8], part: &str) -> String {
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(xlsx_bytes)).unwrap();
        let mut file = archive.by_name(part).unwrap();
        let mut xml = String::new();
        std::io::Read::read_to_string(&mut file, &mut xml).unwrap();
        xml
    }
}
