//! Dependency-light authentication for Arrow Flight request metadata.

use std::{borrow::Cow, ffi::OsString, fmt};

use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tonic::{
    metadata::{Ascii, MetadataMap, MetadataValue},
    Request, Status,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AuthClaim {
    pub sub: String,
    pub tenant_id: String,
    #[serde(default)]
    pub roles: Vec<String>,
    #[serde(default)]
    pub permissions: Vec<String>,
    pub iss: String,
    pub aud: String,
    pub iat: u64,
    pub exp: u64,
    pub jti: String,
    #[serde(rename = "type")]
    pub r#type: String,
}

#[derive(Clone)]
pub struct AuthConfig {
    pub require_auth: bool,
    pub issuer: Option<String>,
    pub audience: Option<String>,
    pub es256_public_key_pem: Option<String>,
    pub legacy_hs256_secret: Option<String>,
    pub allow_legacy_hs256: bool,
}

impl fmt::Debug for AuthConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthConfig")
            .field("require_auth", &self.require_auth)
            .field("issuer", &self.issuer)
            .field("audience", &self.audience)
            .field(
                "es256_public_key_pem",
                &self.es256_public_key_pem.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "legacy_hs256_secret",
                &self.legacy_hs256_secret.as_ref().map(|_| "[REDACTED]"),
            )
            .field("allow_legacy_hs256", &self.allow_legacy_hs256)
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("missing bearer token")]
    MissingBearer,
    #[error("missing outbound authorization metadata")]
    MissingClientAuthorization,
    #[error("invalid outbound authorization metadata")]
    InvalidClientAuthorization,
    #[error("no configured signing material")]
    MissingConfig,
    #[error("invalid signing material or auth policy")]
    InvalidConfig,
    #[error("token validation failed")]
    InvalidToken,
}

/// Validated outbound Flight authorization metadata with redacted debug output.
#[derive(Clone)]
pub struct FlightAuthorizationMetadata {
    value: MetadataValue<Ascii>,
}

impl fmt::Debug for FlightAuthorizationMetadata {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("FlightAuthorizationMetadata")
            .field(&"[REDACTED]")
            .finish()
    }
}

impl FlightAuthorizationMetadata {
    /// Resolve optional metadata from already-loaded values.
    ///
    /// The explicit header wins. The bearer-token value is deliberately raw
    /// and receives exactly one canonical `Bearer ` prefix.
    pub fn from_values(
        explicit_header: Option<&str>,
        raw_bearer_token: Option<&str>,
    ) -> Result<Option<Self>, AuthError> {
        let explicit_header = explicit_header
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let raw_bearer_token = raw_bearer_token
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let header = if let Some(explicit_header) = explicit_header {
            Cow::Borrowed(explicit_header)
        } else if let Some(raw_bearer_token) = raw_bearer_token {
            Cow::Owned(format!("Bearer {raw_bearer_token}"))
        } else {
            return Ok(None);
        };
        let mut value = MetadataValue::try_from(header.as_ref())
            .map_err(|_| AuthError::InvalidClientAuthorization)?;
        value.set_sensitive(true);
        Ok(Some(Self { value }))
    }

    /// Resolve required metadata from already-loaded values.
    pub fn from_values_required(
        explicit_header: Option<&str>,
        raw_bearer_token: Option<&str>,
    ) -> Result<Self, AuthError> {
        Self::from_values(explicit_header, raw_bearer_token)?
            .ok_or(AuthError::MissingClientAuthorization)
    }

    /// Resolve optional metadata from the canonical process environment.
    pub fn from_env() -> Result<Option<Self>, AuthError> {
        Self::from_env_source(client_auth_env)
    }

    /// Resolve required metadata before a client attempts network I/O.
    pub fn from_env_required() -> Result<Self, AuthError> {
        Self::from_env()?.ok_or(AuthError::MissingClientAuthorization)
    }

    /// Insert exactly one canonical authorization value into an existing request.
    pub fn apply<T>(&self, request: &mut Request<T>) {
        request
            .metadata_mut()
            .insert("authorization", self.value.clone());
    }

    /// Build a request containing exactly one canonical authorization value.
    pub fn request<T>(&self, payload: T) -> Request<T> {
        let mut request = Request::new(payload);
        self.apply(&mut request);
        request
    }

    fn from_env_source<F>(mut read: F) -> Result<Option<Self>, AuthError>
    where
        F: FnMut(&str) -> Result<Option<String>, AuthError>,
    {
        let explicit_header = read("PARSERS_FLIGHT_AUTHORIZATION")?;
        if explicit_header
            .as_deref()
            .map(str::trim)
            .is_some_and(|value| !value.is_empty())
        {
            return Self::from_values(explicit_header.as_deref(), None);
        }
        let raw_bearer_token = read("PARSERS_FLIGHT_BEARER_TOKEN")?;
        Self::from_values(explicit_header.as_deref(), raw_bearer_token.as_deref())
    }
}

/// Mint a short-lived, tenant-scoped workload authorization for a co-located
/// Flight client. The returned metadata is redacted by its `Debug` contract;
/// callers cache it in memory and refresh before `expires_at`.
pub fn mint_local_workload_authorization(
    secret: &str,
    tenant_id: &str,
    subject: &str,
    now: u64,
) -> Result<(FlightAuthorizationMetadata, u64), AuthError> {
    let secret = secret.trim();
    if secret.len() < 32 {
        return Err(AuthError::InvalidConfig);
    }
    let (claims, expires_at) = workload_claims(tenant_id, subject, now)?;
    let token = encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|_| AuthError::InvalidConfig)?;
    let metadata = FlightAuthorizationMetadata::from_values_required(None, Some(&token))?;
    Ok((metadata, expires_at))
}

/// Mints a short-lived, tenant-scoped ES256 workload authorization.
///
/// Carries_claims of [`mint_local_workload_authorization`] signed with an ES256
/// private key (`BEGIN PRIVATE KEY` PKCS#8 P-256 PEM). Returns the redacted
/// metadata and its `exp` (`now + 300`).
pub fn mint_es256_workload_authorization(
    private_key_pem: &str,
    tenant_id: &str,
    subject: &str,
    now: u64,
) -> Result<(FlightAuthorizationMetadata, u64), AuthError> {
    let (claims, expires_at) = workload_claims(tenant_id, subject, now)?;
    let key = EncodingKey::from_ec_pem(private_key_pem.trim().as_bytes())
        .map_err(|_| AuthError::InvalidConfig)?;
    let token = encode(&Header::new(Algorithm::ES256), &claims, &key)
        .map_err(|_| AuthError::InvalidConfig)?;
    let metadata = FlightAuthorizationMetadata::from_values_required(None, Some(&token))?;
    Ok((metadata, expires_at))
}

const WORKLOAD_AUTHORIZATION_TTL_SECONDS: u64 = 300;

fn workload_claims(
    tenant_id: &str,
    subject: &str,
    now: u64,
) -> Result<(AuthClaim, u64), AuthError> {
    let tenant_id = tenant_id.trim();
    let subject = subject.trim();
    if tenant_id.is_empty() || tenant_id == "*" || subject.is_empty() {
        return Err(AuthError::InvalidConfig);
    }
    let expires_at = now.saturating_add(WORKLOAD_AUTHORIZATION_TTL_SECONDS);
    let claims = AuthClaim {
        sub: subject.to_owned(),
        tenant_id: tenant_id.to_owned(),
        roles: vec!["service".to_owned()],
        permissions: vec!["flight:invoke".to_owned()],
        iss: "parsers-api".to_owned(),
        aud: "parsers".to_owned(),
        iat: now,
        exp: expires_at,
        jti: uuid::Uuid::new_v4().to_string(),
        r#type: "access".to_owned(),
    };
    Ok((claims, expires_at))
}

fn client_auth_env(name: &str) -> Result<Option<String>, AuthError> {
    match std::env::var_os(name) {
        None => Ok(None),
        Some(value) => os_string_into_client_auth(value).map(Some),
    }
}

fn os_string_into_client_auth(value: OsString) -> Result<String, AuthError> {
    value
        .into_string()
        .map_err(|_| AuthError::InvalidClientAuthorization)
}

/// Immutable, startup-validated verifier for inbound Flight metadata.
#[derive(Debug, Clone)]
pub struct FlightRequestAuthVerifier {
    config: AuthConfig,
}

impl FlightRequestAuthVerifier {
    pub fn new(config: AuthConfig) -> Result<Self, AuthError> {
        if config.require_auth {
            let has_es256 = config
                .es256_public_key_pem
                .as_deref()
                .is_some_and(|pem| !pem.trim().is_empty());
            let has_hs256 = config
                .legacy_hs256_secret
                .as_deref()
                .is_some_and(|secret| !secret.trim().is_empty());
            if config.es256_public_key_pem.is_some() {
                let pem = config.es256_public_key_pem.as_deref().unwrap_or_default();
                DecodingKey::from_ec_pem(pem.as_bytes()).map_err(|_| AuthError::InvalidConfig)?;
            }
            if config.legacy_hs256_secret.is_some()
                && config
                    .legacy_hs256_secret
                    .as_deref()
                    .is_some_and(|secret| secret.trim().is_empty())
            {
                return Err(AuthError::InvalidConfig);
            }
            if !has_es256 && !(config.allow_legacy_hs256 && has_hs256) {
                return Err(AuthError::MissingConfig);
            }
        }
        Ok(Self { config })
    }

    /// Load Flight authentication settings from environment variables.
    pub fn from_env() -> Result<Self, AuthError> {
        let enabled = |name: &str| {
            std::env::var(name)
                .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes"))
                .unwrap_or(false)
        };
        let config = AuthConfig {
            require_auth: enabled("PARSERS_FLIGHT_REQUIRE_AUTH"),
            issuer: std::env::var("PARSERS_FLIGHT_ISSUER").ok(),
            audience: std::env::var("PARSERS_FLIGHT_AUDIENCE").ok(),
            es256_public_key_pem: std::env::var("PARSERS_JWT_PUBLIC_KEY_PEM")
                .ok()
                .or_else(|| {
                    std::env::var("PARSERS_JWT_PUBLIC_KEY_PATH")
                        .ok()
                        .and_then(|path| std::fs::read_to_string(path).ok())
                }),
            legacy_hs256_secret: std::env::var("JWT_SECRET").ok(),
            allow_legacy_hs256: enabled("PARSERS_FLIGHT_ALLOW_LEGACY_HS256"),
        };
        Self::new(config)
    }

    /// Load authentication for a surface advertising `ExistingAuth`.
    /// Such a surface must never start with authentication disabled.
    pub fn from_env_required() -> Result<Self, AuthError> {
        Self::from_env()?.require_enabled()
    }

    pub fn require_enabled(self) -> Result<Self, AuthError> {
        if !self.config.require_auth {
            return Err(AuthError::MissingConfig);
        }
        if self
            .config
            .issuer
            .as_deref()
            .is_none_or(|issuer| issuer.trim().is_empty())
            || self
                .config
                .audience
                .as_deref()
                .is_none_or(|audience| audience.trim().is_empty())
        {
            return Err(AuthError::MissingConfig);
        }
        Ok(self)
    }

    pub fn is_required(&self) -> bool {
        self.config.require_auth
    }

    /// Validate the single canonical `authorization` metadata value.
    /// Errors deliberately never include credentials or JWT decoder details.
    pub fn verify_metadata(&self, metadata: &MetadataMap) -> Result<(), Status> {
        self.verify_metadata_claim(metadata).map(|_| ())
    }

    /// Validate metadata and retain the claim for action-specific policy.
    pub fn verify_metadata_claim(
        &self,
        metadata: &MetadataMap,
    ) -> Result<Option<AuthClaim>, Status> {
        if !self.config.require_auth {
            return Ok(None);
        }
        let mut values = metadata.get_all("authorization").iter();
        let header = values
            .next()
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| Status::unauthenticated("missing authorization metadata"))?;
        if values.next().is_some() {
            return Err(Status::unauthenticated(
                "multiple authorization metadata values",
            ));
        }
        self.config
            .validate_bearer(header)
            .map(Some)
            .map_err(|_| Status::unauthenticated("invalid authorization metadata"))
    }
}

impl AuthConfig {
    pub fn disabled() -> Self {
        Self {
            require_auth: false,
            issuer: None,
            audience: None,
            es256_public_key_pem: None,
            legacy_hs256_secret: None,
            allow_legacy_hs256: false,
        }
    }

    pub fn validate_bearer(&self, header: &str) -> Result<AuthClaim, AuthError> {
        if !self.require_auth {
            return Err(AuthError::MissingConfig);
        }

        let token = header
            .strip_prefix("Bearer ")
            .or_else(|| header.strip_prefix("bearer "))
            .ok_or(AuthError::MissingBearer)?;

        if let Some(public_key) = &self.es256_public_key_pem {
            let validation = self.validation(Algorithm::ES256);
            if let Ok(data) = decode::<AuthClaim>(
                token,
                &DecodingKey::from_ec_pem(public_key.as_bytes())
                    .map_err(|_| AuthError::InvalidToken)?,
                &validation,
            ) {
                return Self::validate_claim_policy(data.claims);
            }
        }

        if self.allow_legacy_hs256 {
            if let Some(secret) = &self.legacy_hs256_secret {
                let validation = self.validation(Algorithm::HS256);
                if let Ok(data) = decode::<AuthClaim>(
                    token,
                    &DecodingKey::from_secret(secret.as_bytes()),
                    &validation,
                ) {
                    return Self::validate_claim_policy(data.claims);
                }
            }
        }

        Err(AuthError::InvalidToken)
    }

    fn validation(&self, algorithm: Algorithm) -> Validation {
        let mut validation = Validation::new(algorithm);
        if let Some(issuer) = &self.issuer {
            validation.set_issuer(&[issuer.as_str()]);
        }
        if let Some(audience) = &self.audience {
            validation.set_audience(&[audience.as_str()]);
        } else {
            validation.validate_aud = false;
        }
        validation
    }

    fn validate_claim_policy(claim: AuthClaim) -> Result<AuthClaim, AuthError> {
        if claim.tenant_id.trim().is_empty()
            || claim.r#type != "access"
            || !claim
                .permissions
                .iter()
                .any(|permission| permission == "flight:invoke")
        {
            return Err(AuthError::InvalidToken);
        }
        Ok(claim)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde::Serialize;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tonic::metadata::MetadataValue;

    const TEST_ES256_PRIVATE_KEY: &str = r#"-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgeaB24Vky0rFr3157
/5LmvrFTw1MgJCQKaYjdukbll4yhRANCAASiBWLWleqMhTEtyIGlhx9kQA+zmuG6
QYp2+MAKG9ymlOp5hkVR2AoO9PFRkds64Z8q5fhpnzNcfOqZwx5R7ZNN
-----END PRIVATE KEY-----"#;
    const TEST_ES256_PUBLIC_KEY: &str = r#"-----BEGIN PUBLIC KEY-----
MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEogVi1pXqjIUxLciBpYcfZEAPs5rh
ukGKdvjAChvcppTqeYZFUdgKDvTxUZHbOuGfKuX4aZ8zXHzqmcMeUe2TTQ==
-----END PUBLIC KEY-----"#;

    #[derive(Serialize)]
    struct CanonicalPythonAccessClaim {
        sub: &'static str,
        tenant_id: &'static str,
        roles: Vec<&'static str>,
        permissions: Vec<&'static str>,
        iss: &'static str,
        aud: &'static str,
        iat: u64,
        exp: u64,
        jti: &'static str,
        #[serde(rename = "type")]
        token_type: &'static str,
    }

    fn canonical_python_claim(now: u64) -> CanonicalPythonAccessClaim {
        CanonicalPythonAccessClaim {
            sub: "flight-service",
            tenant_id: "system",
            roles: vec!["service"],
            permissions: vec!["flight:invoke"],
            iss: "parsers-api",
            aud: "parsers",
            iat: now,
            exp: now + 60,
            jti: "python-service-jti",
            token_type: "access",
        }
    }

    fn verifier() -> FlightRequestAuthVerifier {
        FlightRequestAuthVerifier::new(AuthConfig {
            require_auth: true,
            issuer: Some("parsers-api".to_owned()),
            audience: Some("parsers".to_owned()),
            es256_public_key_pem: None,
            legacy_hs256_secret: Some("test-only-secret".to_owned()),
            allow_legacy_hs256: true,
        })
        .unwrap()
    }

    #[test]
    fn required_auth_fails_closed_without_signing_material() {
        let error = FlightRequestAuthVerifier::new(AuthConfig {
            require_auth: true,
            ..AuthConfig::disabled()
        })
        .unwrap_err();
        assert!(matches!(error, AuthError::MissingConfig));
    }

    #[test]
    fn required_auth_rejects_malformed_or_empty_signing_material_at_startup() {
        let malformed = FlightRequestAuthVerifier::new(AuthConfig {
            require_auth: true,
            es256_public_key_pem: Some("not-a-pem".to_owned()),
            ..AuthConfig::disabled()
        })
        .unwrap_err();
        assert!(matches!(malformed, AuthError::InvalidConfig));

        let empty_hs = FlightRequestAuthVerifier::new(AuthConfig {
            require_auth: true,
            allow_legacy_hs256: true,
            legacy_hs256_secret: Some("  ".to_owned()),
            ..AuthConfig::disabled()
        })
        .unwrap_err();
        assert!(matches!(empty_hs, AuthError::InvalidConfig));
    }

    #[test]
    fn auth_configuration_and_verifier_debug_never_expose_hs256_secret() {
        let secret = "debug-must-not-leak-this-hs256-secret";
        let config = AuthConfig {
            require_auth: true,
            issuer: Some("parsers-api".to_owned()),
            audience: Some("parsers".to_owned()),
            es256_public_key_pem: None,
            legacy_hs256_secret: Some(secret.to_owned()),
            allow_legacy_hs256: true,
        };

        let config_debug = format!("{config:?}");
        assert!(!config_debug.contains(secret));
        assert!(config_debug.contains("[REDACTED]"));

        let verifier = FlightRequestAuthVerifier::new(config).unwrap();
        let verifier_debug = format!("{verifier:?}");
        assert!(!verifier_debug.contains(secret));
        assert!(verifier_debug.contains("[REDACTED]"));
    }

    #[test]
    fn existing_auth_surface_rejects_disabled_verifier() {
        let error = FlightRequestAuthVerifier::new(AuthConfig::disabled())
            .unwrap()
            .require_enabled()
            .unwrap_err();
        assert!(matches!(error, AuthError::MissingConfig));
    }

    #[test]
    fn existing_auth_surface_requires_issuer_and_audience() {
        let error = FlightRequestAuthVerifier::new(AuthConfig {
            require_auth: true,
            es256_public_key_pem: None,
            legacy_hs256_secret: Some("test-only-secret".to_owned()),
            allow_legacy_hs256: true,
            issuer: None,
            audience: Some("parsers".to_owned()),
        })
        .unwrap()
        .require_enabled()
        .unwrap_err();
        assert!(matches!(error, AuthError::MissingConfig));
    }

    #[test]
    fn metadata_requires_valid_bearer_and_redacts_failure() {
        let verifier = verifier();
        let missing = verifier.verify_metadata(&MetadataMap::new()).unwrap_err();
        assert_eq!(missing.code(), tonic::Code::Unauthenticated);

        let mut invalid = MetadataMap::new();
        invalid.insert(
            "authorization",
            MetadataValue::from_static("Bearer secret-token"),
        );
        let error = verifier.verify_metadata(&invalid).unwrap_err();
        assert_eq!(error.message(), "invalid authorization metadata");
        assert!(!error.message().contains("secret-token"));

        let mut duplicate = MetadataMap::new();
        duplicate.append("authorization", MetadataValue::from_static("Bearer one"));
        duplicate.append("authorization", MetadataValue::from_static("Bearer two"));
        let error = verifier.verify_metadata(&duplicate).unwrap_err();
        assert_eq!(error.code(), tonic::Code::Unauthenticated);
        assert_eq!(error.message(), "multiple authorization metadata values");
    }

    #[test]
    fn metadata_accepts_controlled_hs256_bearer() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let claim = AuthClaim {
            sub: "service".into(),
            tenant_id: "internal".into(),
            roles: vec![],
            permissions: vec!["flight:invoke".into()],
            iss: "parsers-api".into(),
            aud: "parsers".into(),
            iat: now,
            exp: now + 60,
            jti: "test-jti".into(),
            r#type: "access".into(),
        };
        let token = encode(
            &Header::new(Algorithm::HS256),
            &claim,
            &EncodingKey::from_secret(b"test-only-secret"),
        )
        .unwrap();
        let mut metadata = MetadataMap::new();
        metadata.insert("authorization", format!("Bearer {token}").parse().unwrap());
        let verified = verifier()
            .verify_metadata_claim(&metadata)
            .unwrap()
            .unwrap();
        assert_eq!(verified, claim);
    }

    fn verify_hs256_claim(claim: &CanonicalPythonAccessClaim) -> Result<(), Status> {
        let token = encode(
            &Header::new(Algorithm::HS256),
            claim,
            &EncodingKey::from_secret(b"test-only-secret"),
        )
        .unwrap();
        let mut metadata = MetadataMap::new();
        metadata.insert("authorization", format!("Bearer {token}").parse().unwrap());
        verifier().verify_metadata(&metadata)
    }

    #[test]
    fn canonical_python_service_token_does_not_require_duplicate_namespace_claim() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let token = encode(
            &Header::new(Algorithm::HS256),
            &canonical_python_claim(now),
            &EncodingKey::from_secret(b"test-only-secret"),
        )
        .unwrap();
        let mut metadata = MetadataMap::new();
        metadata.insert("authorization", format!("Bearer {token}").parse().unwrap());

        verifier().verify_metadata(&metadata).unwrap();
    }

    #[test]
    fn canonical_claim_requires_nonempty_tenant_access_type_and_flight_permission() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let mut empty_tenant = canonical_python_claim(now);
        empty_tenant.tenant_id = "  ";
        assert!(verify_hs256_claim(&empty_tenant).is_err());

        let mut refresh = canonical_python_claim(now);
        refresh.token_type = "refresh";
        assert!(verify_hs256_claim(&refresh).is_err());

        let mut missing_permission = canonical_python_claim(now);
        missing_permission.permissions.clear();
        assert!(verify_hs256_claim(&missing_permission).is_err());
    }

    #[test]
    fn controlled_hs256_enforces_configured_canonical_issuer_and_audience() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let mut wrong_issuer = canonical_python_claim(now);
        wrong_issuer.iss = "other-issuer";
        assert!(verify_hs256_claim(&wrong_issuer).is_err());

        let mut wrong_audience = canonical_python_claim(now);
        wrong_audience.aud = "other-audience";
        assert!(verify_hs256_claim(&wrong_audience).is_err());
    }

    #[test]
    fn canonical_python_service_token_is_accepted_with_es256() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        verify_es256_claim(&canonical_python_claim(now)).unwrap();
    }

    fn verify_es256_claim(claim: &CanonicalPythonAccessClaim) -> Result<(), Status> {
        let token = encode(
            &Header::new(Algorithm::ES256),
            claim,
            &EncodingKey::from_ec_pem(TEST_ES256_PRIVATE_KEY.as_bytes()).unwrap(),
        )
        .unwrap();
        let mut metadata = MetadataMap::new();
        metadata.insert("authorization", format!("Bearer {token}").parse().unwrap());
        let verifier = FlightRequestAuthVerifier::new(AuthConfig {
            require_auth: true,
            issuer: Some("parsers-api".to_owned()),
            audience: Some("parsers".to_owned()),
            es256_public_key_pem: Some(TEST_ES256_PUBLIC_KEY.to_owned()),
            legacy_hs256_secret: None,
            allow_legacy_hs256: false,
        })
        .unwrap();

        verifier.verify_metadata(&metadata)
    }

    #[test]
    fn es256_enforces_configured_canonical_issuer_and_audience() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let mut wrong_issuer = canonical_python_claim(now);
        wrong_issuer.iss = "other-issuer";
        assert!(verify_es256_claim(&wrong_issuer).is_err());

        let mut wrong_audience = canonical_python_claim(now);
        wrong_audience.aud = "other-audience";
        assert!(verify_es256_claim(&wrong_audience).is_err());
    }

    #[test]
    fn es256_workload_authorization_is_accepted_by_es256_only_verifier() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let (authorization, expires_at) = mint_es256_workload_authorization(
            TEST_ES256_PRIVATE_KEY,
            "tenant-a",
            "parsers-worker",
            now,
        )
        .unwrap();
        assert_eq!(expires_at, now + 300);

        let request = authorization.request(());
        let header = request
            .metadata()
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap();
        let token = header.strip_prefix("Bearer ").unwrap();
        assert_eq!(
            jsonwebtoken::decode_header(token).unwrap().alg,
            Algorithm::ES256
        );

        let verifier = FlightRequestAuthVerifier::new(AuthConfig {
            require_auth: true,
            issuer: Some("parsers-api".to_owned()),
            audience: Some("parsers".to_owned()),
            es256_public_key_pem: Some(TEST_ES256_PUBLIC_KEY.to_owned()),
            legacy_hs256_secret: None,
            allow_legacy_hs256: false,
        })
        .unwrap();
        let claim = verifier
            .verify_metadata_claim(request.metadata())
            .unwrap()
            .unwrap();
        assert_eq!(claim.sub, "parsers-worker");
        assert_eq!(claim.tenant_id, "tenant-a");
        assert_eq!(claim.roles, ["service"]);
        assert_eq!(claim.permissions, ["flight:invoke"]);
        assert_eq!(claim.iss, "parsers-api");
        assert_eq!(claim.aud, "parsers");
        assert_eq!(claim.iat, now);
        assert_eq!(claim.exp, now + 300);
        assert_eq!(claim.r#type, "access");
        assert!(uuid::Uuid::parse_str(&claim.jti).is_ok());
    }

    #[test]
    fn es256_workload_authorization_rejects_wildcard_tenant_empty_subject_and_bad_keys() {
        let now = 1_700_000_000;
        let mint = |pem: &str, tenant: &str, subject: &str| {
            mint_es256_workload_authorization(pem, tenant, subject, now)
        };
        for error in [
            mint(TEST_ES256_PRIVATE_KEY, "*", "parsers-worker"),
            mint(TEST_ES256_PRIVATE_KEY, "  ", "parsers-worker"),
            mint(TEST_ES256_PRIVATE_KEY, "tenant-a", "  "),
            mint("not-a-pem", "tenant-a", "parsers-worker"),
            mint(TEST_ES256_PUBLIC_KEY, "tenant-a", "parsers-worker"),
        ]
        .map(Result::unwrap_err)
        {
            assert!(matches!(error, AuthError::InvalidConfig));
        }
    }

    #[test]
    fn outbound_authorization_prefers_explicit_header_then_wraps_raw_bearer() {
        let explicit = FlightAuthorizationMetadata::from_values_required(
            Some("  Bearer explicit-token  "),
            Some("fallback-token"),
        )
        .unwrap();
        let explicit_request = explicit.request(());
        assert_eq!(
            explicit_request
                .metadata()
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer explicit-token"
        );
        assert!(explicit_request
            .metadata()
            .get("authorization")
            .unwrap()
            .is_sensitive());
        assert!(!format!("{explicit_request:?}").contains("explicit-token"));

        let fallback =
            FlightAuthorizationMetadata::from_values_required(None, Some(" raw-token ")).unwrap();
        let fallback_request = fallback.request(());
        assert_eq!(
            fallback_request
                .metadata()
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer raw-token"
        );
    }

    #[test]
    fn outbound_environment_precedence_never_reads_an_unused_fallback() {
        let mut reads = Vec::new();
        let authorization = FlightAuthorizationMetadata::from_env_source(|name| {
            reads.push(name.to_owned());
            match name {
                "PARSERS_FLIGHT_AUTHORIZATION" => Ok(Some("Bearer explicit-token".to_owned())),
                "PARSERS_FLIGHT_BEARER_TOKEN" => Err(AuthError::InvalidClientAuthorization),
                _ => unreachable!(),
            }
        })
        .unwrap()
        .unwrap();

        assert_eq!(reads, ["PARSERS_FLIGHT_AUTHORIZATION"]);
        assert_eq!(
            authorization
                .request(())
                .metadata()
                .get("authorization")
                .unwrap(),
            "Bearer explicit-token"
        );
    }

    #[test]
    fn outbound_authorization_required_variant_fails_closed_when_missing() {
        assert!(FlightAuthorizationMetadata::from_values(None, None)
            .unwrap()
            .is_none());
        let error = FlightAuthorizationMetadata::from_values_required(None, None).unwrap_err();
        assert!(matches!(error, AuthError::MissingClientAuthorization));
    }

    #[test]
    fn outbound_authorization_rejects_controls_and_redacts_debug_and_errors() {
        let secret = "sensitive-client-token";
        let authorization = FlightAuthorizationMetadata::from_values_required(
            Some(&format!("Bearer {secret}")),
            None,
        )
        .unwrap();
        assert!(!format!("{authorization:?}").contains(secret));

        let error = FlightAuthorizationMetadata::from_values_required(
            Some(&format!("Bearer {secret}\nsmuggled: value")),
            None,
        )
        .unwrap_err();
        assert!(matches!(error, AuthError::InvalidClientAuthorization));
        assert!(!error.to_string().contains(secret));
        assert!(!format!("{error:?}").contains(secret));
    }

    #[test]
    fn applying_outbound_authorization_collapses_duplicates_to_exactly_one() {
        let authorization =
            FlightAuthorizationMetadata::from_values_required(Some("Bearer canonical-token"), None)
                .unwrap();
        let mut request = Request::new(());
        request.metadata_mut().append(
            "authorization",
            MetadataValue::from_static("Bearer stale-one"),
        );
        request.metadata_mut().append(
            "authorization",
            MetadataValue::from_static("Bearer stale-two"),
        );

        authorization.apply(&mut request);
        authorization.apply(&mut request);

        let values = request
            .metadata()
            .get_all("authorization")
            .iter()
            .collect::<Vec<_>>();
        assert_eq!(values.len(), 1);
        assert_eq!(values[0], "Bearer canonical-token");
    }
}
