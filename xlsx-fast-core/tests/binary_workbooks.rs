use std::path::PathBuf;
use xlsx_fast_core::{
    parse_excel, parse_excel_bytes, parse_excel_to_writer, CellValue, ParseOptions,
};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[test]
fn binary_workbooks_match_file_and_byte_entry_points() {
    for name in [
        "date.xls",
        "date.xlsb",
        "date_1904.xls",
        "date_1904.xlsb",
        "sheet_names_order.xls",
        "sheet_names_order.xlsb",
    ] {
        let path = fixture(name);
        let file =
            parse_excel(&path, ParseOptions::default()).unwrap_or_else(|e| panic!("{name}: {e:#}"));
        let bytes =
            parse_excel_bytes(std::fs::read(&path).unwrap(), ParseOptions::default()).unwrap();
        assert_eq!(
            serde_json::to_value(&file).unwrap(),
            serde_json::to_value(bytes).unwrap(),
            "{name}"
        );
        assert!(!file.sheets.is_empty());
        if name.starts_with("sheet_names_order") {
            assert_eq!(
                file.sheets
                    .iter()
                    .map(|sheet| sheet.name.clone())
                    .collect::<Vec<_>>(),
                (1..=8).map(|i| format!("Sheet{i}")).collect::<Vec<_>>()
            );
        } else {
            assert!(file.sheets.iter().any(|sheet| !sheet.cells.is_empty()));
        }
    }
}

#[test]
fn binary_dates_and_raw_options_are_respected() {
    for name in ["date.xls", "date.xlsb", "date_1904.xls", "date_1904.xlsb"] {
        let decoded = parse_excel(fixture(name), ParseOptions::default()).unwrap();
        assert!(
            matches!(&decoded.sheets[0].cells[0].value, CellValue::Date {iso, serial}
            if iso == "2021-01-01T00:00:00" && *serial == if name.contains("1904") {42735.0} else {44197.0})
        );
        assert!(
            matches!(
                decoded.sheets[0]
                    .cells
                    .iter()
                    .find(|cell| cell.row == 3 && cell.col == 1)
                    .unwrap()
                    .value,
                CellValue::Number { .. }
            ),
            "elapsed durations must not become calendar dates"
        );
        let date = decoded
            .sheets
            .iter()
            .flat_map(|sheet| &sheet.cells)
            .find(|cell| matches!(cell.value, CellValue::Date { .. }))
            .expect("fixture must include a date");
        let raw = parse_excel(
            fixture(name),
            ParseOptions {
                raw_values: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(raw
            .sheets
            .iter()
            .flat_map(|sheet| &sheet.cells)
            .any(|cell| cell.row == date.row
                && cell.col == date.col
                && matches!(cell.value, CellValue::Raw { .. })));
        let numbers = parse_excel(
            fixture(name),
            ParseOptions {
                decode_dates: false,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!numbers
            .sheets
            .iter()
            .flat_map(|sheet| &sheet.cells)
            .any(|cell| matches!(cell.value, CellValue::Date { .. })));
    }
}

#[test]
fn binary_formula_extraction_is_opt_in() {
    for name in ["xls_formula.xls", "formula_values.xlsb"] {
        let without = parse_excel(fixture(name), ParseOptions::default()).unwrap();
        assert!(without
            .sheets
            .iter()
            .flat_map(|sheet| &sheet.cells)
            .all(|cell| cell.formula.is_none()));
        let with = parse_excel(
            fixture(name),
            ParseOptions {
                include_formulas: true,
                ..Default::default()
            },
        )
        .unwrap_or_else(|e| panic!("{name}: {e:#}"));
        assert!(with
            .sheets
            .iter()
            .flat_map(|sheet| &sheet.cells)
            .any(|cell| cell.formula.as_ref().is_some_and(|f| !f.is_empty())));
    }
}

#[test]
fn preserves_hidden_worksheets_and_skips_chart_tabs() {
    for name in ["any_sheets.xls", "any_sheets.xlsb"] {
        let book = parse_excel(fixture(name), ParseOptions::default())
            .unwrap_or_else(|e| panic!("{name}: {e:#}"));
        assert_eq!(
            book.sheets
                .iter()
                .map(|sheet| sheet.name.as_str())
                .collect::<Vec<_>>(),
            ["Visible", "Hidden", "VeryHidden"]
        );
    }
}

#[test]
fn content_detection_does_not_depend_on_filename_extension() {
    let directory = tempfile::tempdir().unwrap();
    for name in ["date.xls", "date.xlsb"] {
        let renamed = directory.path().join("upload.data");
        std::fs::copy(fixture(name), &renamed).unwrap();
        let book = parse_excel(&renamed, ParseOptions::default()).unwrap();
        assert_eq!(book.sheets[0].cells.len(), 6);
    }
}

#[test]
fn binary_ndjson_uses_existing_cell_contract() {
    for name in ["date.xls", "date.xlsb"] {
        let book = parse_excel(fixture(name), ParseOptions::default()).unwrap();
        let mut output = Vec::new();
        parse_excel_to_writer(fixture(name), &mut output, ParseOptions::default()).unwrap();
        let records: Vec<serde_json::Value> = std::str::from_utf8(&output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            records
                .iter()
                .filter(|record| record["kind"] == "sheet")
                .count(),
            book.sheets.len()
        );
        assert_eq!(
            records
                .iter()
                .filter(|record| record["kind"] == "cell")
                .count(),
            book.sheets
                .iter()
                .map(|sheet| sheet.cells.len())
                .sum::<usize>()
        );
    }
}

#[test]
fn rejects_encrypted_truncated_and_over_budget_workbooks() {
    for name in [
        "password_protected.xls",
        "password_protected.xlsb",
        "too_small.xls",
    ] {
        assert!(
            parse_excel(fixture(name), ParseOptions::default()).is_err(),
            "{name}"
        );
    }
    for name in ["date.xls", "date.xlsb"] {
        assert!(parse_excel(
            fixture(name),
            ParseOptions {
                max_inflate: 16,
                ..Default::default()
            }
        )
        .is_err());
    }
}

#[cfg(feature = "arrow")]
#[test]
fn binary_arrow_matches_cell_counts_and_applies_filters() {
    use xlsx_fast_core::arrow::{parse_excel_to_arrow_batches, ArrowOptions, RangeFilter};
    for name in ["date.xls", "date.xlsb"] {
        let book = parse_excel(fixture(name), ParseOptions::default()).unwrap();
        let sheet = &book.sheets[0];
        let mut count = 0;
        let options = ArrowOptions {
            batch_size: 2,
            sheet_filter: Some(vec![sheet.name.clone()]),
            range_filter: Some(vec![RangeFilter {
                row_start: 1,
                row_end: 2,
                col_start: 1,
                col_end: 2,
            }]),
            ..Default::default()
        };
        parse_excel_to_arrow_batches(fixture(name), ParseOptions::default(), options, |batch| {
            assert_eq!(batch.schema(), xlsx_fast_core::arrow::schema());
            assert!(batch.num_rows() <= 2);
            count += batch.num_rows();
            Ok(())
        })
        .unwrap();
        assert_eq!(
            count,
            sheet
                .cells
                .iter()
                .filter(|cell| cell.row <= 2 && cell.col <= 2)
                .count()
        );
    }
}
