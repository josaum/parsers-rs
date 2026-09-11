//! JWK / JWT — structural split and base64url decode (no cryptography).
//!
//! A compact JWT is `header.payload.signature` (three base64url segments).
//! This module splits the token, decodes the header and payload into JSON, and
//! returns the signature segment raw. Cryptographic verification belongs to the
//! `jsonwebtoken` crate — this is the *parse* surface only.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::Value;

use crate::{JsonError, Result};

/// Decoded JWT parts.
#[derive(Debug, Clone, PartialEq)]
pub struct JwtParts {
    /// Decoded header object (e.g. `{"alg":"HS256","typ":"JWT"}`).
    pub header: Value,
    /// Decoded payload object (the claims).
    pub payload: Value,
    /// Raw base64url signature segment (undecoded).
    pub signature: String,
}

/// Split a compact JWT into header / payload / signature.
///
/// Tolerates trailing `=` padding on each segment. Fails loud on fewer or more
/// than three segments or on non-JSON header/payload.
pub fn split_jwt(token: &str) -> Result<JwtParts> {
    let mut parts = token.trim().split('.');
    let header_b64 = parts
        .next()
        .ok_or_else(|| JsonError::Structure("JWT has no header segment".into()))?;
    let payload_b64 = parts
        .next()
        .ok_or_else(|| JsonError::Structure("JWT has no payload segment".into()))?;
    let signature_b64 = parts
        .next()
        .ok_or_else(|| JsonError::Structure("JWT has no signature segment".into()))?;
    if parts.next().is_some() {
        return Err(JsonError::Structure(
            "JWT has more than three segments".into(),
        ));
    }

    let header_json = decode_b64url(header_b64)?;
    let payload_json = decode_b64url(payload_b64)?;

    Ok(JwtParts {
        header: crate::parse(&header_json)?,
        payload: crate::parse(&payload_json)?,
        signature: signature_b64.to_string(),
    })
}

fn decode_b64url(segment: &str) -> Result<String> {
    let unpadded = segment.trim_end_matches('=');
    let bytes = URL_SAFE_NO_PAD.decode(unpadded)?;
    String::from_utf8(bytes).map_err(|e| JsonError::Decode(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Header `{"alg":"HS256","typ":"JWT"}`, payload `{"sub":"123","name":"Ana"}`.
    const TOKEN: &str =
        "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjMiLCJuYW1lIjoiQW5hIn0.sig";

    #[test]
    fn splits_and_decodes() {
        let parts = split_jwt(TOKEN).unwrap();
        assert_eq!(parts.header["alg"], "HS256");
        assert_eq!(parts.payload["sub"], "123");
        assert_eq!(parts.payload["name"], "Ana");
        assert_eq!(parts.signature, "sig");
    }

    #[test]
    fn rejects_bad_segment_counts() {
        assert!(split_jwt("a.b").is_err());
        assert!(split_jwt("a.b.c.d").is_err());
    }
}
