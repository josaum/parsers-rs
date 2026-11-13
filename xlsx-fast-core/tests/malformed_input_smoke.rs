//! A malformed workbook must be rejected with an error or parsed as far as it
//! goes: never a panic, a stack overflow, or an allocation out of proportion
//! to the file.

#[path = "../../tools/mutation_smoke.rs"]
mod smoke;

use std::io::{Cursor, Write};
use xlsx_fast_core::{parse_xlsx_bytes, parse_xlsx_to_writer, CellValue, ParseOptions};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

#[global_allocator]
static ALLOCATOR: smoke::CappedAllocator = smoke::CappedAllocator;

const NS: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const REL_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

/// A small workbook that reaches every branch of the reader: shared, rich,
/// phonetic and inline strings, a custom and a built-in date style, every
/// cell type, formulas, and rows and cells with and without references.
fn seed_parts() -> Vec<(&'static str, Vec<u8>)> {
    let content_types = r#"<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
</Types>"#
        .to_string();
    let root_rels = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="{REL_NS}/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#
    );
    let workbook = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<workbook xmlns="{NS}" xmlns:r="{REL_NS}">
  <workbookPr date1904="false"/>
  <sheets>
    <sheet name="Data &amp; more" sheetId="1" r:id="rId1"/>
    <sheet name="Second" sheetId="2" r:id="rId4"/>
  </sheets>
  <definedNames><definedName name="Total">'Data &amp; more'!$B$2</definedName></definedNames>
</workbook>"#
    );
    let workbook_rels = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="{REL_NS}/worksheet" Target="worksheets/sheet1.xml"/>
  <Relationship Id="rId4" Type="{REL_NS}/worksheet" Target="/xl/worksheets/sheet2.xml"/>
  <Relationship Id="rId2" Type="{REL_NS}/sharedStrings" Target="sharedStrings.xml"/>
  <Relationship Id="rId3" Type="{REL_NS}/styles" Target="styles.xml"/>
</Relationships>"#
    );
    let shared_strings = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<sst xmlns=\"{NS}\" count=\"6\" uniqueCount=\"5\">\n  \
         <si><t>Name</t></si>\n  \
         <si><t xml:space=\"preserve\"> caf\u{e9} &amp; &lt;tea&gt; &#233; </t></si>\n  \
         <si><r><t>rich </t></r><r><rPr><b/><sz val=\"11\"/></rPr><t>text_x000D_</t></r></si>\n  \
         <si><t>\u{6771}\u{4eac}</t><rPh sb=\"0\" eb=\"2\"><t>\u{30c8}\u{30a6}</t></rPh><phoneticPr fontId=\"1\"/></si>\n  \
         <si/>\n  <si><t><![CDATA[a < b]]></t></si>\n</sst>"
    );
    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<styleSheet xmlns="{NS}">
  <numFmts count="1"><numFmt numFmtId="164" formatCode="yyyy\-mm\-dd\ hh:mm"/></numFmts>
  <cellXfs count="3"><xf numFmtId="0"/><xf numFmtId="14" applyNumberFormat="1"/><xf numFmtId="164"/></cellXfs>
</styleSheet>"#
    );
    let sheet1 = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="{NS}">
  <dimension ref="A1:F4"/>
  <sheetData>
    <row r="1" spans="1:6">
      <c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c><c r="C1" t="s"><v>2</v></c>
      <c r="D1" t="s"><v>3</v></c><c r="E1" t="s"><v>4</v></c><c r="F1" t="s"><v>5</v></c>
    </row>
    <row r="2">
      <c r="A2"><v>42.5</v></c><c r="B2" s="1"><v>45000.75</v></c><c r="C2" s="2"><v>59</v></c>
      <c r="D2" t="b"><v>1</v></c><c r="E2" t="e"><v>#DIV/0!</v></c>
      <c r="F2" t="str"><f>A2&amp;"x"</f><v>42.5x</v></c>
    </row>
    <row>
      <c t="inlineStr"><is><t>inline</t><rPh sb="0" eb="1"><t>x</t></rPh></is></c>
      <c s="1"/><c><f t="shared" ref="C3:C4" si="0">SUM(A2:A2)</f><v>-1.5e3</v></c>
      <c r="$E$3" t="inlineStr"><is><r><t>a</t></r><r><t>b_x0041_</t></r></is></c>
    </row>
    <row r="1048576"><c r="XFD1048576" s="1"><v>2958465</v></c></row>
  </sheetData>
  <mergeCells count="1"><mergeCell ref="A1:B1"/></mergeCells>
</worksheet>"#
    );
    let sheet2 = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="{NS}"><sheetData><row r="3"><c r="B3" t="s"><v>1</v></c></row></sheetData></worksheet>"#
    );
    vec![
        ("[Content_Types].xml", content_types.into_bytes()),
        ("_rels/.rels", root_rels.into_bytes()),
        ("xl/workbook.xml", workbook.into_bytes()),
        ("xl/_rels/workbook.xml.rels", workbook_rels.into_bytes()),
        ("xl/sharedStrings.xml", shared_strings.into_bytes()),
        ("xl/styles.xml", styles.into_bytes()),
        ("xl/worksheets/sheet1.xml", sheet1.into_bytes()),
        ("xl/worksheets/sheet2.xml", sheet2.into_bytes()),
    ]
}

/// Zips `parts`, with `replacement` standing in for the part it names.
fn package(
    parts: &[(&'static str, Vec<u8>)],
    replacement: Option<(&str, &[u8])>,
    method: CompressionMethod,
) -> Vec<u8> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(method);
    for (name, body) in parts {
        let body = match replacement {
            Some((replaced, bytes)) if replaced == *name => bytes,
            _ => body.as_slice(),
        };
        zip.start_file(*name, options).expect("start zip entry");
        zip.write_all(body).expect("write zip entry");
    }
    zip.finish().expect("finish zip").into_inner()
}

/// A workbook whose only sheet is `sheet_data` and whose shared strings are
/// `shared_strings` (both the inner XML of their container element).
fn workbook_with(shared_strings: &[u8], sheet_data: &[u8]) -> Vec<u8> {
    let mut parts = seed_parts();
    for (name, body) in &mut parts {
        match *name {
            "xl/sharedStrings.xml" => {
                *body = format!(r#"<sst xmlns="{NS}">"#).into_bytes();
                body.extend_from_slice(shared_strings);
                body.extend_from_slice(b"</sst>");
            }
            "xl/worksheets/sheet1.xml" => {
                *body = format!(r#"<worksheet xmlns="{NS}"><sheetData>"#).into_bytes();
                body.extend_from_slice(sheet_data);
                body.extend_from_slice(b"</sheetData></worksheet>");
            }
            _ => {}
        }
    }
    package(&parts, None, CompressionMethod::Stored)
}

fn all_options() -> [ParseOptions; 2] {
    [
        ParseOptions::default(),
        ParseOptions {
            decode_dates: false,
            include_formulas: true,
            raw_values: true,
            ..ParseOptions::default()
        },
    ]
}

/// Runs the in-memory entry point under both option sets and serializes what
/// parsed, since the bindings hand the result straight to a serializer.
fn parse_bytes(bytes: &[u8]) {
    for options in all_options() {
        if let Ok(workbook) = parse_xlsx_bytes(bytes.to_vec(), options) {
            let _ = serde_json::to_string(&workbook);
        }
    }
}

/// Runs the path-based entry points: the NDJSON writer and, when built, the
/// Arrow batch reader with serial and parallel sheet workers.
fn parse_file(bytes: &[u8], path: &std::path::Path) {
    std::fs::write(path, bytes).expect("write workbook");
    for options in all_options() {
        let mut sink = Vec::new();
        let _ = parse_xlsx_to_writer(path, &mut sink, options);
    }
    #[cfg(feature = "arrow")]
    for parallel_sheets in [0, 2] {
        use xlsx_fast_core::arrow::{parse_xlsx_to_arrow_batches, ArrowOptions};
        let arrow_options = ArrowOptions {
            batch_size: 4,
            parallel_sheets,
            ..ArrowOptions::default()
        };
        let _ =
            parse_xlsx_to_arrow_batches(path, ParseOptions::default(), arrow_options, |_| Ok(()));
    }
}

#[test]
fn the_seed_workbook_parses() {
    let bytes = package(&seed_parts(), None, CompressionMethod::Deflated);
    let workbook = parse_xlsx_bytes(bytes, ParseOptions::default()).expect("seed parses");
    assert_eq!(workbook.sheets.len(), 2);
    assert_eq!(workbook.sheets[0].cells.len(), 16);
}

#[test]
fn mutated_parts_never_panic() {
    let parts = seed_parts();
    for (name, body) in &parts {
        smoke::assert_no_panics(
            &format!("xlsx-fast-core part {name}"),
            std::slice::from_ref(body),
            2_000,
            |mutated| {
                parse_bytes(&package(
                    &parts,
                    Some((name, mutated)),
                    CompressionMethod::Stored,
                ));
            },
        );
    }
}

#[test]
fn mutated_parts_never_panic_through_the_file_readers() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("mutated.xlsx");
    let parts = seed_parts();
    for (name, body) in &parts {
        smoke::assert_no_panics(
            &format!("xlsx-fast-core file part {name}"),
            std::slice::from_ref(body),
            250,
            |mutated| {
                let bytes = package(&parts, Some((name, mutated)), CompressionMethod::Stored);
                parse_file(&bytes, &path);
            },
        );
    }
}

#[test]
fn mutated_containers_never_panic() {
    let parts = seed_parts();
    let seeds = [
        package(&parts, None, CompressionMethod::Stored),
        package(&parts, None, CompressionMethod::Deflated),
    ];
    smoke::assert_no_panics("xlsx-fast-core container", &seeds, 20_000, parse_bytes);
}

#[test]
fn deeply_nested_elements_are_bounded_on_a_small_stack() {
    const DEPTH: usize = 100_000;
    let cases: Vec<Vec<u8>> = vec![
        workbook_with(&smoke::nested(b"<si>", b"<t>x</t>", b"</si>", DEPTH), b""),
        workbook_with(
            &smoke::nested(b"<si><r>", b"<t>x</t>", b"</r></si>", DEPTH),
            b"",
        ),
        workbook_with(
            &[
                b"<si>".as_slice(),
                &smoke::nested(b"<rPh>", b"<t>x</t>", b"</rPh>", DEPTH),
                b"</si>",
            ]
            .concat(),
            br#"<row r="1"><c r="A1" t="s"><v>0</v></c></row>"#,
        ),
        workbook_with(
            b"",
            &smoke::nested(b"<row>", b"<c><v>1</v></c>", b"</row>", DEPTH),
        ),
        workbook_with(
            b"",
            &smoke::nested(b"<row r=\"1\"><c>", b"<v>1</v>", b"</c></row>", DEPTH),
        ),
        workbook_with(
            b"",
            &[
                b"<row r=\"1\"><c t=\"inlineStr\">".as_slice(),
                &smoke::nested(b"<is><r>", b"<t>x</t>", b"</r></is>", DEPTH),
                b"</c></row>",
            ]
            .concat(),
        ),
        workbook_with(
            b"",
            &smoke::nested(b"<sheetData><x:y>", b"", b"</x:y></sheetData>", DEPTH),
        ),
    ];
    for bytes in cases {
        smoke::run_on_small_stack(move || parse_bytes(&bytes));
    }
}

/// A date-styled cell whose serial is far outside the calendar is a number
/// that cannot be shown as a date, not a reason to stop the process.
#[test]
fn a_date_cell_far_outside_the_calendar_stays_a_number() {
    let sheet = concat!(
        r#"<row r="1"><c r="A1" s="1"><v>1e300</v></c><c r="B1" s="1"><v>-1e300</v></c>"#,
        r#"<c r="C1" s="1"><v>9999999945000</v></c><c r="D1" s="1"><v>110000000000</v></c>"#,
        r#"<c r="E1" s="1"><v>9223372036854775807</v></c><c r="F1" s="1"><v>45000</v></c></row>"#,
    );
    let workbook = parse_xlsx_bytes(
        workbook_with(b"", sheet.as_bytes()),
        ParseOptions::default(),
    )
    .expect("workbook parses");
    let cells = &workbook.sheets[0].cells;
    assert_eq!(cells.len(), 6);
    for cell in &cells[..5] {
        assert!(
            matches!(cell.value, CellValue::Number { .. }),
            "expected a plain number, got {:?}",
            cell.value
        );
    }
    assert!(matches!(&cells[5].value, CellValue::Date { iso, .. } if iso == "2023-03-15T00:00:00"));
}

/// One long shared string referenced by many cells is one allocation. A
/// 1 MB file must not be able to ask for the product of the two.
#[test]
fn many_cells_sharing_one_long_string_do_not_multiply_it() {
    const STRING_BYTES: usize = 256 * 1024;
    const CELLS: usize = 20_000;
    let mut shared = b"<si><t>".to_vec();
    shared.resize(shared.len() + STRING_BYTES, b'a');
    shared.extend_from_slice(b"</t></si>");
    let mut sheet = b"<row r=\"1\">".to_vec();
    for _ in 0..CELLS {
        sheet.extend_from_slice(b"<c t=\"s\"><v>0</v></c>");
    }
    sheet.extend_from_slice(b"</row>");

    let workbook = parse_xlsx_bytes(workbook_with(&shared, &sheet), ParseOptions::default())
        .expect("workbook parses");
    let cells = &workbook.sheets[0].cells;
    assert_eq!(cells.len(), CELLS);
    let (CellValue::String { v: first }, CellValue::String { v: last }) =
        (&cells[0].value, &cells[CELLS - 1].value)
    else {
        panic!("expected shared-string cells");
    };
    assert_eq!(first.len(), STRING_BYTES);
    assert!(std::sync::Arc::ptr_eq(first, last));
}

/// The Arrow reader copies each cell's text into the batch, so a batch is
/// closed by the text it holds as well as by its row count.
#[cfg(feature = "arrow")]
#[test]
fn arrow_batches_are_closed_by_text_size_as_well_as_row_count() {
    use xlsx_fast_core::arrow::{parse_xlsx_to_arrow_batches, ArrowOptions};

    const STRING_BYTES: usize = 256 * 1024;
    const CELLS: usize = 20_000;
    let mut shared = b"<si><t>".to_vec();
    shared.resize(shared.len() + STRING_BYTES, b'a');
    shared.extend_from_slice(b"</t></si>");
    let mut sheet = b"<row r=\"1\">".to_vec();
    for _ in 0..CELLS {
        sheet.extend_from_slice(b"<c t=\"s\"><v>0</v></c>");
    }
    sheet.extend_from_slice(b"</row>");
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("wide.xlsx");
    std::fs::write(&path, workbook_with(&shared, &sheet)).expect("write workbook");

    let mut rows = 0;
    let mut batches = 0;
    parse_xlsx_to_arrow_batches(
        &path,
        ParseOptions::default(),
        ArrowOptions::default(),
        |batch| {
            rows += batch.num_rows();
            batches += 1;
            Ok(())
        },
    )
    .expect("workbook parses");
    // The seed's second sheet contributes one more cell.
    assert_eq!(rows, CELLS + 1);
    assert!(batches > 2, "expected text-sized batches, got {batches}");
}

/// quick-xml 0.36 checks each attribute name against all earlier ones
/// (RUSTSEC-2026-0194), so this is quadratic until the dependency moves. The
/// time is printed, not asserted: the test only pins that it finishes.
#[test]
fn a_start_tag_with_twenty_thousand_attributes_is_parsed() {
    let mut cell = b"<row r=\"1\"><c".to_vec();
    for index in 0..20_000 {
        cell.extend_from_slice(format!(" a{index}=\"v\"").as_bytes());
    }
    cell.extend_from_slice(b" r=\"B1\" t=\"str\"><v>kept</v></c></row>");
    let bytes = workbook_with(b"", &cell);

    let started = std::time::Instant::now();
    let workbook = parse_xlsx_bytes(bytes, ParseOptions::default()).expect("workbook parses");
    eprintln!(
        "xlsx-fast-core: start tag with 20,000 attributes parsed in {:?}",
        started.elapsed()
    );
    let cell = &workbook.sheets[0].cells[0];
    assert_eq!((cell.row, cell.col), (1, 2));
    assert!(matches!(&cell.value, CellValue::String { v } if v.as_ref() == "kept"));
}

/// The seeds above are also the starting corpus of the fuzz targets in
/// `fuzz/`; `WRITE_FUZZ_SEEDS=1` writes them out again after a seed changes.
#[test]
fn the_fuzz_corpus_holds_these_seeds() {
    let parts = seed_parts();
    let sheet = parts
        .iter()
        .find(|(name, _)| *name == "xl/worksheets/sheet1.xml")
        .expect("the seed has a first worksheet")
        .1
        .clone();
    smoke::fuzz_seeds(
        env!("CARGO_MANIFEST_DIR"),
        &[
            (
                "xlsx_container",
                "stored.xlsx",
                package(&parts, None, CompressionMethod::Stored),
            ),
            (
                "xlsx_container",
                "deflated.xlsx",
                package(&parts, None, CompressionMethod::Deflated),
            ),
            ("xlsx_sheet_xml", "sheet1.xml", sheet),
        ],
    );
}
