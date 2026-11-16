#![no_main]

use libfuzzer_sys::fuzz_target;
use xlsx_fast_core::{parse_xlsx_bytes, ParseOptions};

// Any bytes as a whole `.xlsx` file, under the default options and with
// formulas and raw values switched on.
fuzz_target!(|data: &[u8]| {
    let options = [
        ParseOptions::default(),
        ParseOptions {
            decode_dates: false,
            include_formulas: true,
            raw_values: true,
            ..ParseOptions::default()
        },
    ];
    for options in options {
        if let Ok(workbook) = parse_xlsx_bytes(data.to_vec(), options) {
            let _ = serde_json::to_string(&workbook);
        }
    }
});
