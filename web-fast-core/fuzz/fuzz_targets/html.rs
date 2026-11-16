#![no_main]

use libfuzzer_sys::fuzz_target;
use web_fast_core::{parse_html, parse_html_bytes, ParseOptions};

const LABELS: &[&str] = &[
    "utf-8",
    "utf-16le",
    "utf-16be",
    "windows-1252",
    "shift_jis",
    "iso-2022-jp",
    "gb18030",
    "x-user-defined",
    "replacement",
    "no-such-encoding",
    "",
];

// Any bytes as an HTML page: sniffed, then under one encoding label and base
// URL chosen by the input's first byte, then as text when it is UTF-8.
fuzz_target!(|data: &[u8]| {
    let _ = parse_html_bytes(data, None, ParseOptions::default());
    let selector = usize::from(data.first().copied().unwrap_or(0));
    let base = [
        "https://example.org/a/b?c#d",
        "not a url",
        "data:text/plain,x",
        "",
    ][selector % 4];
    let labelled = ParseOptions {
        encoding_label: Some(LABELS[selector % LABELS.len()].to_owned()),
        ..ParseOptions::default()
    };
    let _ = parse_html_bytes(data, Some(base), labelled);
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = parse_html(text, Some(base), ParseOptions::default());
    }
});
