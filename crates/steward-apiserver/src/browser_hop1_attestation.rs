//! Internal-only browser-session to Identity HOP-1 attestation boundary.
//!
//! A browser session is authority only inside Steward. This module turns the exact canonical
//! principal selected by Steward's session middleware into one short-lived ES256 assertion for
//! Identity's private workload endpoint. Neither browser state nor the resulting HOP-1 bearer
//! crosses a browser-facing route.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::http::header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{
    Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode,
    jwk::{JwkSet, KeyAlgorithm, PublicKeyUse},
};
use p256::SecretKey;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::pkcs8::DecodePrivateKey;
use reqwest::{Certificate, Client, Url};
use serde::{Deserialize, Serialize};
use steward_types::Email;
use uuid::Uuid;

use crate::BoxFuture;
use crate::browser_auth::{BrowserPrincipal, BrowserSessionBinding};
use crate::connections::ConnectionBrokerError;
use crate::connections::ConnectionSession;
use crate::mcp_gw_connections::{BrowserMcpGwBearerIssuer, McpGwBearer};

/// The only browser-session operation Identity v1 accepts.
pub const GITHUB_OAUTH_CONNECT_OPERATION: &str = "github_oauth_connect";
const IDENTITY_BROWSER_HOP1_PATH: &str = "/v1/browser-hop1/exchange";
const MAX_ASSERTION_LIFETIME_SECONDS: u64 = 60;
const MAX_ASSERTION_BYTES: usize = 8 * 1024;
const MAX_RESPONSE_BYTES: usize = 32 * 1024;
const MAX_WORKLOAD_TOKEN_BYTES: usize = 8 * 1024;

/// Publicly safe failure class for the internal attestation boundary.
///
/// Deliberately does not carry provider bodies, browser state, workload tokens, assertions, or
/// the resulting bearer; callers can only fail closed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserHop1AttestationError {
    Unavailable,
}

impl std::fmt::Display for BrowserHop1AttestationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("browser HOP-1 attestation is unavailable")
    }
}

impl std::error::Error for BrowserHop1AttestationError {}

/// One private Identity HOP-1 bearer. It intentionally implements neither `Debug` nor `Display`.
pub struct BrowserHop1Bearer(String);

impl BrowserHop1Bearer {
    fn new(value: String) -> Result<Self, BrowserHop1AttestationError> {
        if value.is_empty()
            || value.len() > MAX_ASSERTION_BYTES
            || value.chars().any(char::is_whitespace)
        {
            return Err(BrowserHop1AttestationError::Unavailable);
        }
        Ok(Self(value))
    }

    /// Transfers the bearer only to Steward's server-side MCP-GW adapter.
    fn into_inner(self) -> String {
        self.0
    }
}

/// Immutable deployment inputs for signing browser-to-Identity assertions.
///
/// The private PKCS#8 material is accepted only if it can sign an ES256 proof which verifies with
/// the exact configured public JWKS key. This makes key projection and Identity's pinned JWKS one
/// contract rather than two independent values that can silently drift.
pub struct BrowserHop1AttestationConfig {
    issuer: String,
    assertion_audience: String,
    key_id: String,
    signing_key: EncodingKey,
    public_jwks: String,
}

impl BrowserHop1AttestationConfig {
    /// Construct a signer from a read-only PKCS#8 P-256 key and its deployment-projected JWKS.
    pub fn from_pkcs8_der_and_jwks(
        issuer: String,
        assertion_audience: String,
        key_id: String,
        signing_key: &[u8],
        public_jwks: &str,
    ) -> Result<Self, BrowserHop1AttestationError> {
        if !valid_https_issuer(&issuer)
            || !bounded_non_whitespace(&assertion_audience, 256)
            || !bounded_non_whitespace(&key_id, 128)
            || signing_key.is_empty()
            || signing_key.len() > MAX_RESPONSE_BYTES
            || public_jwks.len() > MAX_RESPONSE_BYTES
        {
            return Err(BrowserHop1AttestationError::Unavailable);
        }
        let derived_public_jwks = derive_public_jwks(signing_key, &key_id)?;
        let public: JwkSet = serde_json::from_str(public_jwks)
            .map_err(|_| BrowserHop1AttestationError::Unavailable)?;
        let public_keys = public
            .keys
            .iter()
            .filter(|key| key.common.key_id.as_deref() == Some(key_id.as_str()))
            .collect::<Vec<_>>();
        if public_keys.len() != 1 {
            return Err(BrowserHop1AttestationError::Unavailable);
        }
        let public_key = public_keys[0];
        if public_key.common.key_algorithm != Some(KeyAlgorithm::ES256)
            || public_key.common.public_key_use != Some(PublicKeyUse::Signature)
        {
            return Err(BrowserHop1AttestationError::Unavailable);
        }
        let verification_key = DecodingKey::from_jwk(public_key)
            .map_err(|_| BrowserHop1AttestationError::Unavailable)?;
        let candidate = Self {
            issuer,
            assertion_audience,
            key_id,
            signing_key: EncodingKey::from_ec_der(signing_key),
            public_jwks: derived_public_jwks,
        };
        candidate.verify_signer_matches_jwks(&verification_key)?;
        Ok(candidate)
    }

    /// Load signer and public verification material from deployment-projected read-only files.
    pub fn from_files(
        issuer: String,
        assertion_audience: String,
        key_id: String,
        signing_key_file: &Path,
        public_jwks_file: &Path,
    ) -> Result<Self, BrowserHop1AttestationError> {
        let signing_key =
            fs::read(signing_key_file).map_err(|_| BrowserHop1AttestationError::Unavailable)?;
        let public_jwks = fs::read_to_string(public_jwks_file)
            .map_err(|_| BrowserHop1AttestationError::Unavailable)?;
        Self::from_pkcs8_der_and_jwks(
            issuer,
            assertion_audience,
            key_id,
            &signing_key,
            &public_jwks,
        )
    }

    /// The non-secret public key-set that Identity pins through its own projected ConfigMap.
    pub fn public_jwks(&self) -> &str {
        &self.public_jwks
    }

    fn sign(&self, principal: &BrowserPrincipal) -> Result<String, BrowserHop1AttestationError> {
        let email = verified_email(principal.display_email.as_str())?;
        let now = unix_seconds()?;
        let exp = now
            .checked_add(MAX_ASSERTION_LIFETIME_SECONDS)
            .ok_or(BrowserHop1AttestationError::Unavailable)?;
        let mut header = Header::new(Algorithm::ES256);
        header.typ = Some("JWT".to_owned());
        header.kid = Some(self.key_id.clone());
        let assertion = BrowserHop1RequestClaims {
            iss: self.issuer.clone(),
            sub: principal.canonical_user_id.as_str().to_owned(),
            aud: self.assertion_audience.clone(),
            exp,
            iat: now,
            nbf: now,
            jti: Uuid::new_v4().simple().to_string(),
            email,
            email_verified: true,
            operation: GITHUB_OAUTH_CONNECT_OPERATION,
            operation_id: format!("op_{}", Uuid::new_v4().simple()),
        };
        let encoded = encode(&header, &assertion, &self.signing_key)
            .map_err(|_| BrowserHop1AttestationError::Unavailable)?;
        if encoded.len() > MAX_ASSERTION_BYTES {
            return Err(BrowserHop1AttestationError::Unavailable);
        }
        Ok(encoded)
    }

    fn verify_signer_matches_jwks(
        &self,
        verification_key: &DecodingKey,
    ) -> Result<(), BrowserHop1AttestationError> {
        let now = unix_seconds()?;
        let mut header = Header::new(Algorithm::ES256);
        header.typ = Some("JWT".to_owned());
        header.kid = Some(self.key_id.clone());
        let proof = encode(
            &header,
            &SignerProof {
                iss: self.issuer.clone(),
                sub: "usr_00000000000000000000000000000000".to_owned(),
                aud: self.assertion_audience.clone(),
                exp: now
                    .checked_add(MAX_ASSERTION_LIFETIME_SECONDS)
                    .ok_or(BrowserHop1AttestationError::Unavailable)?,
                iat: now,
                nbf: now,
                jti: "signer-proof".to_owned(),
            },
            &self.signing_key,
        )
        .map_err(|_| BrowserHop1AttestationError::Unavailable)?;
        let mut validation = Validation::new(Algorithm::ES256);
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&[&self.assertion_audience]);
        validation.validate_nbf = true;
        validation.set_required_spec_claims(&["iss", "sub", "aud", "exp", "iat", "nbf", "jti"]);
        decode::<SignerProof>(&proof, verification_key, &validation)
            .map_err(|_| BrowserHop1AttestationError::Unavailable)?;
        Ok(())
    }
}

/// Exact private Identity workload endpoint; public, browser-facing endpoints are invalid.
#[derive(Clone)]
pub struct IdentityBrowserHop1Endpoint(Url);

impl IdentityBrowserHop1Endpoint {
    pub fn new(value: String) -> Result<Self, BrowserHop1AttestationError> {
        let endpoint = Url::parse(&value).map_err(|_| BrowserHop1AttestationError::Unavailable)?;
        if endpoint.scheme() != "https"
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.path() != IDENTITY_BROWSER_HOP1_PATH
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            return Err(BrowserHop1AttestationError::Unavailable);
        }
        Ok(Self(endpoint))
    }
}

/// Source for the rotated projected Kubernetes service-account token.
#[derive(Clone)]
pub struct ProjectedServiceAccountTokenFile(PathBuf);

impl ProjectedServiceAccountTokenFile {
    pub fn new(path: PathBuf) -> Result<Self, BrowserHop1AttestationError> {
        if path.as_os_str().is_empty() {
            return Err(BrowserHop1AttestationError::Unavailable);
        }
        Ok(Self(path))
    }

    fn read(&self) -> Result<WorkloadBearer, BrowserHop1AttestationError> {
        let value =
            fs::read_to_string(&self.0).map_err(|_| BrowserHop1AttestationError::Unavailable)?;
        WorkloadBearer::new(value.trim().to_owned())
    }
}

/// Non-browser internal client for Identity's workload listener.
#[derive(Clone)]
pub struct IdentityBrowserHop1Client {
    endpoint: IdentityBrowserHop1Endpoint,
    client: Client,
}

impl IdentityBrowserHop1Client {
    pub fn new(
        endpoint: IdentityBrowserHop1Endpoint,
        ca_certificate_pem: Vec<u8>,
    ) -> Result<Self, BrowserHop1AttestationError> {
        let ca_certificate = Certificate::from_pem(&ca_certificate_pem)
            .map_err(|_| BrowserHop1AttestationError::Unavailable)?;
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(10))
            .add_root_certificate(ca_certificate)
            .build()
            .map_err(|_| BrowserHop1AttestationError::Unavailable)?;
        Ok(Self { endpoint, client })
    }
}

/// Private exchange port, made injectable only to unit-test fail-closed request handling.
pub trait BrowserHop1ExchangeClient: Clone + Send + Sync + 'static {
    fn exchange<'a>(
        &'a self,
        workload_bearer: &'a WorkloadBearer,
        assertion: &'a str,
    ) -> BoxFuture<'a, Result<BrowserHop1Bearer, BrowserHop1AttestationError>>;
}

impl BrowserHop1ExchangeClient for IdentityBrowserHop1Client {
    fn exchange<'a>(
        &'a self,
        workload_bearer: &'a WorkloadBearer,
        assertion: &'a str,
    ) -> BoxFuture<'a, Result<BrowserHop1Bearer, BrowserHop1AttestationError>> {
        Box::pin(async move {
            let authorization = format!("Bearer {}", workload_bearer.as_str());
            let response = self
                .client
                .post(self.endpoint.0.clone())
                .header(AUTHORIZATION, authorization)
                .header(CONTENT_TYPE, "application/jwt")
                .body(assertion.to_owned())
                .send()
                .await
                .map_err(|_| BrowserHop1AttestationError::Unavailable)?;
            if response.status() != reqwest::StatusCode::OK
                || !response
                    .headers()
                    .get(CACHE_CONTROL)
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(has_no_store)
                || !response
                    .headers()
                    .get(CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|value| value.starts_with("application/json"))
            {
                return Err(BrowserHop1AttestationError::Unavailable);
            }
            let bytes = bounded_response(response).await?;
            let token: IdentityBearerResponse = serde_json::from_slice(&bytes)
                .map_err(|_| BrowserHop1AttestationError::Unavailable)?;
            if token.token_type != "Bearer"
                || token.expires_in == 0
                || token.expires_in > MAX_ASSERTION_LIFETIME_SECONDS
            {
                return Err(BrowserHop1AttestationError::Unavailable);
            }
            BrowserHop1Bearer::new(token.access_token)
        })
    }
}

/// Issues the transient HOP-1 only for a browser session whose opaque binding was constructed by
/// Steward's authenticated browser middleware.
#[derive(Clone)]
pub struct BrowserHop1AttestationIssuer<C> {
    config: Arc<BrowserHop1AttestationConfig>,
    service_account_token: ProjectedServiceAccountTokenFile,
    client: C,
}

impl<C> BrowserHop1AttestationIssuer<C>
where
    C: BrowserHop1ExchangeClient,
{
    pub fn new(
        config: BrowserHop1AttestationConfig,
        service_account_token: ProjectedServiceAccountTokenFile,
        client: C,
    ) -> Self {
        Self {
            config: Arc::new(config),
            service_account_token,
            client,
        }
    }

    /// Issue for the exact principal and opaque binding produced by the protected Connections
    /// BFF. No browser request can provide this type, a bearer, a subject, or operation fields.
    pub async fn issue(
        &self,
        session: &ConnectionSession<BrowserSessionBinding>,
    ) -> Result<BrowserHop1Bearer, BrowserHop1AttestationError> {
        let assertion = self.config.sign_browser_connection(session)?;
        let workload_bearer = self.service_account_token.read()?;
        self.client.exchange(&workload_bearer, &assertion).await
    }
}

impl<C> BrowserMcpGwBearerIssuer<BrowserSessionBinding> for BrowserHop1AttestationIssuer<C>
where
    C: BrowserHop1ExchangeClient,
{
    fn issue<'a>(
        &'a self,
        session: &'a ConnectionSession<BrowserSessionBinding>,
    ) -> BoxFuture<'a, Result<McpGwBearer, ConnectionBrokerError>> {
        Box::pin(async move {
            let bearer = BrowserHop1AttestationIssuer::issue(self, session)
                .await
                .map_err(|_| ConnectionBrokerError::Unavailable)?;
            McpGwBearer::new(bearer.into_inner()).map_err(|_| ConnectionBrokerError::Unavailable)
        })
    }
}

impl BrowserHop1AttestationConfig {
    fn sign_browser_connection(
        &self,
        session: &ConnectionSession<BrowserSessionBinding>,
    ) -> Result<String, BrowserHop1AttestationError> {
        // Keeping the opaque binding in this signature prevents an accidental call site from
        // minting browser authority from a configured user or arbitrary connection session.
        let _binding = &session.binding;
        self.sign(&BrowserPrincipal {
            canonical_user_id: session.subject.canonical_user_id.clone(),
            display_name: session.subject.display_email.clone(),
            display_email: Email::parse(session.subject.display_email.clone())
                .map_err(|_| BrowserHop1AttestationError::Unavailable)?,
            role: crate::browser_auth::BrowserRole::User,
            member_roles: Vec::new(),
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityBearerResponse {
    access_token: String,
    token_type: String,
    expires_in: u64,
}

/// Bearer used only for the immediately adjacent Identity private workload request.
pub struct WorkloadBearer(String);

impl WorkloadBearer {
    fn new(value: String) -> Result<Self, BrowserHop1AttestationError> {
        if !bounded_non_whitespace(&value, MAX_WORKLOAD_TOKEN_BYTES) {
            return Err(BrowserHop1AttestationError::Unavailable);
        }
        Ok(Self(value))
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BrowserHop1RequestClaims {
    iss: String,
    sub: String,
    aud: String,
    exp: u64,
    iat: u64,
    nbf: u64,
    jti: String,
    email: String,
    email_verified: bool,
    operation: &'static str,
    operation_id: String,
}

#[derive(Deserialize, Serialize)]
struct SignerProof {
    iss: String,
    sub: String,
    aud: String,
    exp: u64,
    iat: u64,
    nbf: u64,
    jti: String,
}

fn valid_https_issuer(value: &str) -> bool {
    Url::parse(value).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
    })
}

fn verified_email(value: &str) -> Result<String, BrowserHop1AttestationError> {
    if value != value.to_ascii_lowercase() {
        return Err(BrowserHop1AttestationError::Unavailable);
    }
    Email::parse(value.to_owned())
        .map(|email| email.0)
        .map_err(|_| BrowserHop1AttestationError::Unavailable)
}

fn bounded_non_whitespace(value: &str, max_len: usize) -> bool {
    !value.is_empty() && value.len() <= max_len && !value.chars().any(char::is_whitespace)
}

fn unix_seconds() -> Result<u64, BrowserHop1AttestationError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| BrowserHop1AttestationError::Unavailable)
}

fn has_no_store(value: &str) -> bool {
    value
        .split(',')
        .any(|directive| directive.trim().eq_ignore_ascii_case("no-store"))
}

fn derive_public_jwks(
    signing_key: &[u8],
    key_id: &str,
) -> Result<String, BrowserHop1AttestationError> {
    let private = SecretKey::from_pkcs8_der(signing_key)
        .map_err(|_| BrowserHop1AttestationError::Unavailable)?;
    let point = private.public_key().to_encoded_point(false);
    let x = point.x().ok_or(BrowserHop1AttestationError::Unavailable)?;
    let y = point.y().ok_or(BrowserHop1AttestationError::Unavailable)?;
    serde_json::to_string(&serde_json::json!({
        "keys": [{
            "kty": "EC",
            "use": "sig",
            "alg": "ES256",
            "kid": key_id,
            "crv": "P-256",
            "x": URL_SAFE_NO_PAD.encode(x),
            "y": URL_SAFE_NO_PAD.encode(y),
        }]
    }))
    .map_err(|_| BrowserHop1AttestationError::Unavailable)
}

async fn bounded_response(
    mut response: reqwest::Response,
) -> Result<Vec<u8>, BrowserHop1AttestationError> {
    if response
        .content_length()
        .is_some_and(|length| length as usize > MAX_RESPONSE_BYTES)
    {
        return Err(BrowserHop1AttestationError::Unavailable);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| BrowserHop1AttestationError::Unavailable)?
    {
        let total = bytes
            .len()
            .checked_add(chunk.len())
            .ok_or(BrowserHop1AttestationError::Unavailable)?;
        if total > MAX_RESPONSE_BYTES {
            return Err(BrowserHop1AttestationError::Unavailable);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
    use p256::SecretKey;
    use p256::elliptic_curve::sec1::ToEncodedPoint;
    use p256::pkcs8::EncodePrivateKey;
    use rand_core::OsRng;
    use steward_types::CanonicalUserId;

    use super::*;
    use crate::browser_auth::{BrowserPrincipal, BrowserRole};

    const ISSUER: &str = "https://steward.example.test";
    const AUDIENCE: &str = "identity-browser-hop1";
    const KEY_ID: &str = "steward-browser-hop1-current";

    struct RemoveFileOnDrop(std::path::PathBuf);

    impl Drop for RemoveFileOnDrop {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    fn signer_material() -> Result<(Vec<u8>, String, DecodingKey), Box<dyn std::error::Error>> {
        let private = SecretKey::random(&mut OsRng);
        let public = private.public_key().to_encoded_point(false);
        let x = URL_SAFE_NO_PAD.encode(public.x().ok_or("missing P-256 x coordinate")?);
        let y = URL_SAFE_NO_PAD.encode(public.y().ok_or("missing P-256 y coordinate")?);
        let jwks = serde_json::json!({
            "keys": [{
                "kty": "EC", "use": "sig", "alg": "ES256", "kid": KEY_ID,
                "crv": "P-256", "x": x, "y": y
            }]
        });
        let jwks = serde_json::to_string(&jwks)?;
        let set: JwkSet = serde_json::from_str(&jwks)?;
        let key = DecodingKey::from_jwk(&set.keys[0])?;
        Ok((private.to_pkcs8_der()?.as_bytes().to_vec(), jwks, key))
    }

    fn config() -> Result<(BrowserHop1AttestationConfig, DecodingKey), Box<dyn std::error::Error>> {
        let (private, jwks, verification) = signer_material()?;
        let config = BrowserHop1AttestationConfig::from_pkcs8_der_and_jwks(
            ISSUER.to_owned(),
            AUDIENCE.to_owned(),
            KEY_ID.to_owned(),
            &private,
            &jwks,
        )?;
        Ok((config, verification))
    }

    fn principal(email: &str) -> Result<BrowserPrincipal, String> {
        Ok(BrowserPrincipal {
            canonical_user_id: CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?,
            display_name: "Alice Example".to_owned(),
            display_email: Email::parse(email.to_owned())?,
            role: BrowserRole::Admin,
            member_roles: vec!["admin".to_owned(), "unrelated-group".to_owned()],
        })
    }

    #[test]
    fn rejects_jwks_that_does_not_verify_the_configured_private_signer()
    -> Result<(), Box<dyn std::error::Error>> {
        let (private, _, _) = signer_material()?;
        let (_, wrong_jwks, _) = signer_material()?;
        let result = BrowserHop1AttestationConfig::from_pkcs8_der_and_jwks(
            ISSUER.to_owned(),
            AUDIENCE.to_owned(),
            KEY_ID.to_owned(),
            &private,
            &wrong_jwks,
        );
        assert!(matches!(
            result,
            Err(BrowserHop1AttestationError::Unavailable)
        ));
        Ok(())
    }

    #[test]
    fn rejects_an_ambiguous_duplicate_active_jwks_key() -> Result<(), Box<dyn std::error::Error>> {
        let (private, jwks, _) = signer_material()?;
        let mut jwks: serde_json::Value = serde_json::from_str(&jwks)?;
        let duplicate = jwks["keys"]
            .as_array()
            .and_then(|keys| keys.first())
            .cloned()
            .ok_or("test JWKS must contain an active key")?;
        jwks["keys"]
            .as_array_mut()
            .ok_or("test JWKS keys must be mutable")?
            .push(duplicate);
        let result = BrowserHop1AttestationConfig::from_pkcs8_der_and_jwks(
            ISSUER.to_owned(),
            AUDIENCE.to_owned(),
            KEY_ID.to_owned(),
            &private,
            &serde_json::to_string(&jwks)?,
        );
        assert!(matches!(
            result,
            Err(BrowserHop1AttestationError::Unavailable)
        ));
        Ok(())
    }

    #[test]
    fn signed_assertion_carries_only_identity_contract_fields_and_never_browser_state()
    -> Result<(), Box<dyn std::error::Error>> {
        let (config, verification) = config()?;
        let assertion = config.sign(&principal("engineer@example.test")?)?;
        assert!(assertion.len() <= MAX_ASSERTION_BYTES);
        let header = decode_header(&assertion)?;
        assert_eq!(header.alg, Algorithm::ES256);
        assert_eq!(header.typ.as_deref(), Some("JWT"));
        assert_eq!(header.kid.as_deref(), Some(KEY_ID));
        let mut validation = Validation::new(Algorithm::ES256);
        validation.set_issuer(&[ISSUER]);
        validation.set_audience(&[AUDIENCE]);
        validation.validate_nbf = true;
        let claims = decode::<serde_json::Value>(&assertion, &verification, &validation)?.claims;
        let object = claims
            .as_object()
            .ok_or("assertion claims must be an object")?;
        let keys = object
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            keys,
            std::collections::BTreeSet::from([
                "aud",
                "email",
                "email_verified",
                "exp",
                "iat",
                "iss",
                "jti",
                "nbf",
                "operation",
                "operation_id",
                "sub",
            ])
        );
        assert_eq!(claims["iss"], ISSUER);
        assert_eq!(claims["sub"], "usr_0123456789abcdef0123456789abcdef");
        assert_eq!(claims["aud"], AUDIENCE);
        assert_eq!(claims["email"], "engineer@example.test");
        assert_eq!(claims["email_verified"], true);
        assert_eq!(claims["operation"], GITHUB_OAUTH_CONNECT_OPERATION);
        let operation_id = claims["operation_id"]
            .as_str()
            .ok_or("operation ID missing")?;
        assert!(operation_id.starts_with("op_"));
        assert_eq!(operation_id.len(), 35);
        let jti = claims["jti"].as_str().ok_or("assertion jti missing")?;
        assert_eq!(jti.len(), 32);
        let issued_at = claims["iat"].as_u64().ok_or("assertion iat missing")?;
        let expires_at = claims["exp"].as_u64().ok_or("assertion exp missing")?;
        assert!((1..=MAX_ASSERTION_LIFETIME_SECONDS).contains(&(expires_at - issued_at)));
        assert!(claims.get("binding").is_none());
        assert!(claims.get("roles").is_none());
        assert!(claims.get("session").is_none());
        Ok(())
    }

    #[test]
    fn signer_rejects_a_noncanonical_email_before_identity_receives_an_assertion()
    -> Result<(), Box<dyn std::error::Error>> {
        let (config, _) = config()?;
        assert_eq!(
            config.sign(&principal("Engineer@example.test")?),
            Err(BrowserHop1AttestationError::Unavailable)
        );
        Ok(())
    }

    #[test]
    fn endpoint_rejects_non_tls_or_non_private_exchange_paths() {
        for endpoint in [
            "http://identity.example.test/v1/browser-hop1/exchange",
            "https://identity.example.test/v1/exchange",
            "https://identity.example.test/v1/browser-hop1/exchange?user=browser",
            "https://user@identity.example.test/v1/browser-hop1/exchange",
        ] {
            assert!(IdentityBrowserHop1Endpoint::new(endpoint.to_owned()).is_err());
        }
    }

    #[derive(Clone, Default)]
    struct RecordingExchange {
        assertions: Arc<Mutex<Vec<String>>>,
    }

    impl BrowserHop1ExchangeClient for RecordingExchange {
        fn exchange<'a>(
            &'a self,
            _workload_bearer: &'a WorkloadBearer,
            assertion: &'a str,
        ) -> BoxFuture<'a, Result<BrowserHop1Bearer, BrowserHop1AttestationError>> {
            Box::pin(async move {
                self.assertions
                    .lock()
                    .map_err(|_| BrowserHop1AttestationError::Unavailable)?
                    .push(assertion.to_owned());
                BrowserHop1Bearer::new("identity-issued-browser-hop1".to_owned())
            })
        }
    }

    #[tokio::test]
    async fn exact_browser_connection_session_issues_one_internal_bearer_and_never_forwards_binding()
    -> Result<(), Box<dyn std::error::Error>> {
        let (config, verification) = config()?;
        let token_file = std::env::temp_dir().join(format!(
            "steward-browser-hop1-test-{}",
            Uuid::new_v4().simple()
        ));
        fs::write(&token_file, "synthetic-workload-token")?;
        let _cleanup = RemoveFileOnDrop(token_file.clone());
        let exchange = RecordingExchange::default();
        let issuer = BrowserHop1AttestationIssuer::new(
            config,
            ProjectedServiceAccountTokenFile::new(token_file.clone())?,
            exchange.clone(),
        );
        let session = ConnectionSession {
            subject: crate::connections::ConnectionSubject {
                canonical_user_id: CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?,
                display_email: "engineer@example.test".to_owned(),
            },
            binding: BrowserSessionBinding::from_test_value("browser-session-private"),
        };
        let bearer = issuer.issue(&session).await?;
        assert_eq!(bearer.into_inner(), "identity-issued-browser-hop1");
        let assertions = exchange
            .assertions
            .lock()
            .map_err(|_| "read exchange assertions")?
            .clone();
        assert_eq!(assertions.len(), 1);
        let mut validation = Validation::new(Algorithm::ES256);
        validation.set_issuer(&[ISSUER]);
        validation.set_audience(&[AUDIENCE]);
        let claims =
            decode::<serde_json::Value>(&assertions[0], &verification, &validation)?.claims;
        assert_eq!(claims["sub"], session.subject.canonical_user_id.as_str());
        assert_eq!(claims["email"], session.subject.display_email);
        assert!(claims.get("binding").is_none());
        assert!(claims.get("browser_session").is_none());
        Ok(())
    }
}
