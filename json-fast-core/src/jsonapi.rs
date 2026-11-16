//! JSON:API resource envelopes.
//!
//! JSON:API is a rigorous REST specification. A document carries a `data`
//! resource or a list of `errors`, plus optional `included`, `meta`, `links`
//! and `jsonapi`. This module checks the core envelope members under
//! JSON:API 1.1, including the mutual exclusion of `data` and `errors`.
//! Resource contents, links, extensions and profiles are not validated.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{JsonError, Result};

/// A JSON:API top-level document.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct JsonApiDocument {
    /// The primary resource or resource collection; explicit null is `Some(Null)`.
    #[serde(
        default,
        deserialize_with = "crate::deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub data: Option<Value>,
    /// Error objects, present on failures.
    #[serde(
        default,
        deserialize_with = "crate::deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub errors: Option<Vec<Value>>,
    /// Related resources.
    #[serde(
        default,
        deserialize_with = "crate::deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub included: Option<Vec<Value>>,
    /// Non-standard metadata.
    #[serde(
        default,
        deserialize_with = "crate::deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub meta: Option<Value>,
}

impl JsonApiDocument {
    /// Parse and validate a JSON:API document envelope.
    ///
    /// Fails loud when both `data` and `errors` are present, or when the
    /// document has none of `data`, `errors`, `meta`.
    pub fn parse(input: &str) -> Result<Self> {
        let document: JsonApiDocument = crate::parse_object(input)?;
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
        if document.data.as_ref().is_some_and(|value| {
            !(value.is_null()
                || value.is_object()
                || value
                    .as_array()
                    .is_some_and(|items| items.iter().all(Value::is_object)))
        }) {
            return Err(JsonError::Structure(
                "JSON:API data must be null, an object, or an array of objects".into(),
            ));
        }
        if document.meta.as_ref().is_some_and(|meta| !meta.is_object()) {
            return Err(JsonError::Structure(
                "JSON:API meta must be an object".into(),
            ));
        }
        if document.included.is_some() && document.data.is_none() {
            return Err(JsonError::Structure(
                "JSON:API included requires data".into(),
            ));
        }
        for (name, items) in [
            ("errors", &document.errors),
            ("included", &document.included),
        ] {
            if items
                .as_ref()
                .is_some_and(|items| items.iter().any(|item| !item.is_object()))
            {
                return Err(JsonError::Structure(format!(
                    "JSON:API {name} must contain objects"
                )));
            }
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
