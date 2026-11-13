//! Workbook parts whose bytes are not UTF-8.
//!
//! A byte that is not UTF-8 costs the character it stands in for when it is
//! in text, fails the workbook when it is in an attribute the parser reads,
//! and costs nothing anywhere the parser does not look. These outcomes predate
//! the quick-xml 0.42 upgrade, which made its reader reject such bytes
//! wherever they are; the expected values are what the parser produced before
//! it, on the JSON path and on the Arrow path.

use std::io::{Cursor, Write};
use xlsx_fast_core::arrow::{parse_xlsx_to_arrow_ipc_bytes, ArrowOptions};
use xlsx_fast_core::{parse_xlsx_bytes, ParseOptions};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

const ROOT_RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#;

const WORKBOOK_RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/></Relationships>"#;

const NS: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const REL_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

/// Every `@` becomes the ISO-8859-1 byte 0xE9, which is not UTF-8 alone.
fn latin_1(text: &str) -> Vec<u8> {
    text.bytes()
        .map(|byte| if byte == b'@' { 0xE9 } else { byte })
        .collect()
}

/// Builds a one-sheet workbook from its sheet name, the inner XML of `<sst>`
/// and the whole sheet part, with `@` read as in [`latin_1`].
fn workbook(sheet_name: &str, shared_strings: &str, sheet: &[u8]) -> Vec<u8> {
    let workbook = latin_1(&format!(
        r#"<workbook xmlns="{NS}" xmlns:r="{REL_NS}"><sheets><sheet name="{sheet_name}" sheetId="1" r:id="rId1"/></sheets></workbook>"#
    ));
    let shared_strings = latin_1(&format!(r#"<sst xmlns="{NS}">{shared_strings}</sst>"#));
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    for (name, body) in [
        ("_rels/.rels", ROOT_RELS.as_bytes()),
        ("xl/workbook.xml", &workbook),
        ("xl/_rels/workbook.xml.rels", WORKBOOK_RELS.as_bytes()),
        ("xl/sharedStrings.xml", &shared_strings),
        ("xl/worksheets/sheet1.xml", sheet),
    ] {
        zip.start_file(name, options).unwrap();
        zip.write_all(body).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

/// A sheet part with a prologue before the root, attributes on the root and
/// the inner XML of `<sheetData>`, with `@` read as in [`latin_1`].
fn sheet(prologue: &str, root_attrs: &str, data: &str) -> Vec<u8> {
    latin_1(&format!(
        r#"{prologue}<worksheet xmlns="{NS}"{root_attrs}><sheetData>{data}</sheetData></worksheet>"#
    ))
}

/// A sheet with one shared-string cell and one inline-string cell holding
/// `text`.
fn two_cells(prologue: &str, root_attrs: &str, text: &str) -> Vec<u8> {
    sheet(
        prologue,
        root_attrs,
        &format!(
            r#"<row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="inlineStr"><is><t>{text}</t></is></c></row>"#
        ),
    )
}

/// Parses `bytes` on the JSON path: the workbook as JSON, or `<parse error>`.
fn json(bytes: Vec<u8>) -> String {
    match parse_xlsx_bytes(bytes, ParseOptions::default()) {
        Ok(workbook) => serde_json::to_string(&workbook).unwrap(),
        Err(_) => "<parse error>".to_string(),
    }
}

/// Parses `bytes` on the Arrow path: the IPC stream's length and byte sum,
/// or `<parse error>`.
fn arrow(bytes: Vec<u8>) -> String {
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), bytes).unwrap();
    match parse_xlsx_to_arrow_ipc_bytes(
        file.path(),
        ParseOptions::default(),
        ArrowOptions::default(),
    ) {
        Ok(ipc) => format!(
            "{} bytes, sum {}",
            ipc.len(),
            ipc.iter().map(|byte| u64::from(*byte)).sum::<u64>()
        ),
        Err(_) => "<parse error>".to_string(),
    }
}

fn both(bytes: Vec<u8>) -> (String, String) {
    (json(bytes.clone()), arrow(bytes))
}

#[test]
fn a_latin_1_byte_in_text_reads_as_a_replacement_character() {
    let (json, arrow) = both(workbook(
        "Sheet1",
        "<si><t>caf@</t></si>",
        &two_cells("", "", "th@"),
    ));
    assert_eq!(
        json,
        concat!(
            r#"{"sheets":[{"id":1,"name":"Sheet1","cells":[{"row":1,"col":1,"#,
            r#""value":{"type":"string","v":"caf"#,
            "\u{FFFD}",
            r#""}},{"row":1,"col":2,"value":{"type":"string","v":"th"#,
            "\u{FFFD}",
            r#""}}]}]}"#
        )
    );
    assert_eq!(arrow, "2824 bytes, sum 38480");
}

#[test]
fn a_declared_latin_1_part_is_still_read_as_utf8() {
    let (json, arrow) = both(workbook(
        "Sheet1",
        "<si><t>caf@</t></si>",
        &two_cells(r#"<?xml version="1.0" encoding="ISO-8859-1"?>"#, "", "th@"),
    ));
    assert_eq!(
        json,
        concat!(
            r#"{"sheets":[{"id":1,"name":"Sheet1","cells":[{"row":1,"col":1,"#,
            r#""value":{"type":"string","v":"caf"#,
            "\u{FFFD}",
            r#""}},{"row":1,"col":2,"value":{"type":"string","v":"th"#,
            "\u{FFFD}",
            r#""}}]}]}"#
        )
    );
    assert_eq!(arrow, "2824 bytes, sum 38480");
}

#[test]
fn a_latin_1_byte_where_the_parser_does_not_look_costs_nothing() {
    // In a comment, a processing instruction and an attribute nobody reads.
    let (json, arrow) = both(workbook(
        "Sheet1",
        "<!-- caf@ --><si><t>plain</t></si>",
        &two_cells("<!-- caf@ --><?pi caf@?>", r#" foo="caf@""#, "text"),
    ));
    assert_eq!(
        json,
        concat!(
            r#"{"sheets":[{"id":1,"name":"Sheet1","cells":[{"row":1,"col":1,"#,
            r#""value":{"type":"string","v":"plain"}},{"row":1,"col":2,"#,
            r#""value":{"type":"string","v":"text"}}]}]}"#
        )
    );
    assert_eq!(arrow, "2824 bytes, sum 37704");
}

#[test]
fn a_latin_1_byte_in_the_sheet_name_fails_the_workbook() {
    let (json, arrow) = both(workbook(
        "caf@",
        "<si><t>plain</t></si>",
        &two_cells("", "", "text"),
    ));
    assert_eq!(json, "<parse error>");
    assert_eq!(arrow, "<parse error>");
}

#[test]
fn a_latin_1_byte_in_a_cell_reference_fails_the_workbook() {
    let (json, arrow) = both(workbook(
        "Sheet1",
        "<si><t>plain</t></si>",
        &sheet(
            "",
            "",
            r#"<row r="1"><c r="A1@" t="inlineStr"><is><t>x</t></is></c></row>"#,
        ),
    ));
    assert_eq!(json, "<parse error>");
    assert_eq!(arrow, "<parse error>");
}

#[test]
fn a_utf16_sheet_part_reads_as_a_sheet_without_cells() {
    let utf8 = String::from_utf8(two_cells("", "", "text")).unwrap();
    let mut utf16 = vec![0xFF, 0xFE];
    for unit in utf8.encode_utf16() {
        utf16.extend_from_slice(&unit.to_le_bytes());
    }
    let (json, arrow) = both(workbook("Sheet1", "<si><t>plain</t></si>", &utf16));
    // The parser does not transcode UTF-16 sheets: the markup is not
    // recognised, so the sheet is there and holds no cells.
    assert_eq!(json, r#"{"sheets":[{"id":1,"name":"Sheet1","cells":[]}]}"#);
    assert_eq!(arrow, "776 bytes, sum 31046");
}
