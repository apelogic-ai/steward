//! SVID-to-HOP-1 exchange. This crate owns the signing key boundary.

use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::rejection::FormRejection;
use axum::extract::{Form, FromRequest, Request, State};
use axum::http::header::{AUTHORIZATION, CACHE_CONTROL, PRAGMA};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Duration as ChronoDuration;
use ed25519_dalek::SigningKey;
use jwt_compact::alg::Ed25519;
use jwt_compact::jwk::JsonWebKey;
use jwt_compact::prelude::UntrustedToken;
use jwt_compact::{AlgorithmExt as _, Claims, Header, TimeOptions};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use spiffe::WorkloadApiClient;
use steward_types::{Principal, RuntimeId, ToolGrant};
use uuid::Uuid;

pub const HOP1_CLAIMS_VERSION: u8 = 1;
pub const DEFAULT_AUTHORITY_TTL: Duration = Duration::from_secs(60);
pub const SPIFFE_CLIENT_ASSERTION_TYPE: &str =
    "urn:ietf:params:oauth:client-assertion-type:jwt-spiffe";

/// An untrusted JWT-SVID. Deliberately implements neither `Debug` nor `Display`.
pub struct SvidAssertion(String);

impl SvidAssertion {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub(crate) fn secret(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for SvidAssertion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self)
    }
}

/// A signed HOP-1 presented for online authority verification.
/// Deliberately implements neither `Debug` nor `Display`.
pub struct Hop1Token(String);

impl Hop1Token {
    fn secret(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for Hop1Token {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedWorkload {
    pub spiffe_id: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum AuthorityState {
    Active,
    Suspended,
    Revoked,
    Terminated,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorityBinding {
    pub workload_id: String,
    pub runtime: RuntimeId,
    pub principal: Principal,
    pub tools: Vec<ToolGrant>,
    pub state: AuthorityState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SvidValidationError {
    Rejected,
    Expired,
    Unavailable,
}

pub trait SvidValidator: Send + Sync + 'static {
    fn validate(
        &self,
        audience: &str,
        assertion: &SvidAssertion,
    ) -> impl Future<Output = Result<ValidatedWorkload, SvidValidationError>> + Send;
}

#[derive(Clone)]
pub struct SpireSvidValidator {
    client: WorkloadApiClient,
}

impl SpireSvidValidator {
    pub const fn new(client: WorkloadApiClient) -> Self {
        Self { client }
    }
}

impl SvidValidator for SpireSvidValidator {
    fn validate(
        &self,
        audience: &str,
        assertion: &SvidAssertion,
    ) -> impl Future<Output = Result<ValidatedWorkload, SvidValidationError>> + Send {
        let client = self.client.clone();
        let audience = audience.to_owned();
        let assertion = assertion.secret().to_owned();
        async move {
            let svid = client
                .validate_jwt_token(&audience, &assertion)
                .await
                .map_err(|error| match error {
                    spiffe::workload_api::WorkloadApiError::PermissionDenied(_)
                    | spiffe::workload_api::WorkloadApiError::NoIdentityIssued
                    | spiffe::workload_api::WorkloadApiError::JwtSvid(_) => {
                        SvidValidationError::Rejected
                    }
                    _ => SvidValidationError::Unavailable,
                })?;
            Ok(ValidatedWorkload {
                spiffe_id: svid.spiffe_id().to_string(),
            })
        }
    }
}

pub trait AuthorityResolver: Send + Sync + 'static {
    fn resolve(
        &self,
        workload: &ValidatedWorkload,
    ) -> impl Future<Output = Result<AuthorityBinding, MintError>> + Send;
}

pub struct MintSigningKey {
    key_id: String,
    signing: SigningKey,
    verifying: ed25519_dalek::VerifyingKey,
}

impl MintSigningKey {
    pub fn from_bytes(bytes: &[u8; 32]) -> Self {
        let signing = SigningKey::from_bytes(bytes);
        let verifying = signing.verifying_key();
        let public_jwk = JsonWebKey::from(&verifying);
        let thumbprint = public_jwk.thumbprint::<Sha256>();
        let key_id = thumbprint
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        Self {
            key_id,
            signing,
            verifying,
        }
    }
}

pub struct MintConfig {
    pub issuer: String,
    pub audience: String,
    pub allowed_scopes: Vec<String>,
    pub svid_audience: String,
    pub authority_ttl: Duration,
    pub introspection_client_credential: IntrospectionClientCredential,
}

/// Gateway credential accepted only by the online introspection endpoint.
/// Deliberately implements neither `Debug` nor `Display`.
pub struct IntrospectionClientCredential(String);

impl IntrospectionClientCredential {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    fn secret(&self) -> &str {
        &self.0
    }
}

impl MintConfig {
    pub fn validate(&self) -> Result<(), MintConfigError> {
        if self.issuer.trim().is_empty() {
            return Err(MintConfigError::EmptyIssuer);
        }
        if self.audience.trim().is_empty() {
            return Err(MintConfigError::EmptyAudience);
        }
        if self.svid_audience.trim().is_empty() {
            return Err(MintConfigError::EmptySvidAudience);
        }
        if self
            .introspection_client_credential
            .secret()
            .trim()
            .is_empty()
        {
            return Err(MintConfigError::InvalidIntrospectionClientCredential);
        }
        let mut scopes = BTreeSet::new();
        if self.allowed_scopes.is_empty()
            || self.allowed_scopes.iter().any(|scope| {
                scope.is_empty()
                    || scope.chars().any(char::is_whitespace)
                    || !scopes.insert(scope.as_str())
            })
        {
            return Err(MintConfigError::InvalidAllowedScopes);
        }
        if self.authority_ttl.is_zero() || self.authority_ttl > DEFAULT_AUTHORITY_TTL {
            return Err(MintConfigError::InvalidAuthorityTtl);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MintConfigError {
    EmptyAudience,
    EmptyIssuer,
    EmptySvidAudience,
    InvalidAllowedScopes,
    InvalidAuthorityTtl,
    InvalidIntrospectionClientCredential,
}

pub struct TokenGrantRequest {
    pub grant_type: String,
    pub client_assertion_type: String,
    pub client_assertion: SvidAssertion,
    pub audience: String,
    pub scope: Vec<String>,
}

#[derive(Serialize)]
pub struct TokenGrantResponse {
    access_token: String,
    expires_in: u64,
    scope: String,
    token_type: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct JwksDocument {
    pub keys: Vec<PublicJwk>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PublicJwk {
    pub alg: String,
    pub crv: String,
    pub kid: String,
    pub kty: String,
    #[serde(rename = "use")]
    pub key_use: String,
    pub x: String,
}

impl TokenGrantResponse {
    pub fn access_token(&self) -> &str {
        &self.access_token
    }

    pub const fn expires_in(&self) -> u64 {
        self.expires_in
    }

    pub fn scope(&self) -> &str {
        &self.scope
    }

    pub fn token_type(&self) -> &str {
        &self.token_type
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MintError {
    InvalidSvid,
    SvidValidatorUnavailable,
    AuthorityUnavailable,
    WorkloadMismatch,
    AuthorityInactive,
    UnsupportedPrincipal,
    InvalidRequest,
    InvalidScope,
    SigningFailed,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
struct Hop1Claims {
    aud: Vec<String>,
    azp: String,
    email: String,
    iss: String,
    jti: String,
    steward: StewardClaims,
    sub: String,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
struct StewardClaims {
    acting_as: String,
    runtime_uid: String,
    tools: Vec<ToolGrant>,
    version: u8,
}

pub struct Mint<V, R> {
    config: MintRuntimeConfig,
    introspection_client_credential_hash: [u8; 32],
    _key: MintSigningKey,
    resolver: R,
    validator: V,
}

struct MintRuntimeConfig {
    issuer: String,
    audience: String,
    allowed_scopes: Vec<String>,
    svid_audience: String,
    authority_ttl: Duration,
}

impl<V, R> Mint<V, R>
where
    V: SvidValidator,
    R: AuthorityResolver,
{
    pub fn new(
        config: MintConfig,
        key: MintSigningKey,
        validator: V,
        resolver: R,
    ) -> Result<Self, MintConfigError> {
        config.validate()?;
        let introspection_client_credential_hash =
            Sha256::digest(config.introspection_client_credential.secret().as_bytes()).into();
        let MintConfig {
            issuer,
            audience,
            allowed_scopes,
            svid_audience,
            authority_ttl,
            introspection_client_credential: _,
        } = config;
        Ok(Self {
            config: MintRuntimeConfig {
                issuer,
                audience,
                allowed_scopes,
                svid_audience,
                authority_ttl,
            },
            introspection_client_credential_hash,
            _key: key,
            resolver,
            validator,
        })
    }

    pub async fn exchange(
        &self,
        request: TokenGrantRequest,
    ) -> Result<TokenGrantResponse, MintError> {
        if request.grant_type != "client_credentials"
            || request.client_assertion_type != SPIFFE_CLIENT_ASSERTION_TYPE
            || request.audience != self.config.audience
        {
            return Err(MintError::InvalidRequest);
        }
        if request
            .scope
            .iter()
            .any(|scope| !self.config.allowed_scopes.contains(scope))
        {
            return Err(MintError::InvalidScope);
        }

        let workload = self
            .validator
            .validate(&self.config.svid_audience, &request.client_assertion)
            .await
            .map_err(|error| match error {
                SvidValidationError::Rejected | SvidValidationError::Expired => {
                    MintError::InvalidSvid
                }
                SvidValidationError::Unavailable => MintError::SvidValidatorUnavailable,
            })?;
        let authority = self.resolver.resolve(&workload).await?;

        if authority.workload_id != workload.spiffe_id {
            return Err(MintError::WorkloadMismatch);
        }
        if authority.state != AuthorityState::Active {
            return Err(MintError::AuthorityInactive);
        }
        let Principal::User { acting_user } = &authority.principal else {
            return Err(MintError::UnsupportedPrincipal);
        };
        if self.config.authority_ttl.is_zero() || self.config.authority_ttl > DEFAULT_AUTHORITY_TTL
        {
            return Err(MintError::InvalidRequest);
        }
        let authority_ttl = ChronoDuration::from_std(self.config.authority_ttl)
            .map_err(|_| MintError::InvalidRequest)?;
        let time = TimeOptions::from_leeway(ChronoDuration::zero());
        let claims = Claims::new(Hop1Claims {
            aud: vec![self.config.audience.clone()],
            azp: workload.spiffe_id,
            email: acting_user.0.clone(),
            iss: self.config.issuer.clone(),
            jti: Uuid::new_v4().to_string(),
            steward: StewardClaims {
                acting_as: "user".to_owned(),
                runtime_uid: authority.runtime.0,
                tools: authority.tools,
                version: HOP1_CLAIMS_VERSION,
            },
            sub: acting_user.0.clone(),
        })
        .set_duration_and_issuance(&time, authority_ttl);
        let header = Header::empty()
            .with_key_id(self._key.key_id.clone())
            .with_token_type("JWT");
        let access_token = Ed25519
            .token(&header, &claims, &self._key.signing)
            .map_err(|_| MintError::SigningFailed)?;

        Ok(TokenGrantResponse {
            access_token,
            expires_in: self.config.authority_ttl.as_secs(),
            scope: request.scope.join(" "),
            token_type: "Bearer".to_owned(),
        })
    }

    pub fn jwks(&self) -> Result<JwksDocument, MintError> {
        let value = serde_json::to_value(JsonWebKey::from(&self._key.verifying))
            .map_err(|_| MintError::SigningFailed)?;
        let x = value
            .get("x")
            .and_then(serde_json::Value::as_str)
            .ok_or(MintError::SigningFailed)?
            .to_owned();
        Ok(JwksDocument {
            keys: vec![PublicJwk {
                alg: "EdDSA".to_owned(),
                crv: "Ed25519".to_owned(),
                kid: self._key.key_id.clone(),
                kty: "OKP".to_owned(),
                key_use: "sig".to_owned(),
                x,
            }],
        })
    }

    pub async fn introspect(
        &self,
        token: &Hop1Token,
    ) -> Result<TokenIntrospectionResponse, MintError> {
        let untrusted = match UntrustedToken::new(token.secret()) {
            Ok(token) => token,
            Err(_) => return Ok(TokenIntrospectionResponse::inactive()),
        };
        let token: jwt_compact::Token<Hop1Claims> =
            match Ed25519.validator(&self._key.verifying).validate(&untrusted) {
                Ok(token) => token,
                Err(_) => return Ok(TokenIntrospectionResponse::inactive()),
            };
        let claims = token.claims();
        let time = TimeOptions::from_leeway(ChronoDuration::zero());
        if claims.validate_expiration(&time).is_err()
            || claims.custom.iss != self.config.issuer
            || claims.custom.aud.as_slice() != [self.config.audience.as_str()]
        {
            return Ok(TokenIntrospectionResponse::inactive());
        }

        let workload = ValidatedWorkload {
            spiffe_id: claims.custom.azp.clone(),
        };
        let authority = match self.resolver.resolve(&workload).await {
            Ok(authority) => authority,
            Err(MintError::AuthorityUnavailable) => return Err(MintError::AuthorityUnavailable),
            Err(_) => return Ok(TokenIntrospectionResponse::inactive()),
        };
        let principal_matches = matches!(
            &authority.principal,
            Principal::User { acting_user }
                if acting_user.0 == claims.custom.email
                    && acting_user.0 == claims.custom.sub
        );
        let active = authority.state == AuthorityState::Active
            && authority.workload_id == claims.custom.azp
            && authority.runtime.0 == claims.custom.steward.runtime_uid
            && authority.tools == claims.custom.steward.tools
            && claims.custom.steward.version == HOP1_CLAIMS_VERSION
            && claims.custom.steward.acting_as == "user"
            && principal_matches;
        Ok(TokenIntrospectionResponse { active })
    }

    fn authenticates_introspection_client(&self, authorization: Option<&HeaderValue>) -> bool {
        let Some(candidate) = authorization
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .filter(|value| !value.is_empty())
        else {
            return false;
        };
        let candidate_hash: [u8; 32] = Sha256::digest(candidate.as_bytes()).into();
        constant_time_equal(&candidate_hash, &self.introspection_client_credential_hash)
    }
}

#[derive(Deserialize)]
struct TokenGrantForm {
    audience: String,
    client_assertion: SvidAssertion,
    client_assertion_type: String,
    grant_type: String,
    #[serde(default)]
    scope: String,
}

#[derive(Deserialize)]
struct TokenIntrospectionForm {
    token: Hop1Token,
}

#[derive(Serialize)]
pub struct TokenIntrospectionResponse {
    active: bool,
}

impl TokenIntrospectionResponse {
    const fn inactive() -> Self {
        Self { active: false }
    }
}

#[derive(Serialize)]
struct OAuthError {
    error: &'static str,
}

type OAuthResult<T> = Result<Json<T>, (StatusCode, Json<OAuthError>)>;
type TokenResult<T> = Result<(HeaderMap, Json<T>), (StatusCode, Json<OAuthError>)>;

async fn token_handler<V, R>(
    State(mint): State<Arc<Mint<V, R>>>,
    form: Result<Form<TokenGrantForm>, FormRejection>,
) -> TokenResult<TokenGrantResponse>
where
    V: SvidValidator,
    R: AuthorityResolver,
{
    let Form(form) = form.map_err(|_| oauth_error(StatusCode::BAD_REQUEST, "invalid_request"))?;
    if form.grant_type != "client_credentials" {
        return Err(oauth_error(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
        ));
    }
    if form.client_assertion_type != SPIFFE_CLIENT_ASSERTION_TYPE {
        return Err(oauth_error(StatusCode::BAD_REQUEST, "invalid_request"));
    }

    let request = TokenGrantRequest {
        grant_type: form.grant_type,
        client_assertion_type: form.client_assertion_type,
        client_assertion: form.client_assertion,
        audience: form.audience,
        scope: form
            .scope
            .split_whitespace()
            .map(ToOwned::to_owned)
            .collect(),
    };
    mint.exchange(request)
        .await
        .map(|response| (no_store_headers(), Json(response)))
        .map_err(map_mint_error)
}

async fn introspection_handler<V, R>(
    State(mint): State<Arc<Mint<V, R>>>,
    request: Request,
) -> TokenResult<TokenIntrospectionResponse>
where
    V: SvidValidator,
    R: AuthorityResolver,
{
    if !mint.authenticates_introspection_client(request.headers().get(AUTHORIZATION)) {
        return Err(oauth_error(StatusCode::UNAUTHORIZED, "invalid_client"));
    }
    let Form(form) = Form::<TokenIntrospectionForm>::from_request(request, &())
        .await
        .map_err(|_| oauth_error(StatusCode::BAD_REQUEST, "invalid_request"))?;
    mint.introspect(&form.token)
        .await
        .map(|response| (no_store_headers(), Json(response)))
        .map_err(map_mint_error)
}

async fn jwks_handler<V, R>(State(mint): State<Arc<Mint<V, R>>>) -> OAuthResult<JwksDocument>
where
    V: SvidValidator,
    R: AuthorityResolver,
{
    mint.jwks()
        .map(Json)
        .map_err(|_| oauth_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error"))
}

fn map_mint_error(error: MintError) -> (StatusCode, Json<OAuthError>) {
    match error {
        MintError::InvalidSvid => oauth_error(StatusCode::UNAUTHORIZED, "invalid_client"),
        MintError::SvidValidatorUnavailable | MintError::AuthorityUnavailable => {
            oauth_error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable")
        }
        MintError::WorkloadMismatch
        | MintError::AuthorityInactive
        | MintError::UnsupportedPrincipal => oauth_error(StatusCode::FORBIDDEN, "invalid_grant"),
        MintError::InvalidRequest => oauth_error(StatusCode::BAD_REQUEST, "invalid_request"),
        MintError::InvalidScope => oauth_error(StatusCode::BAD_REQUEST, "invalid_scope"),
        MintError::SigningFailed => oauth_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error"),
    }
}

fn oauth_error(status: StatusCode, error: &'static str) -> (StatusCode, Json<OAuthError>) {
    (status, Json(OAuthError { error }))
}

fn no_store_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(PRAGMA, HeaderValue::from_static("no-cache"));
    headers
}

fn constant_time_equal(left: &[u8; 32], right: &[u8; 32]) -> bool {
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

pub fn router<V, R>(mint: Arc<Mint<V, R>>) -> Router
where
    V: SvidValidator,
    R: AuthorityResolver,
{
    Router::new()
        .route("/token", post(token_handler::<V, R>))
        .route("/introspect", post(introspection_handler::<V, R>))
        .route("/.well-known/jwks.json", get(jwks_handler::<V, R>))
        .fallback(|| async {
            (
                StatusCode::NOT_FOUND,
                Json(OAuthError { error: "not_found" }),
            )
        })
        .with_state(mint)
}
