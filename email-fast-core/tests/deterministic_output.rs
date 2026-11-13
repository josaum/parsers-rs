//! The same message must give the same bytes every time.
//!
//! Raw headers are held in a `HashMap`, and every `HashMap` gets a different
//! seed, so two parses in one process iterate the same names in different
//! orders (the first test shows that this holds here). JSON and NDJSON must
//! not follow that order.

use std::collections::HashMap;

use email_fast_core::{parse_email_to_ndjson, parse_eml_bytes, ParseOptions};

const RUNS: usize = 20;
const HEADERS: usize = 40;

fn message() -> Vec<u8> {
    let mut message =
        String::from("From: Ana <ana@example.com>\r\nTo: bruno@example.com\r\nSubject: Order\r\n");
    for index in 0..HEADERS {
        message.push_str(&format!("X-Trace-{index}: hop {index}\r\n"));
    }
    message.push_str("Content-Type: text/plain; charset=utf-8\r\n\r\nBody\r\n");
    message.into_bytes()
}

fn options() -> ParseOptions {
    ParseOptions {
        include_raw_headers: true,
        ..ParseOptions::default()
    }
}

fn assert_all_equal(outputs: &[Vec<u8>], what: &str) {
    assert_eq!(outputs.len(), RUNS);
    assert!(!outputs[0].is_empty(), "{what} is empty");
    for (run, output) in outputs.iter().enumerate() {
        assert!(
            output == &outputs[0],
            "{what} of run {run} differs from run 0"
        );
    }
}

/// The premise of this file: maps with the same keys iterate differently
/// within one process, so repeating a parse does exercise the ordering.
#[test]
fn hash_maps_in_one_process_iterate_in_different_orders() {
    let orders: Vec<Vec<usize>> = (0..RUNS)
        .map(|_| {
            let map: HashMap<usize, ()> = (0..HEADERS).map(|key| (key, ())).collect();
            map.into_keys().collect()
        })
        .collect();
    assert!(orders.iter().any(|order| order != &orders[0]));
}

#[test]
fn json_is_the_same_on_every_parse() {
    let bytes = message();
    let parsed = parse_eml_bytes(&bytes, options()).unwrap();
    assert!(parsed
        .headers
        .as_ref()
        .is_some_and(|headers| headers.len() >= HEADERS));

    let outputs: Vec<Vec<u8>> = (0..RUNS)
        .map(|_| serde_json::to_vec(&parse_eml_bytes(&bytes, options()).unwrap()).unwrap())
        .collect();
    assert_all_equal(&outputs, "JSON");
}

#[test]
fn ndjson_is_the_same_on_every_parse() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("input.eml");
    std::fs::write(&path, message()).unwrap();
    let outputs: Vec<Vec<u8>> = (0..RUNS)
        .map(|run| {
            let out = dir.path().join(format!("out{run}.ndjson"));
            parse_email_to_ndjson(&path, &out, options()).unwrap();
            std::fs::read(out).unwrap()
        })
        .collect();
    assert_all_equal(&outputs, "NDJSON");
}
