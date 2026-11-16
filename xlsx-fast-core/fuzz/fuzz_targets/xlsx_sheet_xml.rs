#![no_main]

#[path = "../../../tools/fuzz_package.rs"]
mod package;

use libfuzzer_sys::fuzz_target;
use std::sync::OnceLock;
use xlsx_fast_core::{parse_xlsx_bytes, ParseOptions};

/// The seed workbook the smoke tests build, stored uncompressed.
const SEED: &[u8] = include_bytes!("../seeds/xlsx_container/stored.xlsx");

// The input replaces the first worksheet inside the valid seed package, so
// the fuzzer mutates SpreadsheetML (cell references, numbers, dates) directly.
fuzz_target!(|data: &[u8]| {
    static PARTS: OnceLock<Vec<(String, Vec<u8>)>> = OnceLock::new();
    let parts = PARTS.get_or_init(|| package::parts(SEED));
    let bytes = package::with_part(parts, "xl/worksheets/sheet1.xml", data);
    let options = ParseOptions {
        include_formulas: true,
        ..ParseOptions::default()
    };
    if let Ok(workbook) = parse_xlsx_bytes(bytes, options) {
        let _ = serde_json::to_string(&workbook);
    }
});
