//! Cell text must survive the cases real workbooks contain: a stray bad byte,
//! CDATA, phonetic guides, `_xHHHH_` escapes, and rows or cells written
//! without an explicit reference.

use std::io::{Cursor, Write};
use xlsx_fast_core::{parse_xlsx_bytes, CellValue, ParseOptions, Workbook};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

const ROOT_RELS: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#;

const WORKBOOK: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets>
</workbook>"#;

const WORKBOOK_RELS: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/>
  <Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>"#;

/// Cell style 1 is the built-in date format 14; style 0 is General.
const STYLES: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <cellXfs count="2"><xf numFmtId="0"/><xf numFmtId="14"/></cellXfs>
</styleSheet>"#;

const NS: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";

/// Builds a one-sheet workbook from the inner XML of `<sst>` and `<sheetData>`.
fn workbook(shared_strings: &[u8], sheet_data: &[u8]) -> Vec<u8> {
    let mut sst =
        format!(r#"<?xml version="1.0" encoding="UTF-8"?><sst xmlns="{NS}">"#).into_bytes();
    sst.extend_from_slice(shared_strings);
    sst.extend_from_slice(b"</sst>");
    let mut sheet =
        format!(r#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="{NS}"><sheetData>"#)
            .into_bytes();
    sheet.extend_from_slice(sheet_data);
    sheet.extend_from_slice(b"</sheetData></worksheet>");

    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    for (name, body) in [
        ("_rels/.rels", ROOT_RELS),
        ("xl/workbook.xml", WORKBOOK),
        ("xl/_rels/workbook.xml.rels", WORKBOOK_RELS),
        ("xl/styles.xml", STYLES),
        ("xl/sharedStrings.xml", sst.as_slice()),
        ("xl/worksheets/sheet1.xml", sheet.as_slice()),
    ] {
        zip.start_file(name, options).unwrap();
        zip.write_all(body).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

fn parse(bytes: Vec<u8>) -> Workbook {
    parse_xlsx_bytes(
        bytes,
        ParseOptions {
            include_formulas: true,
            ..ParseOptions::default()
        },
    )
    .expect("workbook parses")
}

/// Every cell of the first sheet as `(row, col, text)`; non-text values are
/// rendered with `Debug` so a test that expects text fails loudly.
fn cells(bytes: Vec<u8>) -> Vec<(u32, u32, String)> {
    parse(bytes).sheets[0]
        .cells
        .iter()
        .map(|cell| {
            let text = match &cell.value {
                CellValue::String { v } => v.to_string(),
                other => format!("{other:?}"),
            };
            (cell.row, cell.col, text)
        })
        .collect()
}

fn texts(bytes: Vec<u8>) -> Vec<String> {
    cells(bytes).into_iter().map(|(_, _, text)| text).collect()
}

/// Replaces every `@` with the byte 0xE9, which is not valid UTF-8 on its own.
fn with_bad_byte(xml: &str) -> Vec<u8> {
    xml.bytes()
        .map(|byte| if byte == b'@' { 0xE9 } else { byte })
        .collect()
}

const SHARED_CELLS: &[u8] =
    br#"<row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c></row>"#;

#[test]
fn one_invalid_byte_in_a_shared_string_costs_one_character() {
    let sst = with_bad_byte("<si><t>caf@ ok &amp; fine</t></si><si><t>second</t></si>");
    assert_eq!(
        texts(workbook(&sst, SHARED_CELLS)),
        ["caf\u{fffd} ok & fine", "second"]
    );
}

#[test]
fn one_invalid_byte_in_a_rich_text_run_costs_one_character() {
    let sst = with_bad_byte(
        "<si><r><t>ri@h </t></r><r><rPr><b/></rPr><t>text</t></r></si><si><t>second</t></si>",
    );
    assert_eq!(
        texts(workbook(&sst, SHARED_CELLS)),
        ["ri\u{fffd}h text", "second"]
    );
}

#[test]
fn one_invalid_byte_in_sheet_text_costs_one_character() {
    let sheet = with_bad_byte(concat!(
        r#"<row r="1">"#,
        r#"<c r="A1" t="inlineStr"><is><t>inl@ne &lt;ok&gt;</t></is></c>"#,
        r#"<c r="B1" t="str"><f>"f@rmula"</f><v>val@e</v></c>"#,
        r#"<c r="C1" t="inlineStr"><is><r><t>ru@ </t></r><r><t>two</t></r></is></c>"#,
        "</row>",
    ));
    let workbook = parse(workbook(b"", &sheet));
    let cells = &workbook.sheets[0].cells;
    let text = |index: usize| match &cells[index].value {
        CellValue::String { v } => v.to_string(),
        other => panic!("expected text, got {other:?}"),
    };
    assert_eq!(text(0), "inl\u{fffd}ne <ok>");
    assert_eq!(text(1), "val\u{fffd}e");
    assert_eq!(cells[1].formula.as_deref(), Some("\"f\u{fffd}rmula\""));
    assert_eq!(text(2), "ru\u{fffd} two");
}

#[test]
fn an_unknown_entity_is_kept_as_written() {
    // Only the unknown entity stays as written; `&amp;` beside it resolves.
    let sst = b"<si><t>a &bogus; b &amp; c</t></si><si><t>x</t></si>";
    assert_eq!(texts(workbook(sst, SHARED_CELLS)), ["a &bogus; b & c", "x"]);
}

#[test]
fn cdata_text_is_kept_literally() {
    let sst = "<si><t><![CDATA[caf\u{e9} <raw> &amp;]]></t></si>\
               <si><r><t>a</t></r><r><t><![CDATA[<b>]]></t></r></si>";
    let sheet = concat!(
        r#"<row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c>"#,
        r#"<c r="C1" t="inlineStr"><is><t><![CDATA[x < y & z]]></t></is></c>"#,
        r#"<c r="D1" t="str"><f><![CDATA[A1&"<"]]></f><v><![CDATA[1 < 2]]></v></c>"#,
        r#"<c r="E1" t="s"><v><![CDATA[0]]></v></c></row>"#,
    );
    let workbook = parse(workbook(sst.as_bytes(), sheet.as_bytes()));
    let cells = &workbook.sheets[0].cells;
    let text = |index: usize| match &cells[index].value {
        CellValue::String { v } => v.to_string(),
        other => panic!("expected text, got {other:?}"),
    };
    assert_eq!(text(0), "caf\u{e9} <raw> &amp;");
    assert_eq!(text(1), "a<b>");
    assert_eq!(text(2), "x < y & z");
    assert_eq!(text(3), "1 < 2");
    assert_eq!(cells[3].formula.as_deref(), Some("A1&\"<\""));
    assert_eq!(text(4), "caf\u{e9} <raw> &amp;");
}

#[test]
fn phonetic_runs_are_not_part_of_the_cell_text() {
    // 東京 with its reading トウキョウ: the reading is a display aid stored in
    // `rPh`; the cell's value is the base text only.
    let sst = "<si><t>\u{6771}\u{4eac}</t>\
               <rPh sb=\"0\" eb=\"2\"><t>\u{30c8}\u{30a6}\u{30ad}\u{30e7}\u{30a6}</t></rPh>\
               <phoneticPr fontId=\"1\"/></si>\
               <si><r><t>\u{6771}</t></r><r><t>\u{4eac}</t></r>\
               <rPh sb=\"0\" eb=\"1\"><t>\u{30c8}\u{30a6}</t></rPh>\
               <rPh sb=\"1\" eb=\"2\"><t>\u{30ad}\u{30e7}\u{30a6}</t></rPh></si>";
    let sheet = "<row r=\"1\"><c r=\"A1\" t=\"s\"><v>0</v></c><c r=\"B1\" t=\"s\"><v>1</v></c>\
                 <c r=\"C1\" t=\"inlineStr\"><is><t>\u{6771}\u{4eac}</t>\
                 <rPh sb=\"0\" eb=\"2\"><t>\u{30c8}\u{30a6}\u{30ad}\u{30e7}\u{30a6}</t></rPh></is></c>\
                 <c r=\"D1\" t=\"inlineStr\"><is><t>after</t></is></c></row>";
    assert_eq!(
        texts(workbook(sst.as_bytes(), sheet.as_bytes())),
        [
            "\u{6771}\u{4eac}",
            "\u{6771}\u{4eac}",
            "\u{6771}\u{4eac}",
            "after"
        ]
    );
}

#[test]
fn whitespace_inside_text_elements_is_preserved() {
    let sst = b"<si><t xml:space=\"preserve\">  two leading, one trailing </t></si>\
                <si><r><t xml:space=\"preserve\">a </t></r>\n  <r><t xml:space=\"preserve\"> b</t></r></si>";
    let sheet = concat!(
        r#"<row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c>"#,
        "<c r=\"C1\" t=\"inlineStr\">\n <is>\n <t xml:space=\"preserve\"> x\ty\n</t>\n </is>\n</c></row>",
    );
    assert_eq!(
        texts(workbook(sst, sheet.as_bytes())),
        ["  two leading, one trailing ", "a  b", " x\ty\n"]
    );
}

#[test]
fn xstring_escapes_in_text_elements_are_decoded() {
    // ECMA-376 ST_Xstring: `_xHHHH_` is the UTF-16 code unit HHHH, and
    // `_x005F_` is a literal underscore that protects a following escape.
    let sst = concat!(
        "<si><t>line1_x000D__x000A_line2</t></si>",
        "<si><t>literal _x005F_x000D_ stays</t></si>",
        "<si><t>pair _xD83D__xDE00_ lone _xD83D_ low _xDE00_ end</t></si>",
        "<si><t>lower _x000d_ not hex _xZZZZ_ short _x41_ open _x0041</t></si>",
        "<si><r><t>A_x0042_</t></r><r><t>_x0043_D</t></r></si>",
    );
    let sheet = concat!(
        r#"<row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c>"#,
        r#"<c r="C1" t="s"><v>2</v></c><c r="D1" t="s"><v>3</v></c><c r="E1" t="s"><v>4</v></c>"#,
        r#"<c r="F1" t="inlineStr"><is><t>tab_x0009_bed</t></is></c></row>"#,
    );
    assert_eq!(
        texts(workbook(sst.as_bytes(), sheet.as_bytes())),
        [
            "line1\r\nline2",
            "literal _x000D_ stays",
            "pair \u{1F600} lone \u{fffd} low \u{fffd} end",
            "lower \r not hex _xZZZZ_ short _x41_ open _x0041",
            "ABCD",
            "tab\tbed",
        ]
    );
}

#[test]
fn an_empty_shared_string_item_keeps_later_indexes_aligned() {
    let sst = b"<si/><si><t/></si><si><t>third</t></si>";
    let sheet = br#"<row r="1"><c r="A1" t="s"><v>2</v></c><c r="B1" t="s"><v>0</v></c></row>"#;
    assert_eq!(
        cells(workbook(sst, sheet)),
        [(1, 1, "third".to_string()), (1, 2, String::new())]
    );
}

#[test]
fn rows_and_cells_without_references_take_the_next_position() {
    // `r` is optional on both `row` and `c`; a missing one means "the next".
    let sheet = concat!(
        r#"<row><c t="str"><v>a1</v></c><c t="str"><v>b1</v></c></row>"#,
        r#"<row/>"#,
        r#"<row><c/><c t="str"><v>b3</v></c><c r="E3" t="str"><v>e3</v></c><c t="str"><v>f3</v></c></row>"#,
        r#"<row r="7"><c t="str"><v>a7</v></c></row>"#,
        r#"<row><c t="str"><v>a8</v></c></row>"#,
    );
    assert_eq!(
        cells(workbook(b"", sheet.as_bytes())),
        [
            (1, 1, "a1".to_string()),
            (1, 2, "b1".to_string()),
            (3, 2, "b3".to_string()),
            (3, 5, "e3".to_string()),
            (3, 6, "f3".to_string()),
            (7, 1, "a7".to_string()),
            (8, 1, "a8".to_string()),
        ]
    );
}

#[test]
fn a_time_that_rounds_to_midnight_moves_to_the_next_day() {
    // 45000 is 2023-03-15. Half a millisecond before midnight rounds to the
    // next day's 00:00:00, not back to the start of the same day.
    let sheet = concat!(
        r#"<row r="1"><c r="A1" s="1"><v>45000.999999999</v></c>"#,
        r#"<c r="B1" s="1"><v>45000.5</v></c><c r="C1" s="1"><v>59.999999999</v></c></row>"#,
    );
    let workbook = parse(workbook(b"", sheet.as_bytes()));
    let iso: Vec<&str> = workbook.sheets[0]
        .cells
        .iter()
        .map(|cell| match &cell.value {
            CellValue::Date { iso, .. } => iso.as_str(),
            other => panic!("expected a date, got {other:?}"),
        })
        .collect();
    assert_eq!(
        iso,
        [
            "2023-03-16T00:00:00",
            "2023-03-15T12:00:00",
            // Serial 60 is Excel's fictitious 1900-02-29, which this parser
            // has always reported as the day before.
            "1900-02-28T00:00:00",
        ]
    );
}
