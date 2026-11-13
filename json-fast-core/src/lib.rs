//! `json-fast-core` — SIMD-accelerated JSON and JSON-family parser.
//!
//! Covers the six official JSON types (String, Number, Boolean, Null, Array,
//! Object — ECMA-404) and the main derived formats used across the stack:
//!
//! | Format          | Module                  | Notes |
//! |-----------------|-------------------------|-------|
//! | JSON (6 types)  | [`parse`], [`parse_value`], [`stringify()`], [`types`] | SIMD via sonic-rs |
//! | JSONL / NDJSON  | [`jsonl`]               | line-delimited streaming |
//! | JSON Schema     | [`schema`]              | draft-07 through 2020-12 constraint keywords, local `$ref`, allocation-free walk |
//! | JSON-RPC 2.0    | [`jsonrpc`]             | request / response envelopes |
//! | JSON:API        | [`jsonapi`]             | resource envelope (`data` XOR `errors`) |
//! | JWK / JWT       | [`jwt`]                 | header/payload split + base64url decode (no crypto) |
//! | JSON-LD         | [`jsonld`]              | `@context` / `@graph` / `@type` peek (deep expansion lives in `owl-fast-core`) |
//!
//! BSON, JSON5 and HJSON are deliberately out of scope (binary / human-first
//! variants); use the `bson` / `json5` / `hjson` crates when those are needed.
//! Everything here fails loud — nothing silently degrades to an unsupported
//! parse.
//!
//! # Quick start
//! ```
//! use json_fast_core::{parse_value, jsonl, JsonType};
//!
//! let v = parse_value(r#"{"nome":"Ana","premium":false}"#).unwrap();
//! assert_eq!(json_fast_core::value_type(&v), JsonType::Object);
//!
//! let lines = jsonl::count_lines("{\"a\":1}\n{\"a\":2}\n");
//! assert_eq!(lines, 2);
//! ```

#![deny(missing_docs)]
#![warn(rust_2018_idioms)]

pub mod error;
pub mod jsonapi;
pub mod jsonl;
pub mod jsonld;
pub mod jsonrpc;
pub mod jwt;
pub mod schema;
pub mod types;

#[cfg(feature = "arrow")]
pub mod arrow;

pub use error::{JsonError, JsonResource, Result};
pub use types::{type_from_str, value_type, JsonType};

/// Crate version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default maximum UTF-8 input size: 8 MiB.
pub const DEFAULT_MAX_INPUT_BYTES: usize = 8 * 1024 * 1024;
/// Default maximum JSON array/object nesting depth.
pub const DEFAULT_MAX_NESTING_DEPTH: usize = 64;
/// Deepest nesting any caller-supplied [`JsonLimits`] can allow.
///
/// The parsing backend recurses once per array/object level and refuses
/// input more than 128 levels deep with an error of its own that quotes the
/// input. A `max_nesting_depth` above this ceiling is treated as this
/// ceiling, so deeper input is always this crate's `resource_limit` error
/// and the backend's limit is never reached. 128 levels is the most the
/// backend parses; an unoptimized build needs a large stack to get there
/// (63 nested objects already take 2 to 4 MiB).
pub const MAX_SUPPORTED_NESTING_DEPTH: usize = 128;
/// Default maximum lexical JSON tokens.
pub const DEFAULT_MAX_TOKENS: usize = 1_000_000;
/// Default maximum physical lines in one JSONL input.
pub const DEFAULT_MAX_LINES: usize = 100_000;
/// Default maximum non-blank records in one JSONL input.
pub const DEFAULT_MAX_RECORDS: usize = 100_000;
/// Default conservative retained-value budget: 64 MiB.
pub const DEFAULT_MAX_RETAINED_BYTES: usize = 64 * 1024 * 1024;
/// Default in-memory Arrow IPC output budget: 64 MiB.
pub const DEFAULT_MAX_IPC_BYTES: usize = 64 * 1024 * 1024;

/// Fail-closed resource limits for JSON and JSONL parsing.
#[derive(Debug, Clone, Copy)]
pub struct JsonLimits {
    /// Maximum UTF-8 bytes in one input.
    pub max_input_bytes: usize,
    /// Maximum object/array nesting depth; values above
    /// [`MAX_SUPPORTED_NESTING_DEPTH`] are treated as that ceiling.
    pub max_nesting_depth: usize,
    /// Maximum lexical tokens across one JSON document or JSONL batch.
    pub max_tokens: usize,
    /// Maximum physical lines in one JSONL batch.
    pub max_lines: usize,
    /// Maximum non-blank records in one JSONL batch.
    pub max_records: usize,
    /// Maximum conservative retained bytes for parsed values.
    pub max_retained_bytes: usize,
    /// Maximum bytes in one returned Arrow IPC stream.
    pub max_ipc_bytes: usize,
}

impl Default for JsonLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
            max_nesting_depth: DEFAULT_MAX_NESTING_DEPTH,
            max_tokens: DEFAULT_MAX_TOKENS,
            max_lines: DEFAULT_MAX_LINES,
            max_records: DEFAULT_MAX_RECORDS,
            max_retained_bytes: DEFAULT_MAX_RETAINED_BYTES,
            max_ipc_bytes: DEFAULT_MAX_IPC_BYTES,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct JsonUsage {
    pub(crate) tokens: usize,
    pub(crate) retained_bytes: usize,
}

pub(crate) fn check_limit(resource: JsonResource, observed: usize, limit: usize) -> Result<()> {
    if observed > limit {
        return Err(JsonError::ResourceLimit {
            resource,
            limit,
            observed,
        });
    }
    Ok(())
}

pub(crate) fn scan_json(input: &str, limits: &JsonLimits) -> Result<JsonUsage> {
    check_limit(
        JsonResource::InputBytes,
        input.len(),
        limits.max_input_bytes,
    )?;

    let bytes = input.as_bytes();
    let mut index = 0usize;
    let mut depth = 0usize;
    let mut tokens = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b' ' | b'\t' | b'\r' | b'\n' | b',' | b':' | b']' | b'}' => {
                if matches!(bytes[index], b']' | b'}') {
                    depth = depth.saturating_sub(1);
                }
                index += 1;
            }
            b'[' | b'{' => {
                depth = depth.saturating_add(1);
                check_limit(
                    JsonResource::NestingDepth,
                    depth,
                    limits.max_nesting_depth.min(MAX_SUPPORTED_NESTING_DEPTH),
                )?;
                tokens = tokens.saturating_add(1);
                check_limit(JsonResource::Tokens, tokens, limits.max_tokens)?;
                index += 1;
            }
            b'"' => {
                tokens = tokens.saturating_add(1);
                check_limit(JsonResource::Tokens, tokens, limits.max_tokens)?;
                index += 1;
                let mut escaped = false;
                while index < bytes.len() {
                    let byte = bytes[index];
                    index += 1;
                    if escaped {
                        escaped = false;
                    } else if byte == b'\\' {
                        escaped = true;
                    } else if byte == b'"' {
                        break;
                    }
                }
            }
            _ => {
                tokens = tokens.saturating_add(1);
                check_limit(JsonResource::Tokens, tokens, limits.max_tokens)?;
                index += 1;
                while index < bytes.len()
                    && !matches!(
                        bytes[index],
                        b' ' | b'\t'
                            | b'\r'
                            | b'\n'
                            | b','
                            | b':'
                            | b'['
                            | b']'
                            | b'{'
                            | b'}'
                            | b'"'
                    )
                {
                    index += 1;
                }
            }
        }
    }

    let retained_bytes = input.len().saturating_add(tokens.saturating_mul(64));
    check_limit(
        JsonResource::RetainedBytes,
        retained_bytes,
        limits.max_retained_bytes,
    )?;
    Ok(JsonUsage {
        tokens,
        retained_bytes,
    })
}

pub(crate) fn parse_preflighted<T: serde::de::DeserializeOwned>(input: &str) -> Result<T> {
    sonic_rs::from_str(input).map_err(JsonError::from)
}

/// Parse a UTF-8 JSON document into a typed value via the SIMD fast path.
///
/// `T` must implement `serde::DeserializeOwned`. For an untyped value use
/// [`parse_value`], which returns a `serde_json::Value` modeling the six JSON
/// types.
pub fn parse<T: serde::de::DeserializeOwned>(input: &str) -> Result<T> {
    parse_with_limits(input, &JsonLimits::default())
}

/// Parse a UTF-8 JSON document after enforcing explicit resource limits.
pub fn parse_with_limits<T: serde::de::DeserializeOwned>(
    input: &str,
    limits: &JsonLimits,
) -> Result<T> {
    scan_json(input, limits)?;
    parse_preflighted(input)
}

/// Parse a JSON document into the generic value model (six JSON types).
pub fn parse_value(input: &str) -> Result<serde_json::Value> {
    parse(input)
}

/// Parse a generic JSON value after enforcing explicit resource limits.
pub fn parse_value_with_limits(input: &str, limits: &JsonLimits) -> Result<serde_json::Value> {
    parse_with_limits(input, limits)
}

/// Serialize a value to a compact JSON string via the SIMD fast path.
pub fn stringify<T: serde::Serialize>(value: &T) -> Result<String> {
    sonic_rs::to_string(value).map_err(JsonError::from)
}

/// Query a JSON document with a JSON pointer (RFC 6901) and deserialize the
/// targeted node. Fails loud when the path is absent.
pub fn query<T: serde::de::DeserializeOwned>(input: &str, json_pointer: &str) -> Result<T> {
    query_with_limits(input, json_pointer, &JsonLimits::default())
}

/// Query a JSON pointer after enforcing explicit parse resource limits.
pub fn query_with_limits<T: serde::de::DeserializeOwned>(
    input: &str,
    json_pointer: &str,
    limits: &JsonLimits,
) -> Result<T> {
    let value: serde_json::Value = parse_with_limits(input, limits)?;
    let node = value
        .pointer(json_pointer)
        .ok_or_else(|| JsonError::Structure(format!("missing JSON pointer {json_pointer:?}")))?;
    serde_json::from_value(node.clone()).map_err(JsonError::from)
}

#[cfg(feature = "mmap")]
use std::path::Path;

/// Parse a UTF-8 JSON file into a generic value by memory-mapping it (feature
/// `mmap`). The bytes are read straight out of the page cache; the parsed
/// value owns its own storage, so the mapping is dropped on return.
///
/// Resource limits apply before the parse, exactly as for [`parse_with_limits`].
#[cfg(feature = "mmap")]
pub fn parse_file(path: impl AsRef<Path>) -> Result<serde_json::Value> {
    parse_file_with_limits(path, &JsonLimits::default())
}

/// [`parse_file`] after enforcing explicit resource limits.
#[cfg(feature = "mmap")]
pub fn parse_file_with_limits(
    path: impl AsRef<Path>,
    limits: &JsonLimits,
) -> Result<serde_json::Value> {
    let path = path.as_ref();
    let file = std::fs::File::open(path)
        .map_err(|e| JsonError::Structure(format!("cannot open {}: {e}", path.display())))?;
    // SAFETY: the mapping is read-only and lives until the parse completes;
    // the mapping itself holds the file handle. Concurrent truncation of the
    // file is the caller's hazard, as with any mmap consumer.
    let map = unsafe { memmap2::Mmap::map(&file) }
        .map_err(|e| JsonError::Structure(format!("cannot map {}: {e}", path.display())))?;
    let input = std::str::from_utf8(&map)
        .map_err(|e| JsonError::Structure(format!("{} is not UTF-8: {e}", path.display())))?;
    scan_json(input, limits)?;
    parse_preflighted(input)
}

/// Validate a JSON file against a schema by memory-mapping the document
/// (feature `mmap`). Equivalent to [`schema::validate`] with the file's
/// content parsed under default resource limits.
#[cfg(feature = "mmap")]
pub fn validate_file(path: impl AsRef<Path>, schema: &str) -> Result<Vec<schema::Finding>> {
    let doc = parse_file(path)?;
    let sch: serde_json::Value = parse(schema)?;
    Ok(schema::validate(&doc, &sch))
}

/// Prelude for convenient imports.
pub mod prelude {
    pub use crate::error::{JsonError, JsonResource, Result};
    pub use crate::jsonl::{count_lines, parse_many, parse_many_with_limits};
    pub use crate::types::{value_type, JsonType};
    pub use crate::{
        parse, parse_value, parse_value_with_limits, parse_with_limits, query, query_with_limits,
        stringify, JsonLimits,
    };

    #[cfg(feature = "arrow")]
    pub use crate::arrow::{self, ArrowOptions};

    #[cfg(feature = "mmap")]
    pub use crate::{parse_file, parse_file_with_limits, validate_file};
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Deserialize, Serialize, PartialEq)]
    struct Point {
        x: f64,
        y: f64,
    }

    #[test]
    fn parses_typed_document() {
        let p: Point = parse(r#"{"x":1.5,"y":-2.0}"#).unwrap();
        assert_eq!(p, Point { x: 1.5, y: -2.0 });
    }

    #[test]
    fn parses_value_roundtrip() {
        let raw = r#"{"a":[1,true,null,"s",{"b":2}]}"#;
        let v = parse_value(raw).unwrap();
        assert_eq!(v["a"][1], serde_json::Value::Bool(true));
        assert_eq!(stringify(&v).unwrap().len(), raw.len());
    }

    #[test]
    fn query_pointer() {
        let n: u64 = query(r#"{"id":10452}"#, "/id").unwrap();
        assert_eq!(n, 10452);
        assert!(query::<serde_json::Value>(r#"{"a":1}"#, "/missing").is_err());
    }

    #[test]
    fn rejects_input_before_parsing_when_byte_budget_is_exceeded() {
        let limits = JsonLimits {
            max_input_bytes: 4,
            ..Default::default()
        };
        let error = parse_value_with_limits(r#"{"a":1}"#, &limits)
            .expect_err("oversized JSON must fail before parsing");
        assert!(matches!(
            error,
            JsonError::ResourceLimit {
                resource: JsonResource::InputBytes,
                limit: 4,
                ..
            }
        ));
    }

    #[test]
    fn rejects_nesting_and_token_budgets_before_materialization() {
        let nesting_limits = JsonLimits {
            max_nesting_depth: 2,
            ..Default::default()
        };
        let nesting = parse_value_with_limits("[[[0]]]", &nesting_limits)
            .expect_err("deep JSON must be rejected");
        assert!(matches!(
            nesting,
            JsonError::ResourceLimit {
                resource: JsonResource::NestingDepth,
                ..
            }
        ));

        let token_limits = JsonLimits {
            max_tokens: 2,
            ..Default::default()
        };
        let tokens = parse_value_with_limits(r#"{"a":1}"#, &token_limits)
            .expect_err("the token budget must be enforced");
        assert!(matches!(
            tokens,
            JsonError::ResourceLimit {
                resource: JsonResource::Tokens,
                ..
            }
        ));
    }

    #[test]
    fn rejects_conservative_retained_output_budget_before_parse() {
        let limits = JsonLimits {
            max_retained_bytes: 1,
            ..Default::default()
        };
        let error = parse_value_with_limits("{}", &limits)
            .expect_err("retained output budget must fail closed");
        assert!(matches!(
            error,
            JsonError::ResourceLimit {
                resource: JsonResource::RetainedBytes,
                limit: 1,
                ..
            }
        ));
    }

    #[cfg(feature = "mmap")]
    #[test]
    fn parses_and_validates_a_mapped_file() {
        let path =
            std::env::temp_dir().join(format!("json-fast-core-mmap-{}.json", std::process::id()));
        std::fs::write(&path, br#"{"id": 1, "name": "x"}"#).unwrap();

        let value = parse_file(&path).unwrap();
        assert_eq!(value["id"], 1);
        assert!(parse_file_with_limits(
            &path,
            &JsonLimits {
                max_input_bytes: 4,
                ..Default::default()
            }
        )
        .is_err());

        let findings = validate_file(&path, r#"{"type": "object", "required": ["id"]}"#).unwrap();
        assert!(findings.is_empty());
        let findings =
            validate_file(&path, r#"{"type": "object", "required": ["missing"]}"#).unwrap();
        assert!(findings
            .iter()
            .any(|f| f.severity == crate::schema::Severity::Error));

        let _ = std::fs::remove_file(&path);
    }
}
