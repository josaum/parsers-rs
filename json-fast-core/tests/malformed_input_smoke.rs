//! Malformed JSON-family input must be rejected with an error, never a panic.
//!
//! Covers `parse_value`, `parse_value_with_limits`, `query`, the JSONL
//! readers, JSON Schema validation, JSON-RPC, JSON:API, JWT, JSON-LD, and the
//! Arrow conversion behind the `arrow` feature.

#[path = "../../tools/mutation_smoke.rs"]
mod smoke;

use json_fast_core::jsonapi::JsonApiDocument;
use json_fast_core::jsonrpc::{JsonRpcRequest, JsonRpcResponse};
use json_fast_core::{jsonl, jsonld, jwt, schema, JsonLimits};
use serde_json::Value;

#[global_allocator]
static ALLOCATOR: smoke::CappedAllocator = smoke::CappedAllocator;

const DOCUMENT: &str = r#"{"id":10452,"name":"Ana é \"q\"","tags":["a","b",null],"score":-1.5e3,
"active":true,"address":{"street":"Av. Paulista","n":1000,"geo":[-23.5,-46.6]},"email":"a@example.org",
"when":"2026-01-02T03:04:05Z","big":18446744073709551615,"nested":[[[{"k":[]}]]]}"#;

const SCHEMA: &str = r##"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object",
"required":["id","name"],"additionalProperties":{"$ref":"#/$defs/any"},
"properties":{"id":{"type":"integer","minimum":0,"multipleOf":1},
"name":{"type":"string","minLength":1,"maxLength":64,"pattern":"^[A-Z].*$"},
"tags":{"type":"array","items":{"type":["string","null"]},"uniqueItems":true,"contains":{"const":"a"},"maxItems":9},
"email":{"format":"email"},"when":{"format":"date-time"},
"address":{"$ref":"#/$defs/address"},"nested":{"$ref":"#node"}},
"patternProperties":{"^s":{"type":"number","exclusiveMaximum":0}},
"propertyNames":{"pattern":"^[a-z]+$"},"dependentRequired":{"id":["name"]},
"allOf":[{"not":{"type":"array"}}],"anyOf":[{"required":["id"]},{"required":["x"]}],
"oneOf":[{"type":"object"},{"type":"null"}],"if":{"required":["big"]},"then":{"minProperties":2},"else":false,
"unevaluatedProperties":true,
"$defs":{"any":true,"address":{"type":"object","properties":{"geo":{"prefixItems":[{"type":"number"}],"items":{"type":"number"},"unevaluatedItems":false}}},
"node":{"$anchor":"node","anyOf":[{"type":"array","items":{"$ref":"#node"}},{"type":"object","additionalProperties":{"$ref":"#node"}}]}}}"##;

const JSONL: &str = "{\"id\":1,\"name\":\"ana\",\"active\":true,\"meta\":{\"k\":1}}\n\n{\"id\":2,\"score\":1.5}\r\n[1,2]\n";
const RPC_REQUEST: &str = r#"{"jsonrpc":"2.0","method":"m.n","params":{"a":[1,2]},"id":"x"}"#;
const RPC_RESPONSE: &str =
    r#"{"jsonrpc":"2.0","error":{"code":-32601,"message":"nope","data":[1]},"id":null}"#;
const API: &str =
    r#"{"data":[{"type":"t","id":"1","attributes":{"v":3.6}}],"included":[{}],"meta":{"n":1}}"#;
const JWT: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjMiLCJuYW1lIjoiQW5hIn0.sig==";
const LD: &str =
    r#"{"@context":{"n":"https://schema.org/name"},"@type":["A"],"@graph":[{"n":"x"}]}"#;

fn seeds() -> Vec<Vec<u8>> {
    [
        DOCUMENT,
        SCHEMA,
        JSONL,
        RPC_REQUEST,
        RPC_RESPONSE,
        API,
        JWT,
        LD,
    ]
    .iter()
    .map(|seed| seed.as_bytes().to_vec())
    .collect()
}

#[cfg(feature = "arrow")]
fn to_arrow(text: &str) {
    use json_fast_core::arrow::{jsonl_schema, parse_jsonl_to_arrow_ipc, ArrowOptions};

    let options = ArrowOptions {
        batch_size: 1,
        schema: None,
    };
    let _ = jsonl_schema(text, &options);
    let _ = parse_jsonl_to_arrow_ipc(text, &options);
    let _ = parse_jsonl_to_arrow_ipc(text, &ArrowOptions::default());
}

#[cfg(not(feature = "arrow"))]
fn to_arrow(_text: &str) {}

/// Feeds `bytes` to every entry point that reads text from outside.
fn exercise(bytes: &[u8]) {
    let text = String::from_utf8_lossy(bytes);
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
    // The input as the document, then as the schema.
    let _ = schema::validate_str(text, SCHEMA);
    let _ = schema::validate_str(DOCUMENT, text);
    to_arrow(text);
}

#[test]
fn seeds_are_accepted() {
    assert!(json_fast_core::parse_value(DOCUMENT).is_ok());
    assert!(schema::validate_str(DOCUMENT, SCHEMA).is_ok());
    assert!(JsonRpcRequest::parse(RPC_REQUEST).is_ok());
    assert!(JsonRpcResponse::parse(RPC_RESPONSE).is_ok());
    assert!(JsonApiDocument::parse(API).is_ok());
    assert!(jwt::split_jwt(JWT).is_ok());
    assert!(jsonld::peek(LD).is_ok());
}

#[test]
fn mutated_json_never_panics() {
    smoke::assert_no_panics("json-fast-core", &seeds(), 20_000, exercise);
}

#[test]
fn a_million_nested_containers_are_refused_on_a_small_stack() {
    let cases: [(&[u8], &[u8], &[u8]); 5] = [
        (b"[", b"1", b"]"),
        (b"{\"a\":", b"1", b"}"),
        (b"[{\"a\":", b"null", b"}]"),
        (b"[", b"", b""),
        (b"{\"items\":", b"{}", b"}"),
    ];
    for (open, body, close) in cases {
        let document = smoke::nested(open, body, close, 1_000_000);
        smoke::run_on_small_stack(move || {
            let text = std::str::from_utf8(&document).expect("ascii");
            assert!(json_fast_core::parse_value(text).is_err());
            assert!(jsonl::parse_many::<Value>(text).is_err());
            assert!(jsonl::iter::<Value>(text).all(|record| record.is_err()));
            assert!(JsonRpcRequest::parse(text).is_err());
            assert!(JsonApiDocument::parse(text).is_err());
            assert!(jsonld::peek(text).is_err());
            assert!(schema::validate_str(text, SCHEMA).is_err());
            assert!(schema::validate_str(DOCUMENT, text).is_err());
            to_arrow(text);
        });
    }
}

/// A schema fragment to nest, and the document shape that matches it:
/// schema opener, schema closer, containers one fragment opens, document
/// opener, document closer.
type NestedSchemaCase = (
    &'static [u8],
    &'static [u8],
    usize,
    &'static [u8],
    &'static [u8],
);

/// Stack of a process's main thread on Linux and macOS (8 MiB).
const MAIN_THREAD_STACK_BYTES: usize = 8 << 20;

/// Validates the deepest schema and document `validate_str` can be handed (64
/// levels, the default nesting limit) on a thread with `stack_bytes` of stack.
fn validate_schemas_nested_to_the_parse_limit(stack_bytes: usize) {
    let depth = json_fast_core::DEFAULT_MAX_NESTING_DEPTH;
    let cases: [NestedSchemaCase; 4] = [
        (b"{\"items\":", b"}", 1, b"[", b"]"),
        (b"{\"properties\":{\"a\":", b"}}", 2, b"{\"a\":", b"}"),
        (b"{\"allOf\":[{\"anyOf\":[", b"]}]}", 4, b"[", b"]"),
        (b"{\"not\":", b"}", 1, b"[", b"]"),
    ];
    for (open, close, levels, doc_open, doc_close) in cases {
        let schema_text = smoke::nested(open, b"true", close, depth / levels - 1);
        let document = smoke::nested(doc_open, b"1", doc_close, depth - 1);
        std::thread::Builder::new()
            .stack_size(stack_bytes)
            .spawn(move || {
                let schema_text = std::str::from_utf8(&schema_text).expect("ascii");
                let document = std::str::from_utf8(&document).expect("ascii");
                schema::validate_str(document, schema_text).expect("both parse within limits");
            })
            .expect("spawn validation thread")
            .join()
            .expect("validation panicked");
    }
}

#[test]
fn schemas_nested_to_the_parse_limit_fit_a_main_thread_stack() {
    validate_schemas_nested_to_the_parse_limit(MAIN_THREAD_STACK_BYTES);
}

/// The validator recurses once per schema level with large frames.
///
/// Measured at the 64-level limit: an optimized build fits 128 KiB, but an
/// unoptimized build overflows 2 MiB (the default for a spawned thread) and
/// needs the 8 MiB of a main thread. Released artifacts are optimized, so
/// this is a debug-build hazard; shrinking the frames would remove it.
#[test]
#[ignore = "schema validation at the 64-level limit overflows a 2 MiB stack in a debug build; not fixed yet"]
fn schemas_nested_to_the_parse_limit_fit_a_small_stack() {
    validate_schemas_nested_to_the_parse_limit(smoke::SMALL_STACK_BYTES);
}

#[test]
fn reference_cycles_terminate() {
    for schema_text in [
        r##"{"$ref":"#"}"##,
        r##"{"$ref":"#/$defs/a","$defs":{"a":{"$ref":"#/$defs/b"},"b":{"$ref":"#/$defs/a"}}}"##,
        r##"{"$anchor":"x","items":{"$ref":"#x"},"properties":{"a":{"$ref":"#"}}}"##,
        r##"{"$ref":"#/missing/~0/~1/9999999999999999999999"}"##,
        r##"{"not":{"$ref":"#"}}"##,
    ] {
        for document in ["1", "[[[1]]]", r#"{"a":{"a":{"a":1}}}"#] {
            let findings = schema::validate_str(document, schema_text).expect("parses");
            assert!(findings.len() < 10_000, "{} findings", findings.len());
        }
    }
}

/// `{"allOf":[{"$ref":"#"},{"$ref":"#"}]}` follows two references per level
/// for 32 levels: 2^32 applications, each leaving a warning.
#[test]
#[ignore = "exponential: branching $ref cycles are bounded per chain (32 hops), not in total; not fixed yet"]
fn branching_reference_cycles_terminate() {
    let findings =
        schema::validate_str("1", r##"{"allOf":[{"$ref":"#"},{"$ref":"#"}]}"##).expect("parses");
    assert!(findings.len() < 10_000, "{} findings", findings.len());
}

#[test]
fn pathological_patterns_are_bounded() {
    let long = "a".repeat(50_000);
    let document = format!("{{\"s\":\"{long}\",\"{long}\":1}}");
    for pattern in [
        "(a*)*b",
        "(a|aa)+$",
        "(?:a{1000}){1000}",
        "((((((((((a{9}){9}){9}){9}){9}){9}){9}){9}){9}){9})",
        "\\\\p{L}{4000}",
        "(?=a)(?<!b)\\\\1",
        "[",
        &"(".repeat(5_000),
    ] {
        let schema_text = format!(
            "{{\"properties\":{{\"s\":{{\"pattern\":\"{pattern}\",\"format\":\"regex\"}}}},\
             \"patternProperties\":{{\"{pattern}\":true}},\"propertyNames\":{{\"pattern\":\"{pattern}\"}}}}"
        );
        if let Ok(findings) = schema::validate_str(&document, &schema_text) {
            assert!(findings.len() < 100);
        }
    }
}
