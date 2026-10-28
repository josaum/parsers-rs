use std::time::{SystemTime, UNIX_EPOCH};

use flight_ipc_config::{AuthClaim, AuthConfig, FlightRequestAuthVerifier};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use tonic::Request;

const TEST_SECRET: &str = "office-flight-test-only-secret";

pub fn required_auth_verifier() -> FlightRequestAuthVerifier {
    FlightRequestAuthVerifier::new(AuthConfig {
        require_auth: true,
        issuer: None,
        audience: None,
        es256_public_key_pem: None,
        legacy_hs256_secret: Some(TEST_SECRET.to_owned()),
        allow_legacy_hs256: true,
    })
    .expect("valid test verifier")
}

pub fn with_valid_bearer<T>(mut request: Request<T>) -> Request<T> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after unix epoch")
        .as_secs();
    let claim = AuthClaim {
        sub: "office-flight-test".to_owned(),
        tenant_id: "test-tenant".to_owned(),
        roles: vec![],
        permissions: vec!["flight:invoke".to_owned()],
        iss: "test".to_owned(),
        aud: "test".to_owned(),
        iat: now,
        exp: now + 60,
        jti: "office-flight-test-jti".to_owned(),
        r#type: "access".to_owned(),
    };
    let token = encode(
        &Header::new(Algorithm::HS256),
        &claim,
        &EncodingKey::from_secret(TEST_SECRET.as_bytes()),
    )
    .expect("encode test JWT");
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {token}")
            .parse()
            .expect("valid authorization metadata"),
    );
    request
}
