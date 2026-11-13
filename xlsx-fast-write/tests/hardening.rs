//! What the writer does with input Excel cannot hold: every workbook it
//! returns must open, and anything it cannot write faithfully is an error
//! instead of a silently different workbook.

use std::io::{Cursor, Read};
use xlsx_fast_core::{parse_xlsx_bytes, CellValue as Read_, ParseOptions};
use xlsx_fast_write::{CellValue, Format, Workbook};

fn bytes_of(workbook: &Workbook) -> Result<Vec<u8>, String> {
    let mut buffer = Cursor::new(Vec::new());
    workbook
        .write_to(&mut buffer)
        .map_err(|error| error.to_string())?;
    Ok(buffer.into_inner())
}

fn part(bytes: &[u8], name: &str) -> String {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).expect("zip opens");
    let mut xml = String::new();
    archive
        .by_name(name)
        .expect("part exists")
        .read_to_string(&mut xml)
        .expect("part is UTF-8");
    xml
}

fn read(bytes: Vec<u8>) -> xlsx_fast_core::Workbook {
    parse_xlsx_bytes(
        bytes,
        ParseOptions {
            include_formulas: true,
            ..ParseOptions::default()
        },
    )
    .expect("written workbook reads back")
}

/// WHY: row and column numbers are written one-based, so the largest index
/// of each integer type used to overflow while the reference was formatted.
#[test]
fn the_largest_coordinates_are_an_error_not_a_panic() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("S");
    sheet.write(u32::MAX, u16::MAX, "far");
    sheet.set_column_width(u16::MAX, 10.0);
    sheet.set_freeze_rows(u32::MAX);
    let error = bytes_of(&workbook).expect_err("cell outside the grid");
    assert!(error.contains("outside"), "{error}");

    // A width or a frozen pane outside the grid carries no data: it is left
    // out and the workbook is still written.
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("S");
    sheet.write(0, 0, "x");
    sheet.set_column_width(u16::MAX, 10.0);
    sheet.set_freeze_rows(u32::MAX);
    let bytes = bytes_of(&workbook).expect("writes");
    let xml = part(&bytes, "xl/worksheets/sheet1.xml");
    assert!(!xml.contains("<col "), "{xml}");
    assert!(!xml.contains("<pane"), "{xml}");
    assert_eq!(read(bytes).sheets[0].cells.len(), 1);
}

/// WHY: Excel's grid ends at row 1,048,576 and column XFD (16,384). A cell
/// past either edge makes Excel report the file as corrupt.
#[test]
fn a_cell_past_the_last_row_or_column_is_an_error() {
    for (row, col) in [(1_048_576, 0), (0, 16_384)] {
        let mut workbook = Workbook::new();
        workbook.add_sheet("S").write(row, col, 1.0);
        assert!(bytes_of(&workbook).is_err(), "({row}, {col}) was written");
    }
    let mut workbook = Workbook::new();
    workbook.add_sheet("S").write(1_048_575, 16_383, 7.0);
    let bytes = bytes_of(&workbook).expect("the last cell of the grid is valid");
    assert!(part(&bytes, "xl/worksheets/sheet1.xml").contains("r=\"XFD1048576\""));
    let cell = &read(bytes).sheets[0].cells[0];
    assert_eq!((cell.row, cell.col), (1_048_576, 16_384));
}

/// WHY: `NaN` and `inf` are not numbers in SpreadsheetML; Excel refuses the
/// whole file. They are written as the `#NUM!` error Excel itself shows.
#[test]
fn numbers_excel_cannot_hold_become_num_errors() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("S");
    let values = [
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        1.5,
        -0.0,
        f64::MAX,
        f64::MIN_POSITIVE,
        5e-324,
    ];
    for (row, value) in (0u32..).zip(values) {
        sheet.write(row, 0, value);
    }
    let bytes = bytes_of(&workbook).expect("writes");
    let xml = part(&bytes, "xl/worksheets/sheet1.xml");
    assert!(!xml.contains("NaN") && !xml.contains("inf"), "{xml}");
    let cells = &read(bytes).sheets[0].cells;
    assert_eq!(cells.len(), values.len());
    for (cell, value) in cells.iter().zip(values) {
        match &cell.value {
            Read_::Error { v } => {
                assert!(!value.is_finite());
                assert_eq!(&**v, "#NUM!");
            }
            Read_::Number { v } => assert_eq!(v.to_bits(), value.to_bits()),
            other => panic!("{value}: {other:?}"),
        }
    }
}

/// WHY: a width is written into an attribute as a number; `NaN` there makes
/// the sheet unreadable, and Excel accepts 0 to 255 only.
#[test]
fn column_widths_excel_cannot_hold_are_dropped_or_clamped() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("S");
    sheet.write(0, 0, "x");
    sheet.set_column_width(0, f64::NAN);
    sheet.set_column_width(1, f64::INFINITY);
    sheet.set_column_width(2, -3.0);
    sheet.set_column_width(3, 1e9);
    sheet.set_column_width(4, 12.5);
    let bytes = bytes_of(&workbook).expect("writes");
    let xml = part(&bytes, "xl/worksheets/sheet1.xml");
    assert!(!xml.contains("NaN") && !xml.contains("inf"), "{xml}");
    assert!(
        !xml.contains("min=\"1\"") && !xml.contains("min=\"2\""),
        "{xml}"
    );
    assert!(
        xml.contains("<col min=\"3\" max=\"3\" width=\"0\""),
        "{xml}"
    );
    assert!(
        xml.contains("<col min=\"4\" max=\"4\" width=\"255\""),
        "{xml}"
    );
    assert!(
        xml.contains("<col min=\"5\" max=\"5\" width=\"12.5\""),
        "{xml}"
    );
    read(bytes);
}

/// WHY: Excel rejects a workbook whose sheet name is empty, longer than 31
/// characters, contains one of `[ ] : * ? / \`, starts or ends with an
/// apostrophe, is the reserved name `History`, or repeats another name
/// ignoring case.
#[test]
fn sheet_names_are_made_valid_and_unique() {
    let long = "x".repeat(40);
    let given = [
        "Data",
        "data",
        "DATA",
        "a[b]c:d*e?f/g\\h",
        "'quoted'",
        "",
        long.as_str(),
        long.as_str(),
        "History",
        "\u{1F600}".repeat(20).as_str().to_owned().leak(),
        "'",
        "Sheet6",
    ];
    let mut workbook = Workbook::new();
    let mut names = Vec::new();
    for name in given {
        let sheet = workbook.add_sheet(name);
        sheet.write(0, 0, "x");
        names.push(sheet.name().to_string());
    }
    assert_eq!(names[0], "Data");
    assert_eq!(names[3], "a_b_c_d_e_f_g_h");
    assert_eq!(names[4], "_quoted_");
    assert_eq!(names[6], "x".repeat(31));
    for (index, name) in names.iter().enumerate() {
        assert!(!name.is_empty(), "name {index} is empty");
        assert!(name.encode_utf16().count() <= 31, "{name:?} is too long");
        assert!(
            !name.contains(['[', ']', ':', '*', '?', '/', '\\']),
            "{name:?}"
        );
        assert!(!name.starts_with('\'') && !name.ends_with('\''), "{name:?}");
        assert!(!name.eq_ignore_ascii_case("history"), "{name:?}");
        for other in &names[..index] {
            assert_ne!(name.to_lowercase(), other.to_lowercase(), "duplicate");
        }
    }
    let read_names: Vec<String> = read(bytes_of(&workbook).expect("writes"))
        .sheets
        .into_iter()
        .map(|sheet| sheet.name)
        .collect();
    assert_eq!(read_names, names);
}

/// WHY: Excel cannot open a workbook that has no worksheet.
#[test]
fn an_empty_workbook_is_written_with_one_empty_sheet() {
    let workbook = Workbook::new();
    assert_eq!(workbook.sheet_count(), 0);
    let bytes = bytes_of(&workbook).expect("writes");
    let read = read(bytes);
    assert_eq!(read.sheets.len(), 1);
    assert_eq!(read.sheets[0].name, "Sheet1");
    assert!(read.sheets[0].cells.is_empty());

    let bytes = Workbook::write_json_spec_to_bytes(r#"{"sheets":[]}"#).expect("writes");
    assert_eq!(self::read(bytes).sheets.len(), 1);
}

/// WHY: a cell holds at most 32,767 characters. Excel repairs a longer one
/// by cutting it, so writing it would lose text without anyone being told.
#[test]
fn a_string_over_the_cell_limit_is_an_error() {
    let longest = "a".repeat(32_767);
    let mut workbook = Workbook::new();
    workbook.add_sheet("S").write(0, 0, longest.as_str());
    let cells = read(bytes_of(&workbook).expect("the limit itself is valid"))
        .sheets
        .remove(0)
        .cells;
    assert!(matches!(&cells[0].value, Read_::String { v } if **v == *longest));

    for text in ["a".repeat(32_768), "\u{1F600}".repeat(16_384)] {
        let mut workbook = Workbook::new();
        workbook.add_sheet("S").write(2, 1, text);
        let error = bytes_of(&workbook).expect_err("over the limit");
        assert!(error.contains("32767") && error.contains("B3"), "{error}");
    }
}

/// WHY: text from an untrusted source that starts with `=`, `+`, `-` or `@`
/// must stay text. Only [`Sheet::write_formula`] and the spec type `"f"`
/// create a formula.
#[test]
fn text_is_never_turned_into_a_formula() {
    let texts = [
        "=1+1",
        "=cmd|' /C calc'!A0",
        "+1+1",
        "-2+3",
        "@SUM(A1)",
        "=HYPERLINK(\"http://x\",\"y\")",
        "\t=1+1",
    ];
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("S");
    for (row, text) in (0u32..).zip(texts) {
        sheet.write(row, 0, text);
        sheet.write_with_format(row, 1, text, &Format::new().bold());
    }
    sheet.write_formula(0, 2, "=SUM(1,2)");
    sheet.write_formula(1, 2, "A1&\"<x>\"");
    let bytes = bytes_of(&workbook).expect("writes");
    let xml = part(&bytes, "xl/worksheets/sheet1.xml");
    assert_eq!(xml.matches("<f>").count(), 2, "{xml}");
    assert!(xml.contains("<f>SUM(1,2)</f>"), "{xml}");
    let cells = read(bytes).sheets.remove(0).cells;
    for cell in &cells {
        if cell.col == 3 {
            continue;
        }
        assert!(cell.formula.is_none(), "{cell:?}");
        let expected = texts[(cell.row - 1) as usize];
        assert!(matches!(&cell.value, Read_::String { v } if &**v == expected));
    }
    let formulas: Vec<_> = cells.iter().filter_map(|c| c.formula.clone()).collect();
    assert_eq!(formulas, ["SUM(1,2)", "A1&\"<x>\""]);

    // The JSON spec follows the same rule: "s" is text, "f" is a formula.
    let spec = r#"{"sheets":[{"name":"S","rows":[[{"t":"s","v":"=1+1"},{"v":"=2+2"},{"t":"f","v":"=3+3"}]]}]}"#;
    let bytes = Workbook::write_json_spec_to_bytes(spec).expect("writes");
    let xml = part(&bytes, "xl/worksheets/sheet1.xml");
    assert_eq!(xml.matches("<f>").count(), 1, "{xml}");
    assert!(xml.contains("<f>3+3</f>"), "{xml}");
}

/// WHY: a date is a number with a date format. Without the format a reader
/// sees 45322 instead of 2024-01-31.
#[test]
fn dates_read_back_as_dates() {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_sheet("S");
    assert!(sheet.write_date(0, 0, "2024-01-31"));
    assert!(sheet.write_date(1, 0, "2024-02-29T13:30:00"));
    assert!(sheet.write_date(2, 0, "1900-03-01"));
    assert!(!sheet.write_date(3, 0, "2023-02-29"));
    assert!(!sheet.write_date(3, 0, "not a date"));
    assert!(!sheet.write_date(3, 0, "1899-12-31"));
    assert!(!sheet.write_date(3, 0, "2024-01-31T25:00:00"));
    assert_eq!(xlsx_fast_write::date_serial(2024, 1, 31), Some(45_322.0));
    assert_eq!(xlsx_fast_write::date_serial(1900, 3, 1), Some(61.0));
    assert_eq!(xlsx_fast_write::date_serial(10_000, 1, 1), None);
    let cells = read(bytes_of(&workbook).expect("writes"))
        .sheets
        .remove(0)
        .cells;
    assert_eq!(cells.len(), 3);
    let isos: Vec<(String, f64)> = cells
        .iter()
        .map(|cell| match &cell.value {
            Read_::Date { iso, serial } => (iso.clone(), *serial),
            other => panic!("not a date: {other:?}"),
        })
        .collect();
    assert!(isos[0].0.starts_with("2024-01-31"), "{isos:?}");
    assert_eq!(isos[0].1, 45_322.0);
    assert!(isos[1].0.starts_with("2024-02-29T13:30"), "{isos:?}");
    assert!(isos[2].0.starts_with("1900-03-01"), "{isos:?}");

    let spec = r#"{"sheets":[{"name":"S","rows":[[{"t":"d","v":"2024-01-31"},{"t":"d","v":45322},{"t":"d","v":"soon"}]]}]}"#;
    let cells = read(Workbook::write_json_spec_to_bytes(spec).expect("writes"))
        .sheets
        .remove(0)
        .cells;
    assert!(matches!(&cells[0].value, Read_::Date { serial, .. } if *serial == 45_322.0));
    assert!(matches!(&cells[1].value, Read_::Date { serial, .. } if *serial == 45_322.0));
    assert!(matches!(&cells[2].value, Read_::String { v } if &**v == "soon"));
}

/// WHY: the spec indexes cells by position. A row longer than the grid used
/// to wrap around and overwrite column A; a number given as text became 0.
#[test]
fn a_spec_is_never_written_as_different_data() {
    let wide: Vec<String> = (0..65_537)
        .map(|index| format!(r#"{{"t":"n","v":{index}}}"#))
        .collect();
    let spec = format!(
        r#"{{"sheets":[{{"name":"S","rows":[[{}]]}}]}}"#,
        wide.join(",")
    );
    assert!(Workbook::write_json_spec_to_bytes(&spec).is_err());

    let spec =
        r#"{"sheets":[{"name":"S","column_widths":{"65535":9,"x":1,"0":"wide"},"rows":[]}]}"#;
    assert!(
        Workbook::write_json_spec_to_bytes(spec).is_err(),
        "width is not a number"
    );
    let spec = r#"{"sheets":[{"name":"S","column_widths":{"65535":9,"x":1},"rows":[[
        {"t":"n","v":"42.5"},{"t":"n","v":"abc"},{"t":"n","v":null},{"t":"n","v":7},
        {"t":"b","v":"true"},{"t":"b","v":1},{"t":"s","v":null},{"t":"s","v":12}]]}]}"#;
    let cells = read(Workbook::write_json_spec_to_bytes(spec).expect("writes"))
        .sheets
        .remove(0)
        .cells;
    let shown: Vec<String> = cells
        .iter()
        .map(|cell| match &cell.value {
            Read_::Number { v } => format!("{}:n:{v}", cell.col),
            Read_::String { v } => format!("{}:s:{v}", cell.col),
            Read_::Bool { v } => format!("{}:b:{v}", cell.col),
            other => format!("{}:{other:?}", cell.col),
        })
        .collect();
    assert_eq!(
        shown,
        ["1:n:42.5", "2:s:abc", "4:n:7", "5:s:true", "6:n:1", "8:s:12"]
    );
}

/// WHY: the package itself must be a sound ZIP for every other reader, and
/// each value must come back as written.
#[test]
fn a_mixed_workbook_passes_unzip_and_reads_back_identically() {
    let mut workbook = Workbook::new();
    for name in ["First", "Second & <third>"] {
        let sheet = workbook.add_sheet(name);
        sheet.set_freeze_rows(1);
        sheet.set_column_width(0, 20.0);
        for row in 0..200u32 {
            sheet.write(row, 0, format!("text {row} \u{e9}"));
            sheet.write(row, 1, f64::from(row) * 1.25);
            sheet.write(row, 2, row % 2 == 0);
            sheet.write(row, 3, CellValue::Empty);
        }
    }
    let bytes = bytes_of(&workbook).expect("writes");
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("mixed.xlsx");
    std::fs::write(&path, &bytes).expect("write file");
    // `unzip` is not installed everywhere (Windows runners); the read-back
    // below still checks the package there.
    if let Ok(output) = std::process::Command::new("unzip")
        .arg("-t")
        .arg(&path)
        .output()
    {
        assert!(
            output.status.success(),
            "unzip -t failed: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
    let read = read(bytes);
    assert_eq!(read.sheets.len(), 2);
    assert_eq!(read.sheets[1].name, "Second & <third>");
    for sheet in &read.sheets {
        assert_eq!(sheet.cells.len(), 600);
        for cell in &sheet.cells {
            let row = cell.row - 1;
            match (cell.col, &cell.value) {
                (1, Read_::String { v }) => assert_eq!(&**v, format!("text {row} \u{e9}")),
                (2, Read_::Number { v }) => assert_eq!(*v, f64::from(row) * 1.25),
                (3, Read_::Bool { v }) => assert_eq!(*v, row % 2 == 0),
                other => panic!("unexpected cell {other:?}"),
            }
        }
    }
}

/// WHY: a sheet name is an XML attribute, which cannot carry control
/// characters. Names that differ only in such characters used to be written
/// as one and the same name, and `Sheet::name` reported a name that was not
/// the one in the file.
#[test]
fn sheet_names_stay_unique_after_unwritable_characters_are_replaced() {
    let mut workbook = Workbook::new();
    let mut names = Vec::new();
    for requested in [
        "a\u{1}",
        "a\u{2}",
        "a\u{fffe}",
        "a\u{fffd}",
        "a\u{ffff}",
        "a\u{b}",
        "a\tb",
        "a\nb",
        "a\rb",
        "a",
    ] {
        names.push(workbook.add_sheet(requested).name().to_string());
    }
    let distinct: std::collections::HashSet<String> =
        names.iter().map(|name| name.to_lowercase()).collect();
    assert_eq!(distinct.len(), names.len(), "{names:?}");
    assert_eq!(names[0], "a\u{fffd}");
    let read_back: Vec<String> = read(bytes_of(&workbook).expect("writes"))
        .sheets
        .into_iter()
        .map(|sheet| sheet.name)
        .collect();
    assert_eq!(read_back, names);
}

/// WHY: a suffix is added to a name cut to make room for it, so long names
/// that share a prefix compete for the same suffixes. Each used to retry
/// every suffix already taken, which made a workbook of many such sheets
/// take minutes.
#[test]
fn many_long_names_sharing_a_prefix_are_made_unique_quickly() {
    const SHEETS: usize = 20_000;
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut workbook = Workbook::new();
        let mut names = std::collections::HashSet::new();
        for round in 0..2 {
            for index in 0..SHEETS {
                let requested = format!("{}{:04x}", "X".repeat(27), index);
                let name = workbook.add_sheet(requested).name().to_string();
                assert!(name.encode_utf16().count() <= 31, "{name:?}");
                assert!(
                    names.insert(name.to_lowercase()),
                    "{name:?} in round {round}"
                );
            }
        }
        // A sheet later named like a cut stem is still free to use.
        let stem = "X".repeat(27);
        assert_eq!(workbook.add_sheet(stem.clone()).name(), stem);
        let _ = sender.send(names.len());
    });
    let made = receiver
        .recv_timeout(std::time::Duration::from_secs(20))
        .expect("naming the sheets did not finish in time");
    assert_eq!(made, 2 * SHEETS);
}

/// WHY: a serial number given as a spec date used to get the date style
/// whatever it was, so -1 read back as 1899-12-30 and 2958466 as a date in
/// the year 10000. Outside the range `write_date` accepts it is a number.
#[test]
fn a_spec_date_serial_outside_the_date_range_is_a_plain_number() {
    let spec = r#"{"sheets":[{"name":"S","rows":[[
        {"t":"d","v":61},{"t":"d","v":2958465.75},{"t":"d","v":-1},{"t":"d","v":60},
        {"t":"d","v":60.999},{"t":"d","v":2958466},{"t":"d","v":1e300}]]}]}"#;
    let cells = read(Workbook::write_json_spec_to_bytes(spec).expect("writes"))
        .sheets
        .remove(0)
        .cells;
    let shown: Vec<String> = cells
        .iter()
        .map(|cell| match &cell.value {
            Read_::Date { serial, .. } => format!("date {serial}"),
            Read_::Number { v } if *v == 1e300 => "number 1e300".to_string(),
            Read_::Number { v } => format!("number {v}"),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(
        shown,
        [
            "date 61",
            "date 2958465.75",
            "number -1",
            "number 60",
            "number 60.999",
            "number 2958466",
            "number 1e300"
        ]
    );
}
