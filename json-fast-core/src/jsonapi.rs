//! JSON:API resource envelopes.
//!
//! JSON:API is a rigorous REST specification. A document carries a `data`
//! resource or a list of `errors`, plus optional `included`, `meta`, `links`
//! and `jsonapi`. This module parses the envelope and enforces the `data` XOR
//! `errors` rule — it does not validate every member.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{JsonError, Result};

/// A JSON:API top-level document.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct JsonApiDocument {
    /// The primary resource or resource collection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    /// Error objects, present on failures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errors: Option<Vec<Value>>,
    /// Related resources.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub included: Option<Vec<Value>>,
    /// Non-standard metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

impl JsonApiDocument {
    /// Parse and validate a JSON:API document envelope.
    ///
    /// Fails loud when both `data` and `errors` are present, or when the
    /// document has none of `data`, `errors`, `meta`.
    pub fn parse(input: &str) -> Result<Self> {
        let document: JsonApiDocument = crate::parse(input)?;
        if document.data.is_some() && document.errors.is_some() {
            return Err(JsonError::Structure(
                "JSON:API document has both data and errors".into(),
            ));
        }
        if document.data.is_none() && document.errors.is_none() && document.meta.is_none() {
            return Err(JsonError::Structure(
                "JSON:API document must have data, errors, or meta".into(),
            ));
        }
        Ok(document)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_data_document() {
        let doc = JsonApiDocument::parse(
            r#"{"data":{"type":"metrics","id":"roas","attributes":{"value":3.6}}}"#,
        )
        .unwrap();
        assert_eq!(doc.data.as_ref().unwrap()["type"], "metrics");
        assert!(doc.errors.is_none());
    }

    #[test]
    fn parses_errors_document() {
        let doc = JsonApiDocument::parse(
            r#"{"errors":[{"status":"404","title":"Not found"}],"meta":{"count":1}}"#,
        )
        .unwrap();
        assert!(doc.data.is_none());
        assert_eq!(doc.errors.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn rejects_data_and_errors_together() {
        let both = r#"{"data":{},"errors":[]}"#;
        assert!(JsonApiDocument::parse(both).is_err());
    }

    #[test]
    fn rejects_empty_document() {
        assert!(JsonApiDocument::parse(r#"{"links":{}}"#).is_err());
    }
}
