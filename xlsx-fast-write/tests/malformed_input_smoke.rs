//! A workbook spec comes from the caller of a binding, so it is untrusted:
//! a malformed one must be an error, never a panic, a stack overflow or an
//! allocation out of proportion to the spec. Whatever the writer does return
//! must be a workbook our own reader opens with the same number of sheets.

#[path = "../../tools/mutation_smoke.rs"]
mod smoke;

use xlsx_fast_core::{parse_xlsx_bytes, ParseOptions};
use xlsx_fast_write::Workbook;

#[global_allocator]
static ALLOCATOR: smoke::CappedAllocator = smoke::CappedAllocator;

/// Specs that between them use every cell type, format, option and the
/// characters the writer escapes.
fn seeds() -> Vec<Vec<u8>> {
    [
        r#"{"sheets":[{"name":"Summary","column_widths":{"0":12.0,"1":20.5,"16383":3},"freeze_rows":1,"rows":[[{"t":"s","v":"Page","fmt":"header"},{"t":"s","v":"Type","fmt":"bold"}],[{"t":"n","v":1},{"t":"s","v":"Digital"},{"t":"b","v":true},{"t":"d","v":"2024-01-31T10:00:00"},{"t":"f","v":"=SUM(A2:A2)"}],[],[{"v":"=1+1"},{"t":"n","v":-1.5e300},{"t":"n","v":"12"},{"t":"x","v":null}]]},{"name":"summary","rows":[[{"t":"s","v":" a<b>&\"c\"\r\n_x0041_\u0001 "}]]}]}"#,
        r#"{"sheets":[{"name":"a[b]:c*d?e/f\\g'","rows":[[{"t":"n","v":1e308},{"t":"n","v":123456789012345678901234567890},{"t":"b","v":0},{"t":"s","v":{"k":[1,2,{"z":null}]}}]]},{"name":"","column_widths":{"65535":1e99,"-1":2,"1":-4}},{"name":"A sheet name that is far longer than thirty-one characters"}]}"#,
        r#"{"sheets":[]}"#,
    ]
    .iter()
    .map(|spec| spec.as_bytes().to_vec())
    .collect()
}

/// Writes `spec` if it is valid and checks the result against the reader.
fn write_and_read_back(spec: &[u8]) {
    let Ok(text) = std::str::from_utf8(spec) else {
        return;
    };
    let declared = serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|value| value.get("sheets")?.as_array().map(Vec::len));
    let Ok(bytes) = Workbook::write_json_spec_to_bytes(text) else {
        return;
    };
    let workbook = parse_xlsx_bytes(bytes, ParseOptions::default())
        .expect("a written workbook reads back through xlsx-fast-core");
    let declared = declared.expect("a spec that was written has a sheet list");
    assert_eq!(workbook.sheets.len(), declared.max(1), "sheet count");
    let mut names: Vec<String> = workbook
        .sheets
        .iter()
        .map(|sheet| sheet.name.to_lowercase())
        .collect();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), workbook.sheets.len(), "sheet names are unique");
}

#[test]
fn the_seed_specs_are_written_and_read_back() {
    for seed in seeds() {
        let text = std::str::from_utf8(&seed).expect("seed is UTF-8");
        Workbook::write_json_spec_to_bytes(text).expect("seed spec is valid");
        write_and_read_back(&seed);
    }
}

#[test]
fn mutated_specs_never_panic_and_always_read_back() {
    smoke::assert_no_panics(
        "xlsx-fast-write spec",
        &seeds(),
        20_000,
        write_and_read_back,
    );
}

/// The builder API takes coordinates and numbers directly; drive it with
/// values taken from the mutated bytes, including the extremes of each type.
#[test]
fn arbitrary_builder_calls_never_panic() {
    let seed: Vec<u8> = (0u8..=255).collect();
    smoke::assert_no_panics("xlsx-fast-write builder", &[seed], 1_500, |bytes| {
        let mut workbook = Workbook::new();
        for chunk in bytes.chunks(12).take(16) {
            let at = |index: usize| chunk.get(index).copied().unwrap_or(0);
            let pick = |byte: u8| match byte % 4 {
                0 => u32::from(byte),
                1 => 1_048_575 - u32::from(byte % 2),
                2 => u32::MAX - u32::from(byte / 4),
                _ => u32::from(byte) << 12,
            };
            let row = pick(at(0));
            let col = pick(at(1)).min(u32::from(u16::MAX)) as u16;
            let number = f64::from_bits(u64::from_le_bytes([
                at(2),
                at(3),
                at(4),
                at(5),
                at(6),
                at(7),
                at(8),
                at(9),
            ]));
            let name = String::from_utf8_lossy(chunk).into_owned();
            let sheet = workbook.add_sheet(name.clone());
            sheet.write(row, col, number);
            sheet.write(row / 2, col / 2, name.as_str());
            sheet.write_formula(row / 3, col / 3, name.as_str());
            sheet.write_date(row / 4, col / 4, name.as_str());
            sheet.set_column_width(col, number);
            sheet.set_freeze_rows(row);
        }
        let mut buffer = std::io::Cursor::new(Vec::new());
        if workbook.write_to(&mut buffer).is_ok() {
            parse_xlsx_bytes(buffer.into_inner(), ParseOptions::default())
                .expect("a written workbook reads back through xlsx-fast-core");
        }
    });
}

/// Deep nesting in a cell value and a very wide or tall spec must be
/// rejected or written within the small stack and the allocation cap.
#[test]
fn deep_and_large_specs_are_bounded_on_a_small_stack() {
    let deep_value = smoke::nested(b"[", b"1", b"]", 100_000);
    let mut deep = br#"{"sheets":[{"name":"S","rows":[[{"t":"s","v":"#.to_vec();
    deep.extend_from_slice(&deep_value);
    deep.extend_from_slice(b"}]]}]}");
    let deep_sheets = smoke::nested(br#"{"sheets":["#, b"", b"]}", 50_000);

    let empty_rows = format!(
        r#"{{"sheets":[{{"name":"S","rows":[{}[{{"v":"last"}}]]}}]}}"#,
        "[],".repeat(2_000_000)
    );
    let many_sheets = format!(
        r#"{{"sheets":[{}{{"name":"x"}}]}}"#,
        r#"{"name":"x"},"#.repeat(20_000)
    );
    for spec in [
        deep,
        deep_sheets,
        empty_rows.into_bytes(),
        many_sheets.into_bytes(),
    ] {
        smoke::run_on_small_stack(move || {
            if let Ok(text) = std::str::from_utf8(&spec) {
                let _ = Workbook::write_json_spec_to_bytes(text);
            }
        });
    }
}
