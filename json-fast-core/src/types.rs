//! The six official JSON types (ECMA-404).
//!
//! JSON has exactly six types — String, Number, Boolean, Null, Array, Object.
//! Complex language-native types (functions, dates) do not exist in JSON; this
//! module makes the six-type model first-class so callers can classify any
//! parsed value and validate a declared type name.

use std::str::FromStr;

use crate::{JsonError, Result};

/// The six official JSON types (ECMA-404).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonType {
    /// Text in double quotes (`"nome": "Carlos Souza"`).
    String,
    /// Integer or float, optional exponent (`"idade": 28, "altura": 1.75`).
    Number,
    /// `true` / `false`.
    Boolean,
    /// Intentional absence of a value (`null`).
    Null,
    /// Ordered collection in brackets (`["leitura", "corrida"]`).
    Array,
    /// Unordered key-value object in braces (`{"rua": "Av. Paulista"}`).
    Object,
}

impl JsonType {
    /// The lowercase JSON name of this type (`"string"`, `"number"`, ...).
    pub const fn as_str(self) -> &'static str {
        match self {
            JsonType::String => "string",
            JsonType::Number => "number",
            JsonType::Boolean => "boolean",
            JsonType::Null => "null",
            JsonType::Array => "array",
            JsonType::Object => "object",
        }
    }
}

impl FromStr for JsonType {
    type Err = JsonError;

    fn from_str(s: &str) -> Result<Self> {
        type_from_str(s)
    }
}

/// Resolve a lowercase JSON type name into [`JsonType`].
///
/// Accepts the six ECMA-404 names plus the JSON-Schema short-hand `"integer"`
/// (mapped to [`JsonType::Number`]). Fails loud on anything else.
pub fn type_from_str(name: &str) -> Result<JsonType> {
    match name {
        "string" => Ok(JsonType::String),
        "number" | "integer" => Ok(JsonType::Number),
        "boolean" => Ok(JsonType::Boolean),
        "null" => Ok(JsonType::Null),
        "array" => Ok(JsonType::Array),
        "object" => Ok(JsonType::Object),
        other => Err(JsonError::Structure(format!(
            "unknown JSON type {other:?}; the six ECMA-404 types are \
             string, number, boolean, null, array, object"
        ))),
    }
}

/// Classify a parsed JSON value by its JSON type.
pub fn value_type(value: &serde_json::Value) -> JsonType {
    match value {
        serde_json::Value::Null => JsonType::Null,
        serde_json::Value::Bool(_) => JsonType::Boolean,
        serde_json::Value::Number(_) => JsonType::Number,
        serde_json::Value::String(_) => JsonType::String,
        serde_json::Value::Array(_) => JsonType::Array,
        serde_json::Value::Object(_) => JsonType::Object,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_all_six_types() {
        assert_eq!(value_type(&json!(null)), JsonType::Null);
        assert_eq!(value_type(&json!(true)), JsonType::Boolean);
        assert_eq!(value_type(&json!(28)), JsonType::Number);
        assert_eq!(value_type(&json!("x")), JsonType::String);
        assert_eq!(value_type(&json!([1, 2])), JsonType::Array);
        assert_eq!(value_type(&json!({"a": 1})), JsonType::Object);
    }

    #[test]
    fn parses_official_names() {
        for name in ["string", "number", "boolean", "null", "array", "object"] {
            assert_eq!(type_from_str(name).unwrap().as_str(), name);
        }
        assert_eq!(type_from_str("integer").unwrap(), JsonType::Number);
        assert!(type_from_str("date").is_err());
    }
}
