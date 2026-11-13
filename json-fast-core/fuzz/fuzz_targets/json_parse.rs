#![no_main]

use json_fast_core::jsonapi::JsonApiDocument;
use json_fast_core::jsonrpc::{JsonRpcRequest, JsonRpcResponse};
use json_fast_core::{jsonl, jsonld, jwt, JsonLimits};
use libfuzzer_sys::fuzz_target;
use serde_json::Value;

// Any text through every reader of JSON, JSON Lines and the envelope formats,
// with the default limits and with small ones.
fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let text = text.as_ref();
    let small = JsonLimits {
        max_input_bytes: 512,
        max_nesting_depth: 4,
        max_tokens: 64,
        max_lines: 3,
        max_records: 2,
        max_retained_bytes: 4096,
        max_ipc_bytes: 1024,
    };
    let _ = json_fast_core::parse_value(text);
    let _ = json_fast_core::parse_value_with_limits(text, &small);
    let _ = json_fast_core::query::<Value>(text, "/address/geo/0");
    let _ = jsonl::count_lines(text);
    let _ = jsonl::parse_many::<Value>(text);
    let _ = jsonl::parse_many_with_limits::<Value>(text, &small);
    let _ = jsonl::iter::<Value>(text).count();
    let _ = jsonl::iter_with_limits::<Value>(text, &small).count();
    let _ = JsonRpcRequest::parse(text);
    let _ = JsonRpcResponse::parse(text);
    let _ = JsonApiDocument::parse(text);
    let _ = jwt::split_jwt(text);
    let _ = jsonld::peek(text);
});
