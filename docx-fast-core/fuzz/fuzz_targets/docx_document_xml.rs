#![no_main]

#[path = "../../../tools/fuzz_package.rs"]
mod package;

use docx_fast_core::{parse_docx_bytes, ParseOptions};
use libfuzzer_sys::fuzz_target;
use std::sync::OnceLock;

/// The seed document the smoke tests build, stored uncompressed.
const SEED: &[u8] = include_bytes!("../seeds/docx_container/stored.docx");

// The input replaces `word/document.xml` inside the valid seed package, so
// the fuzzer mutates WordprocessingML directly and not a ZIP checksum.
fuzz_target!(|data: &[u8]| {
    static PARTS: OnceLock<Vec<(String, Vec<u8>)>> = OnceLock::new();
    let parts = PARTS.get_or_init(|| package::parts(SEED));
    let bytes = package::with_part(parts, "word/document.xml", data);
    if let Ok(document) = parse_docx_bytes(bytes, ParseOptions::default()) {
        let _ = serde_json::to_string(&document);
    }
});
