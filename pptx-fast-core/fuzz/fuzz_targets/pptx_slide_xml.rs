#![no_main]

#[path = "../../../tools/fuzz_package.rs"]
mod package;

use libfuzzer_sys::fuzz_target;
use pptx_fast_core::{parse_pptx_bytes, ParseOptions};
use std::sync::OnceLock;

/// The seed deck the smoke tests build, stored uncompressed.
const SEED: &[u8] = include_bytes!("../seeds/pptx_container/stored.pptx");

// The input replaces the first slide inside the valid seed package, so the
// fuzzer mutates PresentationML directly and not a ZIP checksum.
fuzz_target!(|data: &[u8]| {
    static PARTS: OnceLock<Vec<(String, Vec<u8>)>> = OnceLock::new();
    let parts = PARTS.get_or_init(|| package::parts(SEED));
    let bytes = package::with_part(parts, "ppt/slides/slide1.xml", data);
    if let Ok(deck) = parse_pptx_bytes(bytes, ParseOptions::default()) {
        let _ = serde_json::to_string(&deck);
    }
});
