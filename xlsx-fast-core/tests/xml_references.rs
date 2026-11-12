//! Entity and character references (`&amp;`, `&#233;`, `&#x1F600;`) must
//! resolve the same way on every path that collects text or reads an
//! attribute: shared strings, rich-text runs, inline strings, `<v>`, `<f>`,
//! and the attributes of the workbook, the styles and the sheet.
//!
//! The XML reader hands a reference over as its own event, separate from the
//! text around it, so each path is checked with the same inputs.

use std::io::{Cursor, Write};
use xlsx_fast_core::{parse_xlsx_bytes, CellValue, ParseOptions, Workbook};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

const ROOT_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#;

const WORKBOOK_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/>
  <Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>"#;

const NS: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const REL_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

/// The parts of a one-sheet workbook a test can vary.
struct Parts<'a> {
    /// Value of the sheet's `name` attribute, written between double quotes.
    sheet_name: &'a str,
    /// Inner XML of `<styleSheet>`.
    styles: &'a str,
    /// Inner XML of `<sst>`.
    shared_strings: &'a str,
    /// Inner XML of `<sheetData>`.
    sheet_data: &'a str,
}

impl Default for Parts<'_> {
    fn default() -> Self {
        Self {
            sheet_name: "Sheet1",
            styles: r#"<cellXfs count="1"><xf numFmtId="0"/></cellXfs>"#,
            shared_strings: "",
            sheet_data: "",
        }
    }
}

fn archive(parts: &Parts<'_>) -> Vec<u8> {
    let workbook = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><workbook xmlns="{NS}" xmlns:r="{REL_NS}"><sheets><sheet name="{}" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
        parts.sheet_name
    );
    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><styleSheet xmlns="{NS}">{}</styleSheet>"#,
        parts.styles
    );
    let shared_strings = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><sst xmlns="{NS}">{}</sst>"#,
        parts.shared_strings
    );
    let sheet = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="{NS}"><sheetData>{}</sheetData></worksheet>"#,
        parts.sheet_data
    );

    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    for (name, body) in [
        ("_rels/.rels", ROOT_RELS),
        ("xl/workbook.xml", workbook.as_str()),
        ("xl/_rels/workbook.xml.rels", WORKBOOK_RELS),
        ("xl/styles.xml", styles.as_str()),
        ("xl/sharedStrings.xml", shared_strings.as_str()),
        ("xl/worksheets/sheet1.xml", sheet.as_str()),
    ] {
        zip.start_file(name, options).unwrap();
        zip.write_all(body.as_bytes()).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

fn parse(parts: &Parts<'_>) -> Result<Workbook, String> {
    parse_xlsx_bytes(
        archive(parts),
        ParseOptions {
            include_formulas: true,
            ..ParseOptions::default()
        },
    )
    .map_err(|err| format!("{err:#}"))
}

/// The cell at `index` rendered as text; a non-text value is rendered with
/// `Debug` so a test that expects text fails loudly.
fn text(workbook: &Workbook, index: usize) -> String {
    match &workbook.sheets[0].cells[index].value {
        CellValue::String { v } => v.to_string(),
        other => format!("{other:?}"),
    }
}

/// What each text path reads for one piece of `<t>`, `<v>` or `<f>` content.
#[derive(Debug, PartialEq)]
struct Read {
    shared: String,
    rich_run: String,
    inline: String,
    value: String,
    formula: String,
    phonetic_base: String,
}

impl Read {
    /// Every path read the same text.
    fn all(text: &str) -> Self {
        Self {
            shared: text.to_string(),
            rich_run: format!("[{text}]"),
            inline: text.to_string(),
            value: text.to_string(),
            formula: text.to_string(),
            phonetic_base: "base".to_string(),
        }
    }
}

/// Reads `content` through every text path of the parser.
fn read_everywhere(content: &str) -> Read {
    let shared_strings = format!(
        "<si><t>{content}</t></si>\
         <si><r><t>[</t></r><r><rPr><b/></rPr><t>{content}</t></r><r><t>]</t></r></si>\
         <si><t>base</t><rPh sb=\"0\" eb=\"1\"><t>{content}</t></rPh></si>"
    );
    let sheet_data = format!(
        "<row r=\"1\"><c r=\"A1\" t=\"s\"><v>0</v></c><c r=\"B1\" t=\"s\"><v>1</v></c>\
         <c r=\"C1\" t=\"inlineStr\"><is><t>{content}</t></is></c>\
         <c r=\"D1\" t=\"str\"><f>{content}</f><v>{content}</v></c>\
         <c r=\"E1\" t=\"s\"><v>2</v></c></row>"
    );
    let workbook = parse(&Parts {
        shared_strings: &shared_strings,
        sheet_data: &sheet_data,
        ..Parts::default()
    })
    .expect("workbook parses");
    Read {
        shared: text(&workbook, 0),
        rich_run: text(&workbook, 1),
        inline: text(&workbook, 2),
        value: text(&workbook, 3),
        formula: workbook.sheets[0].cells[3]
            .formula
            .as_deref()
            .unwrap_or("<no formula>")
            .to_string(),
        phonetic_base: text(&workbook, 4),
    }
}

#[test]
fn predefined_entities_resolve_on_every_text_path() {
    assert_eq!(read_everywhere("a&amp;b"), Read::all("a&b"));
    assert_eq!(read_everywhere("a&lt;b&gt;c"), Read::all("a<b>c"));
    assert_eq!(read_everywhere("&quot;q&apos;"), Read::all("\"q'"));
    assert_eq!(read_everywhere("&amp;"), Read::all("&"));
    assert_eq!(read_everywhere("&amp;&amp;&lt;"), Read::all("&&<"));
    // A reference never resolves twice.
    assert_eq!(read_everywhere("&amp;lt;"), Read::all("&lt;"));
}

#[test]
fn character_references_resolve_on_every_text_path() {
    assert_eq!(read_everywhere("caf&#233;"), Read::all("caf\u{e9}"));
    assert_eq!(read_everywhere("&#xE9;t&#xe9;"), Read::all("\u{e9}t\u{e9}"));
    assert_eq!(read_everywhere("&#x1F600;"), Read::all("\u{1F600}"));
    assert_eq!(read_everywhere("l1&#13;&#10;l2"), Read::all("l1\r\nl2"));
    assert_eq!(read_everywhere("&#38;amp;"), Read::all("&amp;"));
}

#[test]
fn spaces_around_a_reference_are_kept() {
    assert_eq!(read_everywhere(" a &amp; b "), Read::all(" a & b "));
    assert_eq!(read_everywhere(" &lt; "), Read::all(" < "));
    assert_eq!(read_everywhere("a &#233; b"), Read::all("a \u{e9} b"));
    assert_eq!(read_everywhere("\n&amp;\n"), Read::all("\n&\n"));
}

#[test]
fn a_reference_next_to_a_cdata_section_resolves_and_cdata_stays_literal() {
    assert_eq!(
        read_everywhere("x &amp; <![CDATA[<raw> &amp;]]> &lt;y"),
        Read::all("x & <raw> &amp; <y")
    );
    assert_eq!(
        read_everywhere("&lt;<![CDATA[&lt;]]>&lt;"),
        Read::all("<&lt;<")
    );
}

#[test]
fn an_unknown_entity_is_kept_as_written_on_every_text_path() {
    assert_eq!(read_everywhere("&foo;"), Read::all("&foo;"));
    assert_eq!(read_everywhere("a &foo; b"), Read::all("a &foo; b"));
}

#[test]
fn an_unknown_entity_does_not_stop_its_neighbours_resolving() {
    assert_eq!(
        read_everywhere("a &foo; b &amp; c &#233;"),
        Read::all("a &foo; b &amp; c &#233;")
    );
}

#[test]
fn an_invalid_character_reference_is_kept_as_written_on_every_text_path() {
    assert_eq!(read_everywhere("&#0;"), Read::all("&#0;"));
    assert_eq!(read_everywhere("&#xD800;"), Read::all("&#xD800;"));
    assert_eq!(read_everywhere("&#x110000;"), Read::all("&#x110000;"));
    assert_eq!(read_everywhere("&#abc;"), Read::all("&#abc;"));
    assert_eq!(read_everywhere("&#;"), Read::all("&#;"));
}

#[test]
fn an_invalid_character_reference_does_not_stop_its_neighbours_resolving() {
    assert_eq!(read_everywhere("a&#0;b &lt; c"), Read::all("a&#0;b &lt; c"));
}

#[test]
fn a_lone_ampersand_is_kept_and_does_not_fail_the_workbook() {
    assert_eq!(read_everywhere("AT&T"), Read::all("AT&T"));
    assert_eq!(read_everywhere("a & b"), Read::all("a & b"));
    assert_eq!(read_everywhere("tail &"), Read::all("tail &"));
    assert_eq!(read_everywhere("a & b &amp; c"), Read::all("a & b &amp; c"));
}

/// `_xHHHH_` escapes are decoded on the whole text of one `<t>` element after
/// its references are resolved, so a reference can complete an escape but an
/// unresolved one cannot. `<v>` and `<f>` are not `ST_Xstring` and never
/// decode escapes.
#[test]
fn xstring_escapes_are_decoded_after_references_resolve() {
    let read = |content: &str| {
        let read = read_everywhere(content);
        assert_eq!(read.shared, read.inline, "shared and inline agree");
        assert_eq!(read.rich_run, format!("[{}]", read.shared));
        assert_eq!(read.value, read.formula, "value and formula agree");
        (read.shared, read.value)
    };
    let both = |text: &str, raw: &str| (text.to_string(), raw.to_string());

    assert_eq!(read("_x0041_&amp;_x0042_"), both("A&B", "_x0041_&_x0042_"));
    assert_eq!(read("_x00&#52;1_"), both("A", "_x0041_"));
    assert_eq!(read("&#95;x0041_"), both("A", "_x0041_"));
    assert_eq!(read("_x0041&#95;"), both("A", "_x0041_"));
    assert_eq!(read("_x00<![CDATA[41]]>_"), both("A", "_x0041_"));
    assert_eq!(read("_x00&foo;41_"), both("_x00&foo;41_", "_x00&foo;41_"));
    // The escaped underscore protects the escape that follows it.
    assert_eq!(read("&#95;x005F_x0041_"), both("_x0041_", "_x005F_x0041_"));
    // An escape decodes once: its output is never rescanned.
    assert_eq!(read("_x005F_&#120;0041_"), both("_x0041_", "_x005F_x0041_"));
}

#[test]
fn references_in_skipped_cells_and_rows_emit_nothing() {
    // `!!` is not a cell reference and row 2000000 is past the last row a
    // worksheet has: both are skipped, whatever their text contains.
    let workbook = parse(&Parts {
        sheet_data: "<row r=\"1\"><c r=\"!!\" t=\"str\"><v>a &amp; b &foo; &#0;</v></c>\
                     <c r=\"B1\" t=\"str\"><v>kept &amp; read</v></c></row>\
                     <row r=\"2000000\"><c t=\"str\"><v>&amp; &lt;</v></c>\
                     <c t=\"inlineStr\"><is><t>&#233;</t></is></c></row>\
                     <row r=\"3\"><c r=\"A3\" t=\"str\"><v>&lt;after&gt;</v></c></row>",
        ..Parts::default()
    })
    .expect("workbook parses");
    let cells: Vec<(u32, u32, String)> = workbook.sheets[0]
        .cells
        .iter()
        .enumerate()
        .map(|(index, cell)| (cell.row, cell.col, text(&workbook, index)))
        .collect();
    assert_eq!(
        cells,
        [
            (1, 2, "kept & read".to_string()),
            (3, 1, "<after>".to_string())
        ]
    );
}

#[test]
fn a_reference_in_a_numeric_or_index_value_is_resolved_before_it_is_read() {
    // `&#49;` is the digit 1: the shared-string index and the number are read
    // from the resolved text.
    let workbook = parse(&Parts {
        shared_strings: "<si><t>zero</t></si><si><t>one</t></si>",
        sheet_data: "<row r=\"1\"><c r=\"A1\" t=\"s\"><v>&#49;</v></c>\
                     <c r=\"B1\"><v>&#49;2</v></c></row>",
        ..Parts::default()
    })
    .expect("workbook parses");
    assert_eq!(text(&workbook, 0), "one");
    assert!(
        matches!(workbook.sheets[0].cells[1].value, CellValue::Number { v } if v == 12.0),
        "got {:?}",
        workbook.sheets[0].cells[1].value
    );
}

fn sheet_name(written: &str) -> Result<String, String> {
    parse(&Parts {
        sheet_name: written,
        ..Parts::default()
    })
    .map(|workbook| workbook.sheets[0].name.clone())
}

#[test]
fn references_in_a_sheet_name_resolve() {
    let name = |written: &str| sheet_name(written).expect("workbook parses");
    assert_eq!(name("a&amp;b"), "a&b");
    assert_eq!(name("a&lt;b&gt;c"), "a<b>c");
    assert_eq!(name("&quot;q&apos;"), "\"q'");
    assert_eq!(name("caf&#233;"), "caf\u{e9}");
    assert_eq!(name("&#x1F600;"), "\u{1F600}");
    assert_eq!(name(" a &amp; b "), " a & b ");
    assert_eq!(name("&amp;lt;"), "&lt;");
}

#[test]
fn an_unreadable_reference_in_a_sheet_name_fails_the_workbook() {
    for written in ["a &foo; b", "a&#0;b", "AT&T"] {
        assert!(sheet_name(written).is_err(), "{written:?} is rejected");
    }
}

#[test]
fn literal_whitespace_in_an_attribute_value_is_kept_as_written() {
    // A tab, a line feed and a carriage return typed directly into the value.
    assert_eq!(
        sheet_name("a\tb\nc\rd\r\ne").expect("workbook parses"),
        "a\tb\nc\rd\r\ne"
    );
    // The same characters written as character references.
    assert_eq!(
        sheet_name("a&#9;b&#10;c&#13;d").expect("workbook parses"),
        "a\tb\nc\rd"
    );
}

#[test]
fn references_in_sheet_attributes_resolve() {
    // `&#66;2` is B2, `&#115;` is the shared-string type `s`, `&#49;` is the
    // date style 1, and the second row is numbered `&#51;` (3).
    let workbook = parse(&Parts {
        styles: "<numFmts count=\"1\"><numFmt numFmtId=\"1&#54;4\" formatCode=\"yyyy&quot;-&quot;mm\"/></numFmts>\
                 <cellXfs count=\"2\"><xf numFmtId=\"0\"/><xf numFmtId=\"16&#52;\"/></cellXfs>",
        shared_strings: "<si><t>shared</t></si>",
        sheet_data: "<row r=\"1\"><c r=\"&#66;1\" t=\"&#115;\"><v>0</v></c>\
                     <c r=\"C1\" s=\"&#49;\"><v>45000</v></c></row>\
                     <row r=\"&#51;\"><c t=\"str\"><v>third</v></c><c r=\"&#68;3\"/><c t=\"str\"><v>e3</v></c></row>",
        ..Parts::default()
    })
    .expect("workbook parses");
    let cells: Vec<(u32, u32, String)> = workbook.sheets[0]
        .cells
        .iter()
        .enumerate()
        .map(|(index, cell)| (cell.row, cell.col, text(&workbook, index)))
        .collect();
    assert_eq!(cells[0], (1, 2, "shared".to_string()));
    assert_eq!((cells[1].0, cells[1].1), (1, 3));
    assert!(
        matches!(workbook.sheets[0].cells[1].value, CellValue::Date { .. }),
        "got {:?}",
        workbook.sheets[0].cells[1].value
    );
    assert_eq!(cells[2], (3, 1, "third".to_string()));
    assert_eq!(cells[3], (3, 5, "e3".to_string()));
}

#[test]
fn text_outside_cells_never_fails_the_workbook() {
    // References and a lone ampersand between elements, where no text is
    // collected, must not turn a readable workbook into an error.
    let workbook = parse(&Parts {
        styles: "&amp; &foo; & <cellXfs count=\"1\"><xf numFmtId=\"0\"/></cellXfs> & ",
        shared_strings: "&amp; & <si>&lt; & <t>kept</t> &foo; </si> &#0; ",
        sheet_data:
            " &amp; & <row r=\"1\"> &foo; <c r=\"A1\" t=\"s\"> & <v>0</v> &lt; </c> &#0; </row> & ",
        ..Parts::default()
    })
    .expect("workbook parses");
    assert_eq!(text(&workbook, 0), "kept");
    assert_eq!(workbook.sheets[0].cells.len(), 1);
}
