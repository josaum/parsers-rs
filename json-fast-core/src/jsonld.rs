//! JSON-LD — light `@context` / `@graph` / `@type` peek.
//!
//! JSON-LD adds special keys (`@context`, `@type`, `@graph`) to turn plain JSON
//! into linked data. This parser plane deliberately ships no JSON-LD processor
//! (no expansion, compaction, or context resolution): the native RDF path in
//! `owl-fast-core` covers Turtle, N-Triples, and N-Quads only. This module
//! only answers "is this a JSON-LD document and what is its context?" for
//! routing decisions.

use serde::Serialize;
use serde_json::Value;

use crate::{JsonError, Result};

/// JSON-LD document metadata surfaced by [`peek`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JsonLdMeta {
    /// `@context` value if present (string, object, or array).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<Value>,
    /// `@graph` value if present (linked-data graph block).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph: Option<Value>,
    /// `@type` value if present at the top level.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub type_: Option<Value>,
}

/// Peek at the JSON-LD metadata of a document.
///
/// Fails loud when the document is not a JSON object (a JSON-LD document must
/// be an object at the root).
pub fn peek(input: &str) -> Result<JsonLdMeta> {
    let value: Value = crate::parse(input)?;
    let obj = value
        .as_object()
        .ok_or_else(|| JsonError::Structure("JSON-LD document must be a JSON object".into()))?;
    Ok(JsonLdMeta {
        context: obj.get("@context").cloned(),
        graph: obj.get("@graph").cloned(),
        type_: obj.get("@type").cloned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peeks_context_and_type() {
        let meta =
            peek(r#"{"@context":"https://schema.org/","@type":"Product","name":"Atlas"}"#).unwrap();
        assert_eq!(
            meta.context,
            Some(Value::String("https://schema.org/".into()))
        );
        assert_eq!(meta.type_, Some(Value::String("Product".into())));
        assert!(meta.graph.is_none());
    }

    #[test]
    fn rejects_non_object_root() {
        assert!(peek("[1,2,3]").is_err());
    }
}
