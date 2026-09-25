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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    /// Request identifier (echoed back on the response).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
}

impl JsonRpcRequest {
    /// Parse and validate a JSON-RPC 2.0 request envelope.
    ///
    /// Fails loud when `jsonrpc` is not `"2.0"` or `method` is missing/empty.
    pub fn parse(input: &str) -> Result<Self> {
        let request: JsonRpcRequest = crate::parse(input)?;
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
        if self.method.is_empty() {
            return Err(JsonError::Structure(
                "jsonrpc request method is empty".into(),
            ));
        }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// A JSON-RPC 2.0 response object (result XOR error).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct JsonRpcResponse {
    /// Must be `"2.0"`.
    pub jsonrpc: String,
    /// Present on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Present on failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
    /// Echoes the request identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
}

impl JsonRpcResponse {
    /// Parse and validate a JSON-RPC 2.0 response envelope.
    ///
    /// Fails loud when `jsonrpc` is not `"2.0"`, or when both `result` and
    /// `error` are present (a response must be exactly one of them).
    pub fn parse(input: &str) -> Result<Self> {
        let response: JsonRpcResponse = crate::parse(input)?;
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
        Ok(())
    }
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
    fn rejects_wrong_version_and_empty_method() {
        assert!(JsonRpcRequest::parse(r#"{"jsonrpc":"1.0","method":"m"}"#).is_err());
        assert!(JsonRpcRequest::parse(r#"{"jsonrpc":"2.0","method":""}"#).is_err());
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
