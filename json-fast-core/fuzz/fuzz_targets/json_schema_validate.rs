#![no_main]

use json_fast_core::schema;
use libfuzzer_sys::fuzz_target;

/// The document and the schema the smoke tests use.
const DOCUMENT: &str = include_str!("../seeds/json_parse/document.json");
const SCHEMA: &str = include_str!("../seeds/json_schema_validate/schema.json");

// The input as the document under the seed schema, then as the schema over
// the seed document.
fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let _ = schema::validate_str(&text, SCHEMA);
    let _ = schema::validate_str(DOCUMENT, &text);
});
