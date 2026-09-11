use std::fs::File;
use std::path::Path;
use tempfile::tempdir;
use xlsx_fast_core::{parse_xlsx, CellValue, ParseOptions};
use zip::write::FileOptions;
use zip::{CompressionMethod, ZipWriter};

#[cfg(feature = "arrow")]
use xlsx_fast_core::arrow::{
    parse_xlsx_to_arrow_batches, parse_xlsx_to_arrow_ipc, parse_xlsx_to_arrow_ipc_bytes,
    ArrowOptions,
};

fn write_part(writer: &mut ZipWriter<File>, path: &str, contents: &str) {
    let options = FileOptions::default().compression_method(CompressionMethod::Stored);
    writer.start_file(path, options).unwrap();
    std::io::Write::write_all(writer, contents.as_bytes()).unwrap();
}

fn write_bytes_part(writer: &mut ZipWriter<File>, path: &str, contents: &[u8]) {
    let options = FileOptions::default().compression_method(CompressionMethod::Stored);
    writer.start_file(path, options).unwrap();
    std::io::Write::write_all(writer, contents).unwrap();
}

fn build_xlsx(path: &Path) {
    let file = File::create(path).unwrap();
    let mut zip = ZipWriter::new(file);

    write_part(
        &mut zip,
        "_rels/.rels",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#,
    );
    write_part(
        &mut zip,
        "xl/workbook.xml",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <workbookPr date1904="false"/>
  <sheets>
    <sheet name="Sheet1" sheetId="1" r:id="rId1"/>
  </sheets>
</workbook>"#,
    );
    write_part(
        &mut zip,
        "xl/_rels/workbook.xml.rels",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/>
</Relationships>"#,
    );
    write_part(
        &mut zip,
        "xl/sharedStrings.xml",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="1" uniqueCount="1">
  <si><t>Hello XLSX</t></si>
</sst>"#,
    );
    write_part(
        &mut zip,
        "xl/worksheets/sheet1.xml",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData>
    <row r="1">
      <c r="A1" t="s"><v>0</v></c>
      <c r="B1"><v>42</v></c>
      <c r="C1"><f>SUM(B1,B1)</f><v>84</v></c>
    </row>
  </sheetData>
</worksheet>"#,
    );

    zip.finish().unwrap();
}

fn build_xlsx_with_drawing_and_chart_sheet(path: &Path) {
    let file = File::create(path).unwrap();
    let mut zip = ZipWriter::new(file);

    write_part(
        &mut zip,
        "_rels/.rels",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#,
    );
    write_part(
        &mut zip,
        "xl/workbook.xml",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets>
    <sheet name="Sheet1" sheetId="1" r:id="rId1"/>
    <sheet name="Charts" sheetId="2" r:id="rId2"/>
  </sheets>
</workbook>"#,
    );
    write_part(
        &mut zip,
        "xl/_rels/workbook.xml.rels",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chartsheet" Target="chartsheets/sheet2.xml"/>
  <Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/>
</Relationships>"#,
    );
    write_part(
        &mut zip,
        "xl/sharedStrings.xml",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="1" uniqueCount="1">
  <si><t>Edge XLSX</t></si>
</sst>"#,
    );
    write_part(
        &mut zip,
        "xl/worksheets/sheet1.xml",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheetData>
    <row r="1">
      <c r="A1" t="s"><v>0</v></c>
    </row>
  </sheetData>
  <drawing r:id="rId10"/>
</worksheet>"#,
    );
    write_part(
        &mut zip,
        "xl/worksheets/_rels/sheet1.xml.rels",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId10" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/>
</Relationships>"#,
    );
    write_part(
        &mut zip,
        "xl/drawings/drawing1.xml",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <xdr:twoCellAnchor>
    <xdr:from><xdr:col>0</xdr:col><xdr:row>0</xdr:row></xdr:from>
    <xdr:to><xdr:col>2</xdr:col><xdr:row>4</xdr:row></xdr:to>
    <xdr:pic>
      <xdr:nvPicPr><xdr:cNvPr id="1" name="Image 1"/></xdr:nvPicPr>
      <xdr:blipFill><a:blip r:embed="rIdImg1"/></xdr:blipFill>
    </xdr:pic>
  </xdr:twoCellAnchor>
  <xdr:twoCellAnchor>
    <xdr:from><xdr:col>4</xdr:col><xdr:row>1</xdr:row></xdr:from>
    <xdr:to><xdr:col>8</xdr:col><xdr:row>10</xdr:row></xdr:to>
    <xdr:graphicFrame>
      <a:graphic>
        <a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/chart">
          <c:chart xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" r:id="rIdChart1"/>
        </a:graphicData>
      </a:graphic>
    </xdr:graphicFrame>
  </xdr:twoCellAnchor>
</xdr:wsDr>"#,
    );
    write_part(
        &mut zip,
        "xl/drawings/_rels/drawing1.xml.rels",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rIdImg1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/image1.png"/>
  <Relationship Id="rIdChart1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart1.xml"/>
</Relationships>"#,
    );
    write_bytes_part(&mut zip, "xl/media/image1.png", b"\x89PNG\r\n\x1a\nedge");
    write_part(
        &mut zip,
        "xl/charts/chart1.xml",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart">
  <c:chart/>
</c:chartSpace>"#,
    );
    write_part(
        &mut zip,
        "xl/chartsheets/sheet2.xml",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<chartsheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetViews/>
</chartsheet>"#,
    );

    zip.finish().unwrap();
}

#[cfg(feature = "arrow")]
fn build_two_sheet_budget_xlsx(path: &Path) -> usize {
    let workbook = r#"<?xml version="1.0" encoding="UTF-8"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets>
    <sheet name="One" sheetId="1" r:id="rId1"/>
    <sheet name="Two" sheetId="2" r:id="rId2"/>
  </sheets>
</workbook>"#;
    let workbook_rels = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.xml"/>
</Relationships>"#;
    let sheet_one = r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData>
</worksheet>"#;
    let sheet_two = r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData><row r="1"><c r="A1"><v>2</v></c></row></sheetData>
</worksheet>"#;

    let file = File::create(path).unwrap();
    let mut zip = ZipWriter::new(file);
    write_part(&mut zip, "xl/workbook.xml", workbook);
    write_part(&mut zip, "xl/_rels/workbook.xml.rels", workbook_rels);
    write_part(&mut zip, "xl/worksheets/sheet1.xml", sheet_one);
    write_part(&mut zip, "xl/worksheets/sheet2.xml", sheet_two);
    zip.finish().unwrap();

    workbook.len() + workbook_rels.len() + sheet_one.len().max(sheet_two.len())
}

#[test]
fn parses_minimal_xlsx_workbook() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("sample.xlsx");
    build_xlsx(&path);

    let workbook = parse_xlsx(
        &path,
        ParseOptions {
            include_formulas: true,
            ..ParseOptions::default()
        },
    )
    .unwrap();

    assert_eq!(workbook.sheets.len(), 1);
    let sheet = &workbook.sheets[0];
    assert_eq!(sheet.name, "Sheet1");
    assert_eq!(sheet.cells.len(), 3);

    match &sheet.cells[0].value {
        CellValue::String { v } => assert_eq!(v.as_ref(), "Hello XLSX"),
        other => panic!("expected shared string cell, got {other:?}"),
    }
    match sheet.cells[1].value {
        CellValue::Number { v } => assert_eq!(v, 42.0),
        ref other => panic!("expected numeric cell, got {other:?}"),
    }
    assert_eq!(sheet.cells[2].formula.as_deref(), Some("SUM(B1,B1)"));
    match sheet.cells[2].value {
        CellValue::Number { v } => assert_eq!(v, 84.0),
        ref other => panic!("expected formula cached numeric cell, got {other:?}"),
    }
}

#[test]
fn parses_workbook_with_drawing_image_and_chart_sheet() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("edge.xlsx");
    build_xlsx_with_drawing_and_chart_sheet(&path);

    let workbook = parse_xlsx(&path, ParseOptions::default()).unwrap();

    assert_eq!(workbook.sheets.len(), 2);
    assert_eq!(workbook.sheets[0].name, "Sheet1");
    assert_eq!(workbook.sheets[0].cells.len(), 1);
    match &workbook.sheets[0].cells[0].value {
        CellValue::String { v } => assert_eq!(v.as_ref(), "Edge XLSX"),
        other => panic!("expected shared string cell, got {other:?}"),
    }
    assert_eq!(workbook.sheets[1].name, "Charts");
    assert!(workbook.sheets[1].cells.is_empty());
}

#[cfg(feature = "arrow")]
#[test]
fn parallel_sheet_workers_share_one_aggregate_inflate_budget() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("parallel-budget.xlsx");
    let max_inflate = build_two_sheet_budget_xlsx(&path);
    let options = ParseOptions {
        max_inflate,
        ..ParseOptions::default()
    };
    let arrow_options = ArrowOptions {
        parallel_sheets: 2,
        ..ArrowOptions::default()
    };

    let err = parse_xlsx_to_arrow_batches(&path, options, arrow_options, |_| Ok(()))
        .expect_err("parallel workers must not multiply max_inflate");
    assert!(format!("{err:#}").contains("max_inflate"), "{err:#}");
}

#[cfg(feature = "arrow")]
#[test]
fn in_memory_arrow_ipc_is_readable() {
    use arrow_ipc::reader::StreamReader;
    use std::io::Cursor;

    let dir = tempdir().unwrap();
    let path = dir.path().join("in-memory-ipc.xlsx");
    build_xlsx(&path);

    let ipc = parse_xlsx_to_arrow_ipc_bytes(&path, ParseOptions::default(), ArrowOptions::default())
        .expect("encode XLSX IPC");
    let rows = StreamReader::try_new(Cursor::new(ipc), None)
        .expect("read XLSX IPC")
        .map(|batch| batch.expect("valid batch").num_rows())
        .sum::<usize>();
    assert_eq!(rows, 3);
}

/// The streaming variant writes the same bytes to a file and reports how many,
/// so a caller can hand the size on without a second stat.
#[cfg(feature = "arrow")]
#[test]
fn streamed_arrow_ipc_file_matches_the_in_memory_encoding() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("streamed-ipc.xlsx");
    let out_path = dir.path().join("cells.arrow");
    build_xlsx(&path);

    let written = parse_xlsx_to_arrow_ipc(
        &path,
        &out_path,
        ParseOptions::default(),
        ArrowOptions::default(),
    )
    .expect("stream XLSX IPC to disk");

    let on_disk = std::fs::read(&out_path).expect("read streamed IPC");
    assert_eq!(written, on_disk.len());
    assert_eq!(
        on_disk,
        parse_xlsx_to_arrow_ipc_bytes(&path, ParseOptions::default(), ArrowOptions::default())
            .expect("encode XLSX IPC in memory"),
        "the file and in-memory encodings must be byte-identical"
    );
}

const XLSX_OPTIONAL_CASES: &[(&str, &str, &str)] = &[
    (
        "shared strings",
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings",
        "sharedStrings.xml",
    ),
    (
        "styles",
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles",
        "styles.xml",
    ),
];

fn build_xlsx_with_optional_part(
    path: &Path,
    relationship_type: &str,
    target: &str,
    optional_part: Option<&str>,
) -> usize {
    let workbook = r#"<?xml version="1.0" encoding="UTF-8"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"
          xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets><sheet name="Sheet1" sheetId="1" r:id="rIdSheet"/></sheets>
</workbook>"#;
    let workbook_rels = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rIdSheet" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
  <Relationship Id="rIdOptional" Type="{relationship_type}" Target="{target}"/>
</Relationships>"#
    );
    let sheet = r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData><row r="1"><c r="A1"><v>42</v></c></row></sheetData>
</worksheet>"#;

    let file = File::create(path).unwrap();
    let mut zip = ZipWriter::new(file);
    write_part(&mut zip, "xl/workbook.xml", workbook);
    write_part(&mut zip, "xl/_rels/workbook.xml.rels", &workbook_rels);
    write_part(&mut zip, "xl/worksheets/sheet1.xml", sheet);
    if let Some(optional_part) = optional_part {
        write_part(&mut zip, &format!("xl/{target}"), optional_part);
    }
    zip.finish().unwrap();

    workbook.len() + workbook_rels.len() + sheet.len() + 16
}

#[test]
fn absent_optional_xlsx_tables_are_omitted_in_row_path() {
    for &(name, relationship_type, target) in XLSX_OPTIONAL_CASES {
        let dir = tempdir().unwrap();
        let path = dir.path().join(format!("absent-{name}.xlsx"));
        build_xlsx_with_optional_part(&path, relationship_type, target, None);

        let workbook = parse_xlsx(&path, ParseOptions::default())
            .unwrap_or_else(|error| panic!("absent optional {name} must be omitted: {error:#}"));
        assert_eq!(workbook.sheets[0].cells.len(), 1);
    }
}

#[test]
fn present_optional_xlsx_tables_cannot_hide_budget_failures_in_row_path() {
    let oversized = "x".repeat(4_096);
    for &(name, relationship_type, target) in XLSX_OPTIONAL_CASES {
        let dir = tempdir().unwrap();
        let path = dir.path().join(format!("over-budget-{name}.xlsx"));
        let max_inflate =
            build_xlsx_with_optional_part(&path, relationship_type, target, Some(&oversized));

        let error = parse_xlsx(
            &path,
            ParseOptions {
                max_inflate,
                ..ParseOptions::default()
            },
        )
        .expect_err("a present optional table must propagate its read failure");
        assert!(
            format!("{error:#}").contains("max_inflate"),
            "{name} returned the wrong error: {error:#}"
        );
    }
}

#[cfg(feature = "arrow")]
#[test]
fn absent_optional_xlsx_tables_are_omitted_in_arrow_path() {
    for &(name, relationship_type, target) in XLSX_OPTIONAL_CASES {
        let dir = tempdir().unwrap();
        let path = dir.path().join(format!("arrow-absent-{name}.xlsx"));
        build_xlsx_with_optional_part(&path, relationship_type, target, None);

        let mut rows = 0;
        parse_xlsx_to_arrow_batches(
            &path,
            ParseOptions::default(),
            ArrowOptions::default(),
            |batch| {
                rows += batch.num_rows();
                Ok(())
            },
        )
        .unwrap_or_else(|error| panic!("absent optional {name} must be omitted: {error:#}"));
        assert_eq!(rows, 1);
    }
}

#[cfg(feature = "arrow")]
#[test]
fn present_optional_xlsx_tables_cannot_hide_budget_failures_in_arrow_path() {
    let oversized = "x".repeat(4_096);
    for &(name, relationship_type, target) in XLSX_OPTIONAL_CASES {
        let dir = tempdir().unwrap();
        let path = dir.path().join(format!("arrow-over-budget-{name}.xlsx"));
        let max_inflate =
            build_xlsx_with_optional_part(&path, relationship_type, target, Some(&oversized));

        let error = parse_xlsx_to_arrow_batches(
            &path,
            ParseOptions {
                max_inflate,
                ..ParseOptions::default()
            },
            ArrowOptions::default(),
            |_| Ok(()),
        )
        .expect_err("a present optional table must propagate its Arrow-path read failure");
        assert!(
            format!("{error:#}").contains("max_inflate"),
            "{name} returned the wrong error: {error:#}"
        );
    }
}

#[test]
fn present_malformed_optional_xlsx_tables_fail_loudly() {
    for &(name, relationship_type, target) in XLSX_OPTIONAL_CASES {
        let dir = tempdir().unwrap();
        let path = dir.path().join(format!("malformed-{name}.xlsx"));
        build_xlsx_with_optional_part(&path, relationship_type, target, Some("<"));

        let result = parse_xlsx(&path, ParseOptions::default());
        assert!(
            result.is_err(),
            "present malformed optional {name} was ignored"
        );
    }
}
