#![no_main]

#[path = "../exercise.rs"]
mod exercise;

use email_fast_core::parse_msg_bytes;
use libfuzzer_sys::fuzz_target;

// Any bytes as an Outlook `.msg` file (a compound file binary container).
fuzz_target!(|data: &[u8]| {
    for options in exercise::option_sets() {
        if let Ok(email) = parse_msg_bytes(data, options) {
            exercise::consume(&email);
        }
    }
});
