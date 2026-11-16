//! json-fast-node: N-API bindings mirroring `json-fast-py` (full parity).
//!
//! SIMD-accelerated JSON / JSONL parsing and JSON-family helpers: the six JSON
//! types, JSONL/NDJSON streaming, JSON Schema validation, JSON-RPC 2.0,
//! JSON:API, JWT split, and JSON-LD context peek.

use std::collections::HashMap;

use json_fast_core::jsonl;
use napi::bindgen_prelude::*;
use napi_derive::napi;
use serde_json::{json, Value};

fn err(e: impl ToString) -> Error {
    Error::from_reason(e.to_string())
}

/// Crate / binding version string.
#[napi(catch_unwind)]
pub fn version() -> &'static str {
    json_fast_core::VERSION
}

/// Parse a JSON document into a JS value (SIMD fast path).
#[napi(catch_unwind)]
pub fn parse_json(s: String) -> Result<Value> {
    json_fast_core::parse::<Value>(&s).map_err(err)
}

/// Parse a JSONL document into an array of JS values (blank lines skipped).
#[napi(catch_unwind)]
pub fn parse_jsonl(s: String) -> Result<Vec<Value>> {
    jsonl::parse_many::<Value>(&s).map_err(err)
}

/// Count non-blank lines in a JSONL document (fast streaming pass).
#[napi(catch_unwind)]
pub fn count_lines(s: String) -> Result<u32> {
    Ok(jsonl::count_lines(&s) as u32)
}

/// Return the six-type classification of a JSON document's root value.
#[napi(catch_unwind)]
pub fn json_type(s: String) -> Result<String> {
    let value: Value = json_fast_core::parse(&s).map_err(err)?;
    Ok(json_fast_core::value_type(&value).as_str().to_string())
}

/// Validate a JSON document against a JSON Schema; returns an array of findings.
///
/// `resources` maps a URI to the JSON text of another schema document that a
/// `$ref` may name. Nothing is fetched: a reference that resolves neither in
/// the schema nor in `resources` is an error finding. Documents are taken in
/// URI order, so when two claim one URI (through `$id`) the first by URI
/// order wins. `index.js` refuses any `resources` that is not a plain object.
#[napi(catch_unwind)]
pub fn validate(
    s: String,
    schema: String,
    resources: Option<HashMap<String, String>>,
) -> Result<Vec<Value>> {
    let doc: Value = json_fast_core::parse(&s).map_err(err)?;
    let sch: Value = json_fast_core::parse(&schema).map_err(err)?;
    let mut supplied = resources
        .unwrap_or_default()
        .into_iter()
        .map(|(uri, text)| match json_fast_core::parse(&text) {
            Ok(value) => Ok((uri, value)),
            Err(error) => Err(err(format!("schema resource {uri:?}: {error}"))),
        })
        .collect::<Result<Vec<(String, Value)>>>()?;
    // An object's key order is not kept by the map; sorting makes the
    // choice between two documents claiming one URI deterministic.
    supplied.sort_by(|a, b| a.0.cmp(&b.0));
    let documents: Vec<(&str, &Value)> = supplied
        .iter()
        .map(|(uri, value)| (uri.as_str(), value))
        .collect();
    Ok(
        json_fast_core::schema::validate_with_resources(&doc, &sch, &documents)
            .into_iter()
            .map(|f| {
                json!({
                    "severity": f.severity.as_str(),
                    "path": f.path,
                    "message": f.message,
                })
            })
            .collect(),
    )
}

/// Parse and validate a JSON-RPC 2.0 request envelope.
#[napi(catch_unwind)]
pub fn json_rpc_request(s: String) -> Result<Value> {
    let request = json_fast_core::jsonrpc::JsonRpcRequest::parse(&s).map_err(err)?;
    serde_json::to_value(&request).map_err(err)
}

/// Parse and validate a JSON-RPC 2.0 response envelope.
#[napi(catch_unwind)]
pub fn json_rpc_response(s: String) -> Result<Value> {
    let response = json_fast_core::jsonrpc::JsonRpcResponse::parse(&s).map_err(err)?;
    serde_json::to_value(&response).map_err(err)
}

/// Parse and validate a JSON:API document envelope.
#[napi(catch_unwind)]
pub fn json_api_document(s: String) -> Result<Value> {
    let document = json_fast_core::jsonapi::JsonApiDocument::parse(&s).map_err(err)?;
    serde_json::to_value(&document).map_err(err)
}

/// Split a compact JWT into `{header, payload, signature}`.
#[napi(catch_unwind)]
pub fn jwt_parts(s: String) -> Result<Value> {
    let parts = json_fast_core::jwt::split_jwt(&s).map_err(err)?;
    Ok(json!({
        "header": parts.header,
        "payload": parts.payload,
        "signature": parts.signature,
    }))
}

/// Peek at a JSON-LD document's `@context` / `@graph` / `@type`.
#[napi(catch_unwind)]
pub fn json_ld_meta(s: String) -> Result<Value> {
    let meta = json_fast_core::jsonld::peek(&s).map_err(err)?;
    serde_json::to_value(&meta).map_err(err)
}

// The `#[napi]` attribute leaves each function above callable as plain Rust
// and generates the N-API glue beside it. These tests call the plain
// functions, so they need no Node runtime; `smoke.test.mjs` covers the glue
// and `index.js` under Node.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_the_core_version() {
        assert_eq!(version(), json_fast_core::VERSION);
        assert!(!version().is_empty());
    }

    #[test]
    fn parses_json_and_jsonl() {
        let value = parse_json(r#"{"brand":"Nylabone","active":true}"#.to_owned()).unwrap();
        assert_eq!(value, json!({"brand": "Nylabone", "active": true}));

        let jsonl = "{\"a\":1}\n\n{\"a\":2}\n".to_owned();
        let rows = parse_jsonl(jsonl.clone()).unwrap();
        assert_eq!(rows, vec![json!({"a": 1}), json!({"a": 2})]);
        assert_eq!(count_lines(jsonl).unwrap(), 2);
    }

    #[test]
    fn classifies_the_root_type() {
        for (text, kind) in [("{}", "object"), ("[1]", "array"), ("null", "null")] {
            assert_eq!(json_type(text.to_owned()).unwrap(), kind);
        }
    }

    #[test]
    fn errors_carry_the_core_message() {
        let error = parse_json("{".to_owned()).unwrap_err();
        assert!(
            error.reason.starts_with("json parse error"),
            "{}",
            error.reason
        );
        let error = parse_jsonl("{\"a\":1}\nnot json\n".to_owned()).unwrap_err();
        assert!(error.reason.contains("line 2"), "{}", error.reason);
    }

    #[test]
    fn validate_takes_supplied_schema_documents_and_fails_closed() {
        let schema = r#"{"$ref": "https://example.com/positive.json"}"#.to_owned();
        let resources = HashMap::from([(
            "https://example.com/positive.json".to_owned(),
            r#"{"type": "integer", "minimum": 1}"#.to_owned(),
        )]);
        let errors = |findings: Vec<Value>| -> Vec<Value> {
            findings
                .into_iter()
                .filter(|finding| finding["severity"] == "error")
                .collect()
        };
        let run = |document: &str, resources: Option<HashMap<String, String>>| {
            validate(document.to_owned(), schema.clone(), resources).unwrap()
        };
        assert_eq!(
            errors(run("3", Some(resources.clone()))),
            Vec::<Value>::new()
        );
        assert!(!errors(run("0", Some(resources))).is_empty());
        let unresolved = errors(run("3", None));
        assert!(
            unresolved[0]["message"]
                .as_str()
                .is_some_and(|message| message.contains("does not resolve")),
            "{unresolved:?}"
        );
        let broken = HashMap::from([(
            "https://example.com/positive.json".to_owned(),
            "{".to_owned(),
        )]);
        assert!(validate("3".to_owned(), schema.clone(), Some(broken)).is_err());
    }

    #[test]
    fn validate_reports_findings_as_plain_objects() {
        let schema = r#"{"type":"object","properties":{"value":{"type":"number"}}}"#;
        let ok = validate(r#"{"value":3.1}"#.to_owned(), schema.to_owned(), None).unwrap();
        assert!(
            ok.iter().all(|finding| finding["severity"] != "error"),
            "{ok:?}"
        );
        let bad = validate(r#"{"value":"x"}"#.to_owned(), schema.to_owned(), None).unwrap();
        let finding = bad
            .iter()
            .find(|finding| finding["severity"] == "error")
            .expect("a type error");
        assert!(finding["path"].is_string() && finding["message"].is_string());
    }

    #[test]
    fn envelope_helpers_round_trip() {
        let request =
            json_rpc_request(r#"{"jsonrpc":"2.0","method":"ping","id":1}"#.to_owned()).unwrap();
        assert_eq!(request["method"], "ping");
        assert!(json_rpc_request(r#"{"jsonrpc":"1.0","method":"ping"}"#.to_owned()).is_err());

        let response = json_rpc_response(r#"{"jsonrpc":"2.0","result":1,"id":1}"#.to_owned());
        assert_eq!(response.unwrap()["result"], 1);

        let document = json_api_document(r#"{"data":{"type":"t","id":"1"}}"#.to_owned());
        assert_eq!(document.unwrap()["data"]["id"], "1");

        let meta = json_ld_meta(r#"{"@context":"https://schema.org","@type":"Thing"}"#.to_owned());
        assert!(meta.is_ok(), "{meta:?}");
    }

    #[test]
    fn jwt_parts_splits_without_verifying() {
        // header {"alg":"none"} . payload {"sub":"a"} . empty signature
        let parts = jwt_parts("eyJhbGciOiJub25lIn0.eyJzdWIiOiJhIn0.".to_owned()).unwrap();
        assert_eq!(parts["header"]["alg"], "none");
        assert_eq!(parts["payload"]["sub"], "a");
        assert!(jwt_parts("not-a-jwt".to_owned()).is_err());
    }
}
