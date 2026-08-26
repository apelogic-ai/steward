use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use jsonwebtoken::errors::ErrorKind;
use jsonwebtoken::jwk::{
    AlgorithmParameters, Jwk, JwkSet, KeyAlgorithm, KeyOperations, PublicKeyUse,
};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;

use crate::browser_auth::{
    BrowserAuthFailure, BrowserAuthorizationRequest, BrowserFuture, BrowserOidcProvider,
    GoogleOidcConfig, VerifiedOrganizationClaims,
};

const MAX_ID_TOKEN_BYTES: usize = 16 * 1024;
const MAX_DISCOVERY_BYTES: usize = 32 * 1024;
const MAX_JWKS_BYTES: usize = 128 * 1024;
const MAX_TOKEN_RESPONSE_BYTES: usize = 32 * 1024;
const CLOCK_SKEW_SECONDS: u64 = 60;
const MAX_TOKEN_AGE_SECONDS: u64 = 300;
const DEFAULT_CACHE_SECONDS: u64 = 300;
const MAX_CACHE_SECONDS: u64 = 3_600;
const JWKS_REFRESH_FAILURE_BACKOFF_SECONDS: u64 = 5;
const GOOGLE_DISCOVERY_ENDPOINT: &str =
    "https://accounts.google.com/.well-known/openid-configuration";
const GOOGLE_AUTHORIZATION_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
const GOOGLE_JWKS_ENDPOINT: &str = "https://www.googleapis.com/oauth2/v3/certs";

#[derive(Clone, Deserialize)]
#[serde(untagged)]
enum Audience {
    Single(String),
    Multiple(Vec<String>),
}

#[derive(Clone, Deserialize)]
struct IdTokenClaims {
    iss: String,
    sub: String,
    aud: Audience,
    #[serde(default)]
    azp: Option<String>,
    exp: u64,
    iat: u64,
    nonce: String,
    email: String,
    email_verified: bool,
    hd: String,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IdTokenError {
    Malformed,
    ForbiddenHeader,
    MissingKeyId,
    UnknownKeyId,
    DuplicateKeyId,
    InvalidKey,
    InvalidSignature,
    InvalidClaims,
}

fn verify_id_token(
    token: &str,
    jwks: &JwkSet,
    client_id: &str,
    hosted_domain: &str,
    expected_nonce: &str,
    now: u64,
) -> Result<VerifiedOrganizationClaims, IdTokenError> {
    if token.is_empty() || token.len() > MAX_ID_TOKEN_BYTES {
        return Err(IdTokenError::Malformed);
    }
    let header = decode_header(token).map_err(|_| IdTokenError::Malformed)?;
    if header.alg != Algorithm::RS256
        || header.jku.is_some()
        || header.jwk.is_some()
        || header.x5u.is_some()
        || header.x5c.is_some()
        || header.x5t.is_some()
        || header.x5t_s256.is_some()
        || header.crit.is_some()
    {
        return Err(IdTokenError::ForbiddenHeader);
    }
    let kid = header.kid.ok_or(IdTokenError::MissingKeyId)?;
    if kid.is_empty() || kid.len() > 128 || !kid.is_ascii() {
        return Err(IdTokenError::MissingKeyId);
    }
    let key = select_verification_key(jwks, &kid)?;
    let decoding_key = DecodingKey::from_jwk(key).map_err(|_| IdTokenError::InvalidKey)?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.required_spec_claims.clear();
    validation.validate_exp = false;
    validation.validate_aud = false;
    let token = decode::<IdTokenClaims>(token, &decoding_key, &validation).map_err(|error| {
        if matches!(error.kind(), ErrorKind::InvalidSignature) {
            IdTokenError::InvalidSignature
        } else {
            IdTokenError::Malformed
        }
    })?;
    validate_claims(token.claims, client_id, hosted_domain, expected_nonce, now)
}

fn select_verification_key<'a>(jwks: &'a JwkSet, kid: &str) -> Result<&'a Jwk, IdTokenError> {
    let mut matching = jwks
        .keys
        .iter()
        .filter(|key| key.common.key_id.as_deref() == Some(kid));
    let key = matching.next().ok_or(IdTokenError::UnknownKeyId)?;
    if matching.next().is_some() {
        return Err(IdTokenError::DuplicateKeyId);
    }
    if !matches!(key.algorithm, AlgorithmParameters::RSA(_))
        || !matches!(
            key.common.public_key_use,
            None | Some(PublicKeyUse::Signature)
        )
        || !key.common.key_operations.as_ref().is_none_or(|operations| {
            !operations.is_empty()
                && operations
                    .iter()
                    .all(|operation| matches!(operation, KeyOperations::Verify))
        })
        || !matches!(key.common.key_algorithm, None | Some(KeyAlgorithm::RS256))
    {
        return Err(IdTokenError::InvalidKey);
    }
    Ok(key)
}

fn validate_claims(
    claims: IdTokenClaims,
    client_id: &str,
    hosted_domain: &str,
    expected_nonce: &str,
    now: u64,
) -> Result<VerifiedOrganizationClaims, IdTokenError> {
    let audience_matches = match &claims.aud {
        Audience::Single(audience) => audience == client_id,
        Audience::Multiple(audiences) => {
            audiences.len() == 1
                && audiences
                    .first()
                    .is_some_and(|audience| audience == client_id)
        }
    };
    let subject_is_bounded = !claims.sub.is_empty()
        && claims.sub.len() <= 255
        && claims.sub.is_ascii()
        && !claims.sub.chars().any(char::is_whitespace);
    let email_is_bounded = !claims.email.is_empty() && claims.email.len() <= 320;
    let display_name_is_bounded = claims.name.as_ref().is_none_or(|name| {
        let trimmed = name.trim();
        !trimmed.is_empty() && trimmed.len() <= 256 && !trimmed.chars().any(char::is_control)
    });
    let nonce_is_bounded = !claims.nonce.is_empty() && claims.nonce.len() <= 512;
    let expiration_is_valid = claims.exp.saturating_add(CLOCK_SKEW_SECONDS) > now;
    let issued_at_is_valid = claims.iat <= now.saturating_add(CLOCK_SKEW_SECONDS)
        && now <= claims.iat.saturating_add(MAX_TOKEN_AGE_SECONDS);
    if claims.iss != steward_types::GOOGLE_ORGANIZATION_ISSUER
        || !audience_matches
        || claims.azp.as_deref().is_some_and(|azp| azp != client_id)
        || !expiration_is_valid
        || !issued_at_is_valid
        || !subject_is_bounded
        || !nonce_is_bounded
        || !crate::browser_auth::secret_eq(&claims.nonce, expected_nonce)
        || !email_is_bounded
        || !display_name_is_bounded
        || !claims.email_verified
        || claims.hd != hosted_domain
    {
        return Err(IdTokenError::InvalidClaims);
    }
    Ok(VerifiedOrganizationClaims {
        issuer: claims.iss,
        subject: claims.sub,
        hosted_domain: claims.hd,
        email: claims.email,
        email_verified: claims.email_verified,
        display_name: claims.name.map(|name| name.trim().to_owned()),
        nonce: claims.nonce,
    })
}

fn map_id_token_error(_error: IdTokenError) -> BrowserAuthFailure {
    BrowserAuthFailure::InvalidIdentity
}

type HttpFuture<'a> = Pin<Box<dyn Future<Output = Result<HttpResponse, HttpFailure>> + Send + 'a>>;

struct GoogleClientSecret(String);

impl GoogleClientSecret {
    pub fn new(value: String) -> Result<Self, String> {
        if value.is_empty()
            || value.len() > 4096
            || !value.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err("Google OIDC client secret must be configured".to_owned());
        }
        Ok(Self(value))
    }
}

#[derive(Clone, Deserialize)]
struct DiscoveryDocument {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    response_types_supported: Vec<String>,
    id_token_signing_alg_values_supported: Vec<String>,
    code_challenge_methods_supported: Vec<String>,
}

impl DiscoveryDocument {
    fn validate(self) -> Result<Self, BrowserAuthFailure> {
        if self.issuer != steward_types::GOOGLE_ORGANIZATION_ISSUER
            || self.authorization_endpoint != GOOGLE_AUTHORIZATION_ENDPOINT
            || self.token_endpoint != GOOGLE_TOKEN_ENDPOINT
            || self.jwks_uri != GOOGLE_JWKS_ENDPOINT
            || !self
                .response_types_supported
                .iter()
                .any(|value| value == "code")
            || !self
                .id_token_signing_alg_values_supported
                .iter()
                .any(|value| value == "RS256")
            || !self
                .code_challenge_methods_supported
                .iter()
                .any(|value| value == "S256")
        {
            return Err(BrowserAuthFailure::ProviderUnavailable);
        }
        Ok(self)
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    id_token: String,
}

#[derive(Clone)]
struct Cached<T> {
    value: T,
    expires_at: u64,
}

#[derive(Clone, Copy)]
struct JwksRefreshFailure {
    observed_generation: u64,
    retry_at: u64,
}

#[derive(Default)]
struct ProviderCache {
    discovery: Option<Cached<DiscoveryDocument>>,
    jwks: Option<Cached<JwkSet>>,
    jwks_generation: u64,
    jwks_refresh_failure: Option<JwksRefreshFailure>,
}

#[derive(Default)]
struct RefreshGateState {
    held: bool,
    waiters: Vec<Waker>,
}

#[derive(Default)]
struct RefreshGate {
    state: Mutex<RefreshGateState>,
}

impl RefreshGate {
    fn acquire(self: &Arc<Self>) -> RefreshGateAcquire {
        RefreshGateAcquire {
            gate: Arc::clone(self),
        }
    }
}

struct RefreshGateAcquire {
    gate: Arc<RefreshGate>,
}

impl Future for RefreshGateAcquire {
    type Output = Result<RefreshGatePermit, BrowserAuthFailure>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = match self.gate.state.lock() {
            Ok(state) => state,
            Err(_) => return Poll::Ready(Err(BrowserAuthFailure::ProviderUnavailable)),
        };
        if !state.held {
            state.held = true;
            return Poll::Ready(Ok(RefreshGatePermit {
                gate: Arc::clone(&self.gate),
            }));
        }
        if !state
            .waiters
            .iter()
            .any(|waker| waker.will_wake(context.waker()))
        {
            state.waiters.push(context.waker().clone());
        }
        Poll::Pending
    }
}

struct RefreshGatePermit {
    gate: Arc<RefreshGate>,
}

impl Drop for RefreshGatePermit {
    fn drop(&mut self) {
        if let Ok(mut state) = self.gate.state.lock() {
            state.held = false;
            for waker in state.waiters.drain(..) {
                waker.wake();
            }
        }
    }
}

struct HttpResponse {
    status: u16,
    content_type: Option<String>,
    cache_control: Option<String>,
    body: Vec<u8>,
}

#[derive(Clone, Copy)]
enum HttpFailure {
    Unavailable,
    InvalidResponse,
}

trait GoogleHttpTransport: Send + Sync + 'static {
    fn get<'a>(&'a self, url: &'a str, max_body: usize) -> HttpFuture<'a>;

    fn post_token<'a>(
        &'a self,
        url: &'a str,
        request: TokenExchangeRequest<'a>,
        max_body: usize,
    ) -> HttpFuture<'a>;
}

struct TokenExchangeRequest<'a> {
    code: &'a str,
    client_id: &'a str,
    client_secret: &'a GoogleClientSecret,
    callback_uri: &'a str,
    pkce_verifier: &'a str,
}

struct ReqwestGoogleTransport {
    client: reqwest::Client,
}

impl ReqwestGoogleTransport {
    fn new() -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(5))
            .user_agent("steward-google-oidc/1")
            .build()
            .map_err(|_| "build bounded Google OIDC HTTP client".to_owned())?;
        Ok(Self { client })
    }

    async fn response(
        mut response: reqwest::Response,
        max_body: usize,
    ) -> Result<HttpResponse, HttpFailure> {
        if response
            .content_length()
            .is_some_and(|length| length > max_body as u64)
        {
            return Err(HttpFailure::InvalidResponse);
        }
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let cache_control = response
            .headers()
            .get(reqwest::header::CACHE_CONTROL)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| HttpFailure::Unavailable)?
        {
            if body.len().saturating_add(chunk.len()) > max_body {
                return Err(HttpFailure::InvalidResponse);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(HttpResponse {
            status,
            content_type,
            cache_control,
            body,
        })
    }
}

impl GoogleHttpTransport for ReqwestGoogleTransport {
    fn get<'a>(&'a self, url: &'a str, max_body: usize) -> HttpFuture<'a> {
        Box::pin(async move {
            if !matches!(url, GOOGLE_DISCOVERY_ENDPOINT | GOOGLE_JWKS_ENDPOINT) {
                return Err(HttpFailure::InvalidResponse);
            }
            let response = self
                .client
                .get(url)
                .send()
                .await
                .map_err(|_| HttpFailure::Unavailable)?;
            Self::response(response, max_body).await
        })
    }

    fn post_token<'a>(
        &'a self,
        url: &'a str,
        request: TokenExchangeRequest<'a>,
        max_body: usize,
    ) -> HttpFuture<'a> {
        Box::pin(async move {
            if url != GOOGLE_TOKEN_ENDPOINT {
                return Err(HttpFailure::InvalidResponse);
            }
            let response = self
                .client
                .post(url)
                .form(&[
                    ("code", request.code),
                    ("client_id", request.client_id),
                    ("client_secret", request.client_secret.0.as_str()),
                    ("redirect_uri", request.callback_uri),
                    ("grant_type", "authorization_code"),
                    ("code_verifier", request.pkce_verifier),
                ])
                .send()
                .await
                .map_err(|_| HttpFailure::Unavailable)?;
            Self::response(response, max_body).await
        })
    }
}

#[derive(Clone)]
pub struct GoogleOidcProvider {
    config: GoogleOidcConfig,
    client_secret: Arc<GoogleClientSecret>,
    transport: Arc<dyn GoogleHttpTransport>,
    now: Arc<dyn Fn() -> u64 + Send + Sync>,
    cache: Arc<Mutex<ProviderCache>>,
    refresh_gate: Arc<RefreshGate>,
}

impl GoogleOidcProvider {
    pub fn new(config: GoogleOidcConfig, client_secret: String) -> Result<Self, String> {
        Self::from_parts(
            config,
            client_secret,
            Arc::new(ReqwestGoogleTransport::new()?),
            Arc::new(crate::browser_auth::epoch_seconds),
        )
    }

    fn from_parts(
        config: GoogleOidcConfig,
        client_secret: String,
        transport: Arc<dyn GoogleHttpTransport>,
        now: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> Result<Self, String> {
        Ok(Self {
            config,
            client_secret: Arc::new(GoogleClientSecret::new(client_secret)?),
            transport,
            now,
            cache: Arc::new(Mutex::new(ProviderCache::default())),
            refresh_gate: Arc::new(RefreshGate::default()),
        })
    }

    #[cfg(test)]
    fn with_transport<F>(
        config: GoogleOidcConfig,
        client_secret: String,
        transport: Arc<dyn GoogleHttpTransport>,
        now: F,
    ) -> Result<Self, String>
    where
        F: Fn() -> u64 + Send + Sync + 'static,
    {
        Self::from_parts(config, client_secret, transport, Arc::new(now))
    }

    async fn discovery(&self) -> Result<DiscoveryDocument, BrowserAuthFailure> {
        let now = (self.now)();
        if let Some(document) = self
            .cache
            .lock()
            .map_err(|_| BrowserAuthFailure::ProviderUnavailable)?
            .discovery
            .as_ref()
            .filter(|cached| cached.expires_at > now)
            .map(|cached| cached.value.clone())
        {
            return Ok(document);
        }
        let _permit = self.refresh_gate.acquire().await?;
        self.discovery_after_refresh_gate().await
    }

    async fn discovery_after_refresh_gate(&self) -> Result<DiscoveryDocument, BrowserAuthFailure> {
        let now = (self.now)();
        if let Some(document) = self
            .cache
            .lock()
            .map_err(|_| BrowserAuthFailure::ProviderUnavailable)?
            .discovery
            .as_ref()
            .filter(|cached| cached.expires_at > now)
            .map(|cached| cached.value.clone())
        {
            return Ok(document);
        }
        let response = self
            .transport
            .get(GOOGLE_DISCOVERY_ENDPOINT, MAX_DISCOVERY_BYTES)
            .await
            .map_err(map_http_failure)?;
        if response.body.len() > MAX_DISCOVERY_BYTES {
            return Err(BrowserAuthFailure::ProviderUnavailable);
        }
        let ttl = validate_json_response(&response)?;
        let document: DiscoveryDocument = serde_json::from_slice(&response.body)
            .map_err(|_| BrowserAuthFailure::ProviderUnavailable)?;
        let document = document.validate()?;
        let stored_at = (self.now)();
        self.cache
            .lock()
            .map_err(|_| BrowserAuthFailure::ProviderUnavailable)?
            .discovery = Some(Cached {
            value: document.clone(),
            expires_at: stored_at.saturating_add(ttl),
        });
        Ok(document)
    }

    async fn jwks(
        &self,
        force_after_generation: Option<u64>,
    ) -> Result<JwkSet, BrowserAuthFailure> {
        let now = (self.now)();
        let cached = {
            let cache = self
                .cache
                .lock()
                .map_err(|_| BrowserAuthFailure::ProviderUnavailable)?;
            cache
                .jwks
                .as_ref()
                .filter(|cached| cached.expires_at > now)
                .map(|cached| cached.value.clone())
        };
        if force_after_generation.is_none()
            && let Some(jwks) = cached
        {
            return Ok(jwks);
        }
        let _permit = self.refresh_gate.acquire().await?;
        let now = (self.now)();
        let attempt_generation = {
            let cache = self
                .cache
                .lock()
                .map_err(|_| BrowserAuthFailure::ProviderUnavailable)?;
            if force_after_generation.is_some_and(|expected| cache.jwks_generation != expected)
                && let Some(jwks) = cache
                    .jwks
                    .as_ref()
                    .filter(|cached| cached.expires_at > now)
                    .map(|cached| cached.value.clone())
            {
                return Ok(jwks);
            }
            if force_after_generation.is_none()
                && let Some(jwks) = cache
                    .jwks
                    .as_ref()
                    .filter(|cached| cached.expires_at > now)
                    .map(|cached| cached.value.clone())
            {
                return Ok(jwks);
            }
            if cache.jwks_refresh_failure.is_some_and(|failure| {
                failure.observed_generation == cache.jwks_generation && failure.retry_at > now
            }) {
                return Err(BrowserAuthFailure::ProviderUnavailable);
            }
            cache.jwks_generation
        };

        let refreshed = async {
            let discovery = self.discovery_after_refresh_gate().await?;
            let response = self
                .transport
                .get(&discovery.jwks_uri, MAX_JWKS_BYTES)
                .await
                .map_err(map_http_failure)?;
            if response.body.len() > MAX_JWKS_BYTES {
                return Err(BrowserAuthFailure::ProviderUnavailable);
            }
            let ttl = validate_json_response(&response)?;
            let jwks: JwkSet = serde_json::from_slice(&response.body)
                .map_err(|_| BrowserAuthFailure::ProviderUnavailable)?;
            validate_jwks(&jwks)?;
            Ok((jwks, ttl))
        }
        .await;

        match refreshed {
            Ok((jwks, ttl)) => {
                let stored_at = (self.now)();
                let mut cache = self
                    .cache
                    .lock()
                    .map_err(|_| BrowserAuthFailure::ProviderUnavailable)?;
                cache.jwks_generation = cache.jwks_generation.saturating_add(1);
                cache.jwks_refresh_failure = None;
                cache.jwks = Some(Cached {
                    value: jwks.clone(),
                    expires_at: stored_at.saturating_add(ttl),
                });
                Ok(jwks)
            }
            Err(error) => {
                let failed_at = (self.now)();
                let mut cache = self
                    .cache
                    .lock()
                    .map_err(|_| BrowserAuthFailure::ProviderUnavailable)?;
                if cache.jwks_generation == attempt_generation {
                    cache.jwks_refresh_failure = Some(JwksRefreshFailure {
                        observed_generation: attempt_generation,
                        retry_at: failed_at.saturating_add(JWKS_REFRESH_FAILURE_BACKOFF_SECONDS),
                    });
                }
                Err(error)
            }
        }
    }
}

impl BrowserOidcProvider for GoogleOidcProvider {
    fn authorization_url(
        &self,
        flow: &BrowserAuthorizationRequest,
    ) -> Result<String, BrowserAuthFailure> {
        Ok(self.config.authorization_request_url(flow))
    }

    fn exchange_code<'a>(
        &'a self,
        code: &'a str,
        pkce_verifier: &'a str,
        callback_uri: &'a str,
        expected_nonce: &'a str,
    ) -> BrowserFuture<'a, Result<VerifiedOrganizationClaims, BrowserAuthFailure>> {
        Box::pin(async move {
            if code.is_empty()
                || code.len() > 2_048
                || pkce_verifier.len() < 43
                || pkce_verifier.len() > 128
                || !pkce_verifier.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
                })
                || callback_uri != self.config.callback_uri()
                || expected_nonce.is_empty()
                || expected_nonce.len() > 512
            {
                return Err(BrowserAuthFailure::InvalidRequest);
            }
            let discovery = self.discovery().await?;
            let response = self
                .transport
                .post_token(
                    &discovery.token_endpoint,
                    TokenExchangeRequest {
                        code,
                        client_id: self.config.client_id(),
                        client_secret: &self.client_secret,
                        callback_uri,
                        pkce_verifier,
                    },
                    MAX_TOKEN_RESPONSE_BYTES,
                )
                .await
                .map_err(map_http_failure)?;
            if response.body.len() > MAX_TOKEN_RESPONSE_BYTES || !has_json_content_type(&response) {
                return Err(BrowserAuthFailure::ProviderUnavailable);
            }
            if (400..500).contains(&response.status) {
                return Err(BrowserAuthFailure::InvalidIdentity);
            }
            validate_json_response(&response)?;
            let token: TokenResponse = serde_json::from_slice(&response.body)
                .map_err(|_| BrowserAuthFailure::ProviderUnavailable)?;
            if token.id_token.is_empty() || token.id_token.len() > MAX_ID_TOKEN_BYTES {
                return Err(BrowserAuthFailure::InvalidIdentity);
            }
            let header =
                decode_header(&token.id_token).map_err(|_| BrowserAuthFailure::InvalidIdentity)?;
            let kid = header
                .kid
                .filter(|kid| !kid.is_empty() && kid.len() <= 128 && kid.is_ascii())
                .ok_or(BrowserAuthFailure::InvalidIdentity)?;
            let jwks = self.jwks(None).await?;
            let generation = self
                .cache
                .lock()
                .map_err(|_| BrowserAuthFailure::ProviderUnavailable)?
                .jwks_generation;
            let jwks = match select_verification_key(&jwks, &kid) {
                Ok(_) => jwks,
                Err(IdTokenError::UnknownKeyId) => self.jwks(Some(generation)).await?,
                Err(error) => return Err(map_id_token_error(error)),
            };
            verify_id_token(
                &token.id_token,
                &jwks,
                self.config.client_id(),
                self.config.hosted_domain(),
                expected_nonce,
                (self.now)(),
            )
            .map_err(map_id_token_error)
        })
    }
}

fn map_http_failure(_error: HttpFailure) -> BrowserAuthFailure {
    BrowserAuthFailure::ProviderUnavailable
}

fn validate_json_response(response: &HttpResponse) -> Result<u64, BrowserAuthFailure> {
    if !(200..300).contains(&response.status) || !has_json_content_type(response) {
        return Err(BrowserAuthFailure::ProviderUnavailable);
    }
    Ok(cache_ttl(response.cache_control.as_deref()))
}

fn has_json_content_type(response: &HttpResponse) -> bool {
    response
        .content_type
        .as_deref()
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
}

fn cache_ttl(cache_control: Option<&str>) -> u64 {
    if cache_control.is_some_and(|value| {
        value
            .split(',')
            .map(str::trim)
            .any(|directive| directive.eq_ignore_ascii_case("no-store"))
    }) {
        return 0;
    }
    cache_control
        .into_iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .find_map(|directive| directive.strip_prefix("max-age="))
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_CACHE_SECONDS)
        .clamp(1, MAX_CACHE_SECONDS)
}

fn validate_jwks(jwks: &JwkSet) -> Result<(), BrowserAuthFailure> {
    if jwks.keys.is_empty() {
        return Err(BrowserAuthFailure::ProviderUnavailable);
    }
    let mut kids = std::collections::HashSet::with_capacity(jwks.keys.len());
    for key in &jwks.keys {
        let kid = key
            .common
            .key_id
            .as_deref()
            .filter(|kid| !kid.is_empty() && kid.len() <= 128 && kid.is_ascii())
            .ok_or(BrowserAuthFailure::ProviderUnavailable)?;
        if !kids.insert(kid) || select_verification_key(jwks, kid).is_err() {
            return Err(BrowserAuthFailure::ProviderUnavailable);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use tokio::sync::Notify;

    use jsonwebtoken::jwk::JwkSet;

    use crate::browser_auth::{BrowserAuthFailure, BrowserOidcProvider, GoogleOidcConfig};
    use steward_types::OrganizationId;

    use super::{
        Audience, Cached, DiscoveryDocument, GoogleClientSecret, GoogleHttpTransport,
        GoogleOidcProvider, HttpFailure, HttpFuture, HttpResponse, IdTokenClaims, IdTokenError,
        JWKS_REFRESH_FAILURE_BACKOFF_SECONDS, TokenExchangeRequest, cache_ttl, validate_claims,
        verify_id_token,
    };

    const FIXTURE_HEADER: &str = "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCIsImtpZCI6ImZpeHR1cmUta2V5In0";
    const FIXTURE_PAYLOAD: &str = "eyJpc3MiOiJodHRwczovL2FjY291bnRzLmdvb2dsZS5jb20iLCJzdWIiOiJmaXh0dXJlLXN1YmplY3QiLCJhdWQiOiJmaXh0dXJlLWNsaWVudCIsImF6cCI6ImZpeHR1cmUtY2xpZW50IiwiZXhwIjoyMDAwLCJpYXQiOjEwMDAsIm5vbmNlIjoiZml4dHVyZS1ub25jZSIsImVtYWlsIjoiYWxpY2VAZXhhbXBsZS5jb20iLCJlbWFpbF92ZXJpZmllZCI6dHJ1ZSwiaGQiOiJleGFtcGxlLmNvbSJ9";
    const FIXTURE_SIGNATURE: &str = "zM7KyP19QiF_mYofXn7XRXMbLJaC9PNwbs5rVUmq-MU0RH752hE4JtJXwKViKp9jc5jKqqB-rhlqTNLpp3JKjPeIv_ZMwtI4-dbIfvZXkGtl2biQ27i6CA0eYotZm_KOuIO8etRseG89IdRRHjCOeV2LgBN_3WswM_Yvun9fejzbHlPOgmF149Q4gDxsmtB5hOVoFcgU8uaDVYDo-rJwebLsUUEpCA1lEAQIuARh9kuJw-xD9kVsRwAM4hc3h4puTmRNCoeSAeak5DBtlRY6Y4arwUcRt6dkA-Rr1xfgzEp3DW6rldIMAp_dAS9ZWUKz_e1hlG3X4twnEetvO_-JKg";
    const FIXTURE_MODULUS: &str = "7lwICTwFDPsqKp-jQTx5kGWbea0KNnIWYGRDqfqiK7ultoTvn4oyiUtT5il14OkSr7JTz-gFFKsIg9_Vcsby12Cyc86xbttYL8LJHSUM4OLwB_x1NzMBuijWcOuoGACqLWUi8upA2yM_7z8XyDeQONxNA880w4MJ0xQyXk6Rk-rVmU7SJJgBgDC1lAsfc5j0k4e3wo4L5zBV9_XdhqyqtOZdsrKSuVLHA-lBT-xSKWAl_pPMTIi7NMWvpiRTQ_2vu0bty1EqgitxQ164U-UZChY_ILX_ooweeOODS7l3vcQuyUQ3rbA-b7VFOQpeIE134NYnhWhlJRNN8Bc2MrzYew";

    fn fixture_token() -> String {
        format!("{FIXTURE_HEADER}.{FIXTURE_PAYLOAD}.{FIXTURE_SIGNATURE}")
    }

    fn fixture_jwks() -> Result<JwkSet, String> {
        serde_json::from_value(serde_json::json!({
            "keys": [{
                "kty": "RSA",
                "use": "sig",
                "alg": "RS256",
                "kid": "fixture-key",
                "n": FIXTURE_MODULUS,
                "e": "AQAB"
            }]
        }))
        .map_err(|error| format!("parse neutral public JWKS fixture: {error}"))
    }

    fn fixture_config() -> Result<GoogleOidcConfig, String> {
        GoogleOidcConfig::new(
            "fixture-client",
            "https://steward.example.test",
            "https://steward.example.test/admin/auth/callback",
            "example.com",
            OrganizationId::parse("org_example")?,
        )
    }

    struct FixtureTransport {
        gets: Mutex<VecDeque<HttpResponse>>,
        posts: Mutex<VecDeque<HttpResponse>>,
    }

    impl FixtureTransport {
        fn new(gets: Vec<HttpResponse>, posts: Vec<HttpResponse>) -> Self {
            Self {
                gets: Mutex::new(gets.into()),
                posts: Mutex::new(posts.into()),
            }
        }
    }

    impl GoogleHttpTransport for FixtureTransport {
        fn get<'a>(&'a self, _url: &'a str, _max_body: usize) -> HttpFuture<'a> {
            Box::pin(async move {
                self.gets
                    .lock()
                    .map_err(|_| HttpFailure::Unavailable)?
                    .pop_front()
                    .ok_or(HttpFailure::Unavailable)
            })
        }

        fn post_token<'a>(
            &'a self,
            _url: &'a str,
            request: TokenExchangeRequest<'a>,
            _max_body: usize,
        ) -> HttpFuture<'a> {
            Box::pin(async move {
                if request.client_secret.0.is_empty() {
                    return Err(HttpFailure::InvalidResponse);
                }
                self.posts
                    .lock()
                    .map_err(|_| HttpFailure::Unavailable)?
                    .pop_front()
                    .ok_or(HttpFailure::Unavailable)
            })
        }
    }

    struct BlockingFailureTransport {
        calls: AtomicUsize,
        started: Notify,
        release: Notify,
        id_token: String,
    }

    impl BlockingFailureTransport {
        fn new(id_token: String) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                started: Notify::new(),
                release: Notify::new(),
                id_token,
            }
        }
    }

    impl GoogleHttpTransport for BlockingFailureTransport {
        fn get<'a>(&'a self, _url: &'a str, _max_body: usize) -> HttpFuture<'a> {
            Box::pin(async move {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                if call == 0 {
                    self.started.notify_one();
                    self.release.notified().await;
                }
                Err(HttpFailure::Unavailable)
            })
        }

        fn post_token<'a>(
            &'a self,
            _url: &'a str,
            _request: TokenExchangeRequest<'a>,
            _max_body: usize,
        ) -> HttpFuture<'a> {
            Box::pin(async move {
                json_response(serde_json::json!({ "id_token": self.id_token.as_str() }))
                    .map_err(|_| HttpFailure::InvalidResponse)
            })
        }
    }

    struct CountingFailureTransport {
        calls: AtomicUsize,
    }

    impl CountingFailureTransport {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl GoogleHttpTransport for CountingFailureTransport {
        fn get<'a>(&'a self, _url: &'a str, _max_body: usize) -> HttpFuture<'a> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Err(HttpFailure::Unavailable)
            })
        }

        fn post_token<'a>(
            &'a self,
            _url: &'a str,
            _request: TokenExchangeRequest<'a>,
            _max_body: usize,
        ) -> HttpFuture<'a> {
            Box::pin(async { Err(HttpFailure::Unavailable) })
        }
    }

    fn json_response(value: serde_json::Value) -> Result<HttpResponse, String> {
        Ok(HttpResponse {
            status: 200,
            content_type: Some("application/json".to_owned()),
            cache_control: Some("public, max-age=300".to_owned()),
            body: serde_json::to_vec(&value)
                .map_err(|error| format!("serialize fixture response: {error}"))?,
        })
    }

    fn fixture_discovery() -> serde_json::Value {
        serde_json::json!({
            "issuer": "https://accounts.google.com",
            "authorization_endpoint": "https://accounts.google.com/o/oauth2/v2/auth",
            "token_endpoint": "https://oauth2.googleapis.com/token",
            "jwks_uri": "https://www.googleapis.com/oauth2/v3/certs",
            "response_types_supported": ["code"],
            "id_token_signing_alg_values_supported": ["RS256"],
            "code_challenge_methods_supported": ["S256"]
        })
    }

    fn fixture_jwks_value() -> serde_json::Value {
        serde_json::json!({
            "keys": [{
                "kty": "RSA", "use": "sig", "alg": "RS256", "kid": "fixture-key",
                "n": FIXTURE_MODULUS, "e": "AQAB"
            }]
        })
    }

    fn other_jwks_value() -> serde_json::Value {
        serde_json::json!({
            "keys": [{
                "kty": "RSA", "use": "sig", "alg": "RS256", "kid": "other-key",
                "n": FIXTURE_MODULUS, "e": "AQAB"
            }]
        })
    }

    fn fixture_now() -> u64 {
        1_100
    }

    fn seed_provider_cache(
        provider: &GoogleOidcProvider,
        now: u64,
        jwks_expires_at: u64,
        generation: u64,
    ) -> Result<(), String> {
        let discovery: DiscoveryDocument = serde_json::from_value(fixture_discovery())
            .map_err(|error| format!("parse discovery fixture: {error}"))?;
        let discovery = discovery
            .validate()
            .map_err(|error| format!("validate discovery fixture: {error:?}"))?;
        let mut cache = provider
            .cache
            .lock()
            .map_err(|_| "lock seeded provider cache".to_owned())?;
        cache.discovery = Some(Cached {
            value: discovery,
            expires_at: now + 3_600,
        });
        cache.jwks = Some(Cached {
            value: fixture_jwks()?,
            expires_at: jwks_expires_at,
        });
        cache.jwks_generation = generation;
        Ok(())
    }

    #[tokio::test]
    async fn production_provider_exchanges_and_verifies_without_retaining_tokens()
    -> Result<(), String> {
        let transport = Arc::new(FixtureTransport::new(
            vec![
                json_response(fixture_discovery())?,
                json_response(fixture_jwks_value())?,
            ],
            vec![json_response(
                serde_json::json!({"id_token": fixture_token()}),
            )?],
        ));
        let provider = GoogleOidcProvider::with_transport(
            fixture_config()?,
            "fixture-secret".to_owned(),
            transport,
            fixture_now,
        )?;
        let claims = provider
            .exchange_code(
                "one-time-code",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "https://steward.example.test/admin/auth/callback",
                "fixture-nonce",
            )
            .await
            .map_err(|error| format!("production fixture exchange: {error:?}"))?;
        assert_eq!(claims.subject, "fixture-subject");
        Ok(())
    }

    #[tokio::test]
    async fn unknown_key_refreshes_once_and_token_endpoint_errors_are_bounded() -> Result<(), String>
    {
        let transport = Arc::new(FixtureTransport::new(
            vec![
                json_response(fixture_discovery())?,
                json_response(other_jwks_value())?,
                json_response(fixture_jwks_value())?,
            ],
            vec![json_response(
                serde_json::json!({"id_token": fixture_token()}),
            )?],
        ));
        let provider = GoogleOidcProvider::with_transport(
            fixture_config()?,
            "fixture-secret".to_owned(),
            transport.clone(),
            fixture_now,
        )?;
        assert!(
            provider
                .exchange_code(
                    "one-time-code",
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "https://steward.example.test/admin/auth/callback",
                    "fixture-nonce",
                )
                .await
                .is_ok(),
            "an unknown kid must trigger one JWKS rotation refresh"
        );
        assert_eq!(
            transport
                .gets
                .lock()
                .map_err(|_| "lock fixture GET queue".to_owned())?
                .len(),
            0,
            "rotation must consume exactly discovery, initial JWKS, and one refresh"
        );

        for (status, expected) in [
            (400, BrowserAuthFailure::InvalidIdentity),
            (503, BrowserAuthFailure::ProviderUnavailable),
        ] {
            let mut response = json_response(serde_json::json!({"error": "fixture-error"}))?;
            response.status = status;
            let provider = GoogleOidcProvider::with_transport(
                fixture_config()?,
                "sensitive-fixture-value".to_owned(),
                Arc::new(FixtureTransport::new(
                    vec![json_response(fixture_discovery())?],
                    vec![response],
                )),
                fixture_now,
            )?;
            let result = provider
                .exchange_code(
                    "sensitive-one-time-code",
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "https://steward.example.test/admin/auth/callback",
                    "fixture-nonce",
                )
                .await;
            assert!(matches!(result, Err(error) if error == expected));
            let category = format!("{:?}", result.err());
            assert!(!category.contains("sensitive"));
        }
        Ok(())
    }

    #[tokio::test]
    async fn discovery_and_response_envelopes_fail_closed() -> Result<(), String> {
        let mut wrong_discovery = fixture_discovery();
        wrong_discovery["issuer"] = serde_json::json!("https://issuer.example.test");
        let provider = GoogleOidcProvider::with_transport(
            fixture_config()?,
            "fixture-secret".to_owned(),
            Arc::new(FixtureTransport::new(
                vec![json_response(wrong_discovery)?],
                vec![],
            )),
            fixture_now,
        )?;
        assert!(matches!(
            provider
                .exchange_code(
                    "one-time-code",
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "https://steward.example.test/admin/auth/callback",
                    "fixture-nonce",
                )
                .await,
            Err(BrowserAuthFailure::ProviderUnavailable)
        ));

        assert_eq!(cache_ttl(Some("public, max-age=999999")), 3_600);
        assert_eq!(cache_ttl(Some("no-store, max-age=300")), 0);
        assert_eq!(cache_ttl(Some("public, max-age=invalid")), 300);
        Ok(())
    }

    #[test]
    fn client_secret_is_an_exact_bounded_raw_scalar() {
        assert!(GoogleClientSecret::new(String::new()).is_err());
        assert!(GoogleClientSecret::new(" fixture-secret".to_owned()).is_err());
        assert!(GoogleClientSecret::new("fixture-secret ".to_owned()).is_err());
        assert!(GoogleClientSecret::new("fixture-secret\n".to_owned()).is_err());
        assert!(GoogleClientSecret::new("fixture\tsecret".to_owned()).is_err());
        assert!(GoogleClientSecret::new("x".repeat(4_097)).is_err());
        assert!(GoogleClientSecret::new("fixture-secret".to_owned()).is_ok());
    }

    #[tokio::test]
    async fn invalid_rotation_preserves_fresh_last_good_and_expiry_fails_closed()
    -> Result<(), String> {
        let unknown_key_token =
            token_with_header(serde_json::json!({"alg":"RS256","typ":"JWT","kid":"rotated-key"}))?;
        let transport = Arc::new(FixtureTransport::new(
            vec![
                json_response(fixture_discovery())?,
                json_response(fixture_jwks_value())?,
                json_response(serde_json::json!({"keys": []}))?,
            ],
            vec![
                json_response(serde_json::json!({"id_token": fixture_token()}))?,
                json_response(serde_json::json!({"id_token": unknown_key_token}))?,
                json_response(serde_json::json!({"id_token": fixture_token()}))?,
            ],
        ));
        let provider = GoogleOidcProvider::with_transport(
            fixture_config()?,
            "fixture-secret".to_owned(),
            transport,
            fixture_now,
        )?;
        let exchange = || {
            provider.exchange_code(
                "one-time-code",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "https://steward.example.test/admin/auth/callback",
                "fixture-nonce",
            )
        };
        assert!(exchange().await.is_ok());
        assert!(matches!(
            exchange().await,
            Err(BrowserAuthFailure::ProviderUnavailable)
        ));
        assert!(
            exchange().await.is_ok(),
            "an invalid rotation response must not replace a fresh last-good key set"
        );

        let clock = Arc::new(AtomicU64::new(1_100));
        let provider = GoogleOidcProvider::with_transport(
            fixture_config()?,
            "fixture-secret".to_owned(),
            Arc::new(FixtureTransport::new(
                vec![
                    json_response(fixture_discovery())?,
                    json_response(fixture_jwks_value())?,
                ],
                vec![
                    json_response(serde_json::json!({"id_token": fixture_token()}))?,
                    json_response(serde_json::json!({"id_token": fixture_token()}))?,
                ],
            )),
            {
                let clock = Arc::clone(&clock);
                move || clock.load(Ordering::SeqCst)
            },
        )?;
        assert!(
            provider
                .exchange_code(
                    "one-time-code",
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "https://steward.example.test/admin/auth/callback",
                    "fixture-nonce",
                )
                .await
                .is_ok()
        );
        clock.store(1_500, Ordering::SeqCst);
        assert!(matches!(
            provider
                .exchange_code(
                    "one-time-code",
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "https://steward.example.test/admin/auth/callback",
                    "fixture-nonce",
                )
                .await,
            Err(BrowserAuthFailure::ProviderUnavailable)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_unknown_kid_failure_is_serialized_and_backed_off_per_generation()
    -> Result<(), String> {
        let clock = Arc::new(AtomicU64::new(1_100));
        let unknown_key_token =
            token_with_header(serde_json::json!({"alg":"RS256","typ":"JWT","kid":"rotated-key"}))?;
        let transport = Arc::new(BlockingFailureTransport::new(unknown_key_token));
        let provider = GoogleOidcProvider::with_transport(
            fixture_config()?,
            "fixture-secret".to_owned(),
            transport.clone(),
            {
                let clock = Arc::clone(&clock);
                move || clock.load(Ordering::SeqCst)
            },
        )?;
        seed_provider_cache(&provider, 1_100, 2_000, 7)?;

        let first_provider = provider.clone();
        let first = tokio::spawn(async move {
            first_provider
                .exchange_code(
                    "one-time-code",
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "https://steward.example.test/admin/auth/callback",
                    "fixture-nonce",
                )
                .await
        });
        transport.started.notified().await;
        let second_provider = provider.clone();
        let second = tokio::spawn(async move {
            second_provider
                .exchange_code(
                    "one-time-code",
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "https://steward.example.test/admin/auth/callback",
                    "fixture-nonce",
                )
                .await
        });
        tokio::task::yield_now().await;
        transport.release.notify_one();

        assert!(matches!(
            first.await,
            Ok(Err(BrowserAuthFailure::ProviderUnavailable))
        ));
        assert!(matches!(
            second.await,
            Ok(Err(BrowserAuthFailure::ProviderUnavailable))
        ));
        assert_eq!(
            transport.calls.load(Ordering::SeqCst),
            1,
            "waiters observing the same failed generation must not retry serially"
        );
        assert!(matches!(
            provider.jwks(Some(7)).await,
            Err(BrowserAuthFailure::ProviderUnavailable)
        ));
        assert_eq!(transport.calls.load(Ordering::SeqCst), 1);

        clock.store(
            1_100 + JWKS_REFRESH_FAILURE_BACKOFF_SECONDS,
            Ordering::SeqCst,
        );
        assert!(provider.jwks(Some(7)).await.is_err());
        assert_eq!(
            transport.calls.load(Ordering::SeqCst),
            2,
            "the same generation may retry only after the bounded backoff"
        );

        provider
            .cache
            .lock()
            .map_err(|_| "lock provider generation".to_owned())?
            .jwks_generation = 8;
        assert!(provider.jwks(Some(8)).await.is_err());
        assert_eq!(
            transport.calls.load(Ordering::SeqCst),
            3,
            "a failure record for one generation must not suppress a later generation"
        );
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_unknown_kid_discovery_failure_is_backed_off_per_jwks_generation()
    -> Result<(), String> {
        let transport = Arc::new(BlockingFailureTransport::new(String::new()));
        let provider = GoogleOidcProvider::with_transport(
            fixture_config()?,
            "fixture-secret".to_owned(),
            transport.clone(),
            fixture_now,
        )?;
        seed_provider_cache(&provider, 1_100, 2_000, 7)?;
        provider
            .cache
            .lock()
            .map_err(|_| "expire discovery fixture".to_owned())?
            .discovery
            .as_mut()
            .ok_or_else(|| "missing discovery fixture".to_owned())?
            .expires_at = 1_100;

        let first_provider = provider.clone();
        let first = tokio::spawn(async move { first_provider.jwks(Some(7)).await });
        transport.started.notified().await;
        let second_provider = provider.clone();
        let second = tokio::spawn(async move { second_provider.jwks(Some(7)).await });
        tokio::task::yield_now().await;
        transport.release.notify_one();

        assert!(matches!(
            first.await,
            Ok(Err(BrowserAuthFailure::ProviderUnavailable))
        ));
        assert!(matches!(
            second.await,
            Ok(Err(BrowserAuthFailure::ProviderUnavailable))
        ));
        assert_eq!(
            transport.calls.load(Ordering::SeqCst),
            1,
            "waiters must not repeat a failed prerequisite discovery request"
        );
        let failure = provider
            .cache
            .lock()
            .map_err(|_| "read refresh failure".to_owned())?
            .jwks_refresh_failure
            .ok_or_else(|| "discovery failure was not recorded".to_owned())?;
        assert_eq!(failure.observed_generation, 7);
        assert!(provider.jwks(Some(7)).await.is_err());
        assert_eq!(transport.calls.load(Ordering::SeqCst), 1);

        provider
            .cache
            .lock()
            .map_err(|_| "advance JWKS generation".to_owned())?
            .jwks_generation = 8;
        assert!(provider.jwks(Some(8)).await.is_err());
        assert_eq!(
            transport.calls.load(Ordering::SeqCst),
            2,
            "a discovery failure observed for generation 7 must not suppress generation 8"
        );
        Ok(())
    }

    #[tokio::test]
    async fn jwks_waiter_recomputes_time_and_rejects_cache_expired_while_queued()
    -> Result<(), String> {
        let clock = Arc::new(AtomicU64::new(1_100));
        let now_calls = Arc::new(AtomicUsize::new(0));
        let transport = Arc::new(CountingFailureTransport::new());
        let provider = GoogleOidcProvider::with_transport(
            fixture_config()?,
            "fixture-secret".to_owned(),
            transport.clone(),
            {
                let clock = Arc::clone(&clock);
                let now_calls = Arc::clone(&now_calls);
                move || {
                    now_calls.fetch_add(1, Ordering::SeqCst);
                    clock.load(Ordering::SeqCst)
                }
            },
        )?;
        seed_provider_cache(&provider, 1_100, 1_101, 8)?;

        let permit = provider
            .refresh_gate
            .acquire()
            .await
            .map_err(|error| format!("acquire fixture refresh gate: {error:?}"))?;
        let queued_provider = provider.clone();
        let queued = tokio::spawn(async move { queued_provider.jwks(Some(7)).await });
        while now_calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        clock.store(1_102, Ordering::SeqCst);
        drop(permit);

        assert!(matches!(
            queued.await,
            Ok(Err(BrowserAuthFailure::ProviderUnavailable))
        ));
        assert_eq!(
            transport.calls.load(Ordering::SeqCst),
            1,
            "a cache entry that expired while queued must not be returned"
        );
        Ok(())
    }

    fn fixture_claims() -> IdTokenClaims {
        IdTokenClaims {
            iss: "https://accounts.google.com".to_owned(),
            sub: "fixture-subject".to_owned(),
            aud: Audience::Single("fixture-client".to_owned()),
            azp: Some("fixture-client".to_owned()),
            exp: 2_000,
            iat: 1_000,
            nonce: "fixture-nonce".to_owned(),
            email: "alice@example.com".to_owned(),
            email_verified: true,
            hd: "example.com".to_owned(),
            name: Some("Alice Example".to_owned()),
        }
    }

    #[test]
    fn hostile_claim_matrix_rejects_each_identity_escape() {
        let check = |claims| {
            validate_claims(
                claims,
                "fixture-client",
                "example.com",
                "fixture-nonce",
                1_100,
            )
        };
        let verified_display_name = check(fixture_claims())
            .ok()
            .and_then(|claims| claims.display_name);
        assert_eq!(verified_display_name.as_deref(), Some("Alice Example"));
        let mut cases = Vec::new();
        let mut claims = fixture_claims();
        claims.iss = "accounts.google.com".to_owned();
        cases.push(claims);
        let mut claims = fixture_claims();
        claims.aud =
            Audience::Multiple(vec!["fixture-client".to_owned(), "other-client".to_owned()]);
        cases.push(claims);
        let mut claims = fixture_claims();
        claims.azp = Some("other-client".to_owned());
        cases.push(claims);
        let mut claims = fixture_claims();
        claims.sub = "subject with spaces".to_owned();
        cases.push(claims);
        let mut claims = fixture_claims();
        claims.exp = 1_040;
        cases.push(claims);
        let mut claims = fixture_claims();
        claims.iat = 1_161;
        cases.push(claims);
        let mut claims = fixture_claims();
        claims.iat = 799;
        cases.push(claims);
        let mut claims = fixture_claims();
        claims.name = Some("\0Alice".to_owned());
        cases.push(claims);
        let mut claims = fixture_claims();
        claims.name = Some("a".repeat(257));
        cases.push(claims);
        let mut claims = fixture_claims();
        claims.nonce = "other-nonce".to_owned();
        cases.push(claims);
        let mut claims = fixture_claims();
        claims.email_verified = false;
        cases.push(claims);
        let mut claims = fixture_claims();
        claims.hd = "example.org".to_owned();
        cases.push(claims);

        for claims in cases {
            assert!(matches!(check(claims), Err(IdTokenError::InvalidClaims)));
        }
        let mut valid_without_azp = fixture_claims();
        valid_without_azp.azp = None;
        assert!(check(valid_without_azp).is_ok());
    }

    fn token_with_header(header: serde_json::Value) -> Result<String, String> {
        let header = serde_json::to_vec(&header)
            .map_err(|error| format!("serialize hostile JOSE header: {error}"))?;
        Ok(format!(
            "{}.{FIXTURE_PAYLOAD}.{FIXTURE_SIGNATURE}",
            crate::browser_auth::base64_url(&header)
        ))
    }

    #[test]
    fn algorithm_confusion_embedded_keys_and_missing_kid_are_rejected() -> Result<(), String> {
        for (header, expected) in [
            (
                serde_json::json!({"alg":"HS256","kid":"fixture-key"}),
                IdTokenError::ForbiddenHeader,
            ),
            (
                serde_json::json!({"alg":"RS256"}),
                IdTokenError::MissingKeyId,
            ),
            (
                serde_json::json!({"alg":"RS256","kid":"fixture-key","jku":"https://keys.example.test"}),
                IdTokenError::ForbiddenHeader,
            ),
            (
                serde_json::json!({"alg":"RS256","kid":"fixture-key","crit":["fixture"]}),
                IdTokenError::ForbiddenHeader,
            ),
        ] {
            assert!(matches!(
                verify_id_token(
                    &token_with_header(header)?,
                    &fixture_jwks()?,
                    "fixture-client",
                    "example.com",
                    "fixture-nonce",
                    1_100,
                ),
                Err(error) if error == expected
            ));
        }

        let wrong_ops: JwkSet = serde_json::from_value(serde_json::json!({
            "keys": [{
                "kty":"RSA", "use":"sig", "key_ops":["sign"], "alg":"RS256",
                "kid":"fixture-key", "n":FIXTURE_MODULUS, "e":"AQAB"
            }]
        }))
        .map_err(|error| format!("parse hostile key-ops fixture: {error}"))?;
        assert!(matches!(
            verify_id_token(
                &fixture_token(),
                &wrong_ops,
                "fixture-client",
                "example.com",
                "fixture-nonce",
                1_100,
            ),
            Err(IdTokenError::InvalidKey)
        ));
        Ok(())
    }

    #[test]
    fn signed_google_id_token_requires_exact_identity_boundary() -> Result<(), String> {
        let claims = verify_id_token(
            &fixture_token(),
            &fixture_jwks()?,
            "fixture-client",
            "example.com",
            "fixture-nonce",
            1_100,
        )
        .map_err(|error| format!("verify neutral signed fixture: {error:?}"))?;
        assert_eq!(claims.issuer, "https://accounts.google.com");
        assert_eq!(claims.subject, "fixture-subject");
        assert_eq!(claims.email, "alice@example.com");
        assert!(claims.email_verified);
        assert_eq!(claims.hosted_domain, "example.com");
        assert_eq!(claims.nonce, "fixture-nonce");

        for (client_id, hosted_domain, nonce, now) in [
            ("other-client", "example.com", "fixture-nonce", 1_100),
            ("fixture-client", "example.org", "fixture-nonce", 1_100),
            ("fixture-client", "example.com", "other-nonce", 1_100),
            ("fixture-client", "example.com", "fixture-nonce", 1_301),
        ] {
            assert!(matches!(
                verify_id_token(
                    &fixture_token(),
                    &fixture_jwks()?,
                    client_id,
                    hosted_domain,
                    nonce,
                    now,
                ),
                Err(IdTokenError::InvalidClaims)
            ));
        }
        Ok(())
    }

    #[test]
    fn jose_header_and_jwks_fail_closed_before_claims() -> Result<(), String> {
        let malformed = "not-a-jwt";
        assert!(matches!(
            verify_id_token(
                malformed,
                &fixture_jwks()?,
                "fixture-client",
                "example.com",
                "fixture-nonce",
                1_100,
            ),
            Err(IdTokenError::Malformed)
        ));

        let duplicate: JwkSet = serde_json::from_value(serde_json::json!({
            "keys": [
                {"kty":"RSA","use":"sig","alg":"RS256","kid":"fixture-key","n":FIXTURE_MODULUS,"e":"AQAB"},
                {"kty":"RSA","use":"sig","alg":"RS256","kid":"fixture-key","n":FIXTURE_MODULUS,"e":"AQAB"}
            ]
        }))
        .map_err(|error| format!("parse duplicate-key fixture: {error}"))?;
        assert!(matches!(
            verify_id_token(
                &fixture_token(),
                &duplicate,
                "fixture-client",
                "example.com",
                "fixture-nonce",
                1_100,
            ),
            Err(IdTokenError::DuplicateKeyId)
        ));

        let bad_signature = format!(
            "{FIXTURE_HEADER}.{FIXTURE_PAYLOAD}.A{}",
            &FIXTURE_SIGNATURE[1..]
        );
        assert!(matches!(
            verify_id_token(
                &bad_signature,
                &fixture_jwks()?,
                "fixture-client",
                "example.com",
                "fixture-nonce",
                1_100,
            ),
            Err(IdTokenError::InvalidSignature)
        ));
        Ok(())
    }
}
