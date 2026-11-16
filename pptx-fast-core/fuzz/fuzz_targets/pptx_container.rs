#![no_main]

use libfuzzer_sys::fuzz_target;
use pptx_fast_core::{parse_pptx_bytes, ParseOptions};

// Any bytes as a whole `.pptx` file: the ZIP container first, then every part.
fuzz_target!(|data: &[u8]| {
    if let Ok(deck) = parse_pptx_bytes(data.to_vec(), ParseOptions::default()) {
        let _ = serde_json::to_string(&deck);
    }
});
