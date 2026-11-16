#![no_main]

#[path = "../exercise.rs"]
mod exercise;

use email_fast_core::parse_mbox_reader;
use libfuzzer_sys::fuzz_target;

// Any bytes as an mbox file of several messages.
fuzz_target!(|data: &[u8]| {
    for options in exercise::option_sets() {
        let _ = parse_mbox_reader(std::io::Cursor::new(data), options, |email| {
            exercise::consume(&email);
            Ok(())
        });
    }
});
