#![no_main]

use docx_fast_core::{parse_docx_bytes, ParseOptions};
use libfuzzer_sys::fuzz_target;

// Any bytes as a whole `.docx` file: the ZIP container first, then every part.
fuzz_target!(|data: &[u8]| {
    if let Ok(document) = parse_docx_bytes(data.to_vec(), ParseOptions::default()) {
        let _ = serde_json::to_string(&document);
    }
});
