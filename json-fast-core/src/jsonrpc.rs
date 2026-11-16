//! JSON-RPC 2.0 request / response envelopes.
//!
//! A JSON-RPC request is `{"jsonrpc":"2.0","method":"...","params":...,"id":...}`;
//! a response is `{"jsonrpc":"2.0","result":...,"id":...}` or
//! `{"jsonrpc":"2.0","error":{"code":...,"message":"...","data":...},"id":...}`.
//! Parsing here is structural: it validates the envelope, it never dispatches.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{JsonError, Result};

/// A JSON-RPC 2.0 request object.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct JsonRpcRequest {
    /// Must be `"2.0"`.
    pub jsonrpc: String,
    /// Method name.
    pub method: String,
    /// Positional or named parameters.
    #[serde(
        default,
        deserialize_with = "crate::deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub params: Option<Value>,
    /// Request identifier (echoed back on the response). Explicit null is
    /// `Some(Null)`; only an absent identifier denotes a notification.
    #[serde(
        default,
        deserialize_with = "crate::deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub id: Option<Value>,
}

impl JsonRpcRequest {
    /// Parse and validate a JSON-RPC 2.0 request envelope.
    ///
    /// Requires version `"2.0"`, a string method, structured parameters if
    /// present, and a string, number or null identifier if present.
    pub fn parse(input: &str) -> Result<Self> {
        let request: JsonRpcRequest = crate::parse_object(input)?;
        request.validate()?;
        Ok(request)
    }

    fn validate(&self) -> Result<()> {
        if self.jsonrpc != "2.0" {
            return Err(JsonError::Structure(format!(
                "jsonrpc must be \"2.0\", got {:?}",
                self.jsonrpc
            )));
        }
        if self
            .params
            .as_ref()
            .is_some_and(|params| !params.is_array() && !params.is_object())
        {
            return Err(JsonError::Structure(
                "jsonrpc params must be an array or object".into(),
            ));
        }
        validate_id(self.id.as_ref())?;
        Ok(())
    }
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct JsonRpcError {
    /// Error code (integer).
    pub code: i64,
    /// Short human-readable message.
    pub message: String,
    /// Optional additional data.
    #[serde(
        default,
        deserialize_with = "crate::deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub data: Option<Value>,
}

/// A JSON-RPC 2.0 response object (result XOR error).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct JsonRpcResponse {
    /// Must be `"2.0"`.
    pub jsonrpc: String,
    /// Present on success, including an explicit null result (`Some(Null)`).
    #[serde(
        default,
        deserialize_with = "crate::deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub result: Option<Value>,
    /// Present on failure.
    #[serde(
        default,
        deserialize_with = "deserialize_error",
        skip_serializing_if = "Option::is_none"
    )]
    pub error: Option<JsonRpcError>,
    /// Echoes the request identifier; an unknown identifier is `Some(Null)`.
    #[serde(
        default,
        deserialize_with = "crate::deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub id: Option<Value>,
}

impl JsonRpcResponse {
    /// Parse and validate a JSON-RPC 2.0 response envelope.
    ///
    /// Fails loud when `jsonrpc` is not `"2.0"`, or when both `result` and
    /// `error` are present (a response must be exactly one of them), or the
    /// required identifier is missing or has an invalid type.
    pub fn parse(input: &str) -> Result<Self> {
        let response: JsonRpcResponse = crate::parse_object(input)?;
        response.validate()?;
        Ok(response)
    }

    fn validate(&self) -> Result<()> {
        if self.jsonrpc != "2.0" {
            return Err(JsonError::Structure(format!(
                "jsonrpc must be \"2.0\", got {:?}",
                self.jsonrpc
            )));
        }
        if self.result.is_some() && self.error.is_some() {
            return Err(JsonError::Structure(
                "jsonrpc response has both result and error".into(),
            ));
        }
        if self.result.is_none() && self.error.is_none() {
            return Err(JsonError::Structure(
                "jsonrpc response has neither result nor error".into(),
            ));
        }
        if self.id.is_none() {
            return Err(JsonError::Structure("jsonrpc response requires id".into()));
        }
        validate_id(self.id.as_ref())?;
        Ok(())
    }
}

fn validate_id(id: Option<&Value>) -> Result<()> {
    if id.is_some_and(|id| !(id.is_null() || id.is_string() || id.is_number())) {
        return Err(JsonError::Structure(
            "jsonrpc id must be a string, number, or null".into(),
        ));
    }
    Ok(())
}

fn deserialize_error<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<JsonRpcError>, D::Error> {
    struct ErrorObject;
    impl<'de> serde::de::Visitor<'de> for ErrorObject {
        type Value = Option<JsonRpcError>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a JSON-RPC error object")
        }

        fn visit_map<M: serde::de::MapAccess<'de>>(
            self,
            map: M,
        ) -> std::result::Result<Self::Value, M::Error> {
            JsonRpcError::deserialize(serde::de::value::MapAccessDeserializer::new(map)).map(Some)
        }
    }
    deserializer.deserialize_map(ErrorObject)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_request() {
        let req = JsonRpcRequest::parse(
            r#"{"jsonrpc":"2.0","method":"catalog.lookup","params":["item-1"],"id":1}"#,
        )
        .unwrap();
        assert_eq!(req.method, "catalog.lookup");
        assert_eq!(req.id, Some(Value::from(1)));
    }

    #[test]
    fn rejects_wrong_version_and_accepts_string_methods() {
        assert!(JsonRpcRequest::parse(r#"{"jsonrpc":"1.0","method":"m"}"#).is_err());
        assert!(JsonRpcRequest::parse(r#"{"jsonrpc":"2.0","method":""}"#).is_ok());
    }

    #[test]
    fn parses_result_and_error_responses() {
        let ok = JsonRpcResponse::parse(r#"{"jsonrpc":"2.0","result":3.6,"id":1}"#).unwrap();
        assert_eq!(ok.result, Some(json!(3.6)));
        let err = JsonRpcResponse::parse(
            r#"{"jsonrpc":"2.0","error":{"code":-32601,"message":"Method not found"},"id":null}"#,
        )
        .unwrap();
        assert_eq!(err.error.unwrap().code, -32601);
    }

    #[test]
    fn rejects_result_and_error_together() {
        let both = r#"{"jsonrpc":"2.0","result":1,"error":{"code":1,"message":"x"}}"#;
        assert!(JsonRpcResponse::parse(both).is_err());
    }
}
