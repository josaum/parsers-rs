//! Error types for `json-fast-core`.

use std::fmt;

/// Resource governed by a parser limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonResource {
    /// UTF-8 input bytes.
    InputBytes,
    /// Maximum array/object nesting depth.
    NestingDepth,
    /// Lexical JSON tokens, including containers, keys, and scalar values.
    Tokens,
    /// Physical JSONL lines, including blank lines.
    Lines,
    /// Non-blank JSONL records.
    Records,
    /// Conservative bytes retained by parsed values.
    RetainedBytes,
    /// Bytes emitted into an Arrow IPC stream.
    IpcBytes,
}

impl JsonResource {
    /// Stable machine-readable resource name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InputBytes => "input_bytes",
            Self::NestingDepth => "nesting_depth",
            Self::Tokens => "tokens",
            Self::Lines => "lines",
            Self::Records => "records",
            Self::RetainedBytes => "retained_bytes",
            Self::IpcBytes => "ipc_bytes",
        }
    }
}

impl fmt::Display for JsonResource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Errors produced by `json-fast-core`.
///
/// Every path fails loud: nothing silently degrades to a partial or permissive
/// parse. Callers choose how to surface the message.
#[derive(Debug)]
pub enum JsonError {
    /// An input or retained-output resource budget was exceeded.
    ResourceLimit {
        /// Governed resource.
        resource: JsonResource,
        /// Configured inclusive limit.
        limit: usize,
        /// First observed value beyond the limit.
        observed: usize,
    },
    /// SIMD JSON parse failure (sonic-rs) or a malformed line in a JSONL batch.
    Parse(String),
    /// Re-serialization failure (sonic-rs / serde_json).
    Serialize(String),
    /// A structural expectation was not met (bad `jsonrpc` version, missing
    /// JSON pointer, unknown JSON type name, ...).
    Structure(String),
    /// Base64url decode failure (JWT split).
    Decode(String),
}

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JsonError::ResourceLimit {
                resource,
                limit,
                observed,
            } => write!(
                f,
                "resource_limit: {resource}: observed {observed} exceeds limit {limit}"
            ),
            JsonError::Parse(m) => write!(f, "json parse error: {m}"),
            JsonError::Serialize(m) => write!(f, "json serialize error: {m}"),
            JsonError::Structure(m) => write!(f, "json structure error: {m}"),
            JsonError::Decode(m) => write!(f, "json base64url decode error: {m}"),
        }
    }
}

impl std::error::Error for JsonError {}

impl JsonError {
    /// Stable top-level error code.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::ResourceLimit { .. } => "resource_limit",
            Self::Parse(_) => "parse",
            Self::Serialize(_) => "serialize",
            Self::Structure(_) => "structure",
            Self::Decode(_) => "decode",
        }
    }
}

impl From<sonic_rs::Error> for JsonError {
    fn from(e: sonic_rs::Error) -> Self {
        JsonError::Parse(e.to_string())
    }
}

impl From<serde_json::Error> for JsonError {
    fn from(e: serde_json::Error) -> Self {
        JsonError::Serialize(e.to_string())
    }
}

impl From<base64::DecodeError> for JsonError {
    fn from(e: base64::DecodeError) -> Self {
        JsonError::Decode(e.to_string())
    }
}

/// Convenience result alias used across the crate.
pub type Result<T> = std::result::Result<T, JsonError>;
