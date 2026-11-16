#![no_main]

#[path = "../exercise.rs"]
mod exercise;

use email_fast_core::parse_eml_bytes;
use libfuzzer_sys::fuzz_target;

// Any bytes as an RFC 5322 / MIME message.
fuzz_target!(|data: &[u8]| {
    for options in exercise::option_sets() {
        if let Ok(email) = parse_eml_bytes(data, options) {
            exercise::consume(&email);
        }
    }
});
