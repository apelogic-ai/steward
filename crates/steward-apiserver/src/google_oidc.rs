use std::collections::BTreeSet;
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
use steward_types::{OrganizationId, OrganizationIdentityPolicy};

use crate::browser_auth::{
    BrowserAuthFailure, BrowserAuthorizationRequest, BrowserFuture, BrowserOidcProvider,
    BrowserUserTokenVerifier, GoogleOidcConfig, VerifiedOrganizationClaims,
    VerifiedUserTokenClaims,
};

const MAX_ID_TOKEN_BYTES: usize = 16 * 1024;
const MAX_DISCOVERY_BYTES: usize = 32 * 1024;
const MAX_JWKS_BYTES: usize = 128 * 1024;
const MAX_TOKEN_RESPONSE_BYTES: usize = 32 * 1024;
const MAX_USER_TOKEN_AGE_SECONDS: u64 = 86_400;
const MAX_USER_TOKEN_CLIENTS: usize = 32;
const MAX_USER_TOKEN_CLIENT_ID_BYTES: usize = 128;
const MAX_USER_TOKEN_KEYS: usize = 64;
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

#[cfg(test)]
const USER_TOKEN_FIXTURE_HEADER: &str =
    "eyJhbGciOiJSUzI1NiIsInR5cCI6ImF0K2p3dCIsImtpZCI6InVzZXItdG9rZW4tZml4dHVyZS1rZXkifQ";
#[cfg(test)]
const USER_TOKEN_FIXTURE_PAYLOAD: &str = "eyJpc3MiOiJodHRwczovL2lzc3Vlci5leGFtcGxlLnRlc3QiLCJzdWIiOiJmaXh0dXJlLXVzZXItc3ViamVjdCIsImF6cCI6InBvcnRhbC1jbGllbnQiLCJleHAiOjIwMDAsImlhdCI6MTAwMCwibmJmIjoxMDAwLCJlbWFpbCI6ImFsaWNlQGV4YW1wbGUuY29tIiwiZW1haWxfdmVyaWZpZWQiOnRydWUsIm5hbWUiOiJBbGljZSBFeGFtcGxlIiwiYXVkIjoic3Rld2FyZC1hcGkifQ";
#[cfg(test)]
const USER_TOKEN_FIXTURE_SIGNATURE: &str = "Q6B63SKh36prVYxUO2HanbqtIITSOdFpJga6_tQjD9kkRjXESYaKCEMR_gVKb0xwirkthGfctk5e2GZAbtBF0mJUbGWH3EmJvFY_X8f6xmlDJgdL-01HGimbjwSnfGRCa5I7dNw2QUVfzz2Zu4SeFEOxgUzYcZS4ac5MiLO-tidTQrQyrtu8F_tLPXkRGF1BgSnUz7rIvm6vOnBTOaNG7tjAyApbd0ho-pN7kCJy7t4HpDmK6ygC_RmBxHl7gxPR6CduNa0eJrqGQVfqY3yE_2H6dVNFxegfZJ1A_vjlHJgjYPtDW9rhP_PH3KskuGYrQChUxU-CU41UsOQBHkDH4A";
#[cfg(test)]
const USER_TOKEN_FIXTURE_MODULUS: &str = "p4AkZb7cQ3OdRD6ymSZ2km-qeVKFV2shZ_bgi9D_uzULB5hzC4DIFuErY2G-60NJgO2o9WgolasVN6rmjNGVAIRiKble3zc9fs_Ud2u1bDiNn_VWeQwu03najyF1aPFIyt-9FnV6HKg5B0Bfh18wr7rs4wPVE6YZkvNjSyw_LElSwPKydsgS_gWl6OKBi1O0D5GT8OTau2QquMFcnfcxw_Vc9oS7MY4LoCY4fl0gjEOijqFQ66N1E1Kc8OAW3mwrmWdGL6wAvHsx7VV6fBFelN8evWJGjXza1-r3psDYpEHfQuhhDDPkyAlyZPUXegF_3GNSi8wBUpT9ydogI8vhMw";
#[cfg(test)]
const USER_ID_TOKEN_FIXTURE_HEADER: &str =
    "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCIsImtpZCI6InVzZXItdG9rZW4tZml4dHVyZS1rZXkifQ";
#[cfg(test)]
const USER_ID_TOKEN_FIXTURE_PAYLOAD: &str = "eyJpc3MiOiJodHRwczovL2lzc3Vlci5leGFtcGxlLnRlc3QiLCJzdWIiOiJmaXh0dXJlLXVzZXItc3ViamVjdCIsImF6cCI6InBvcnRhbC1jbGllbnQiLCJleHAiOjIwMDAsImlhdCI6MTAwMCwibmJmIjoxMDAwLCJlbWFpbCI6ImFsaWNlQGV4YW1wbGUuY29tIiwiZW1haWxfdmVyaWZpZWQiOnRydWUsIm5hbWUiOiJBbGljZSBFeGFtcGxlIiwiYXVkIjoicG9ydGFsLWNsaWVudCJ9";
#[cfg(test)]
const USER_ID_TOKEN_FIXTURE_SIGNATURE: &str = "SUG09-K8QFmruvGZpkujk4JhW9X-kIKR6bOtaetKaJsOoRr52B0r9Ihnieo2YHVm1ZW5MYUZo_jlkZAVQ1iyPBcIllOcYIL5C4AjZLfLFWCx3AHSAf1UJv5phzbW4fbuoKE4raSjDT1VyvikUTAU2h86iJ59G4PQVpdn46190MdeJ6dvyJ78niRCezyQNtJER_nVPG7qqhWqBF2AIBgFpTHdQ-c78TBUOcENWvgirZPrbLXO-pYnWPcjKPEGaOzBavQcl7-SQnm4Zokl_rhv2Yyw2e98NGw0yssus1BCVfFO_CxdzjjY2W1AMFREfVwAcUtsucX0wqH14dK3D_7w8g";

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

#[derive(Clone, Deserialize)]
struct UserTokenClaims {
    iss: String,
    sub: String,
    aud: Audience,
    #[serde(default)]
    azp: Option<String>,
    #[serde(default)]
    client_id: Option<String>,
    exp: u64,
    iat: u64,
    #[serde(default)]
    nbf: Option<u64>,
    #[serde(default)]
    name: Option<String>,
    #[serde(flatten)]
    provider_claims: serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone)]
struct UserTokenIdentityConfig {
    issuer: String,
    discovery_endpoint: String,
    identity_policy: OrganizationIdentityPolicy,
    organization_claim: String,
    user_id_claim: String,
    email_claim: String,
    trust_email: bool,
}

pub struct UserTokenSettings {
    pub issuer: String,
    pub organization_claim: String,
    pub organization_id: OrganizationId,
    pub user_id_claim: String,
    pub email_claim: String,
    pub trust_email: bool,
    pub api_audience: String,
    pub portal_audience: Option<String>,
    pub frontend_client_ids: Vec<String>,
    pub maximum_token_age_seconds: u64,
    pub required_typ: Option<String>,
}

#[derive(Clone)]
pub struct UserTokenConfig {
    identity: UserTokenIdentityConfig,
    api_audience: String,
    portal_audience: Option<String>,
    frontend_client_ids: BTreeSet<String>,
    maximum_token_age_seconds: u64,
    required_typ: Option<String>,
}

impl UserTokenConfig {
    pub fn new(settings: UserTokenSettings) -> Result<Self, String> {
        let UserTokenSettings {
            issuer,
            organization_claim,
            organization_id,
            user_id_claim,
            email_claim,
            trust_email,
            api_audience,
            portal_audience,
            frontend_client_ids,
            maximum_token_age_seconds,
            required_typ,
        } = settings;
        let issuer_url = reqwest::Url::parse(&issuer)
            .map_err(|_| "user token issuer must be one exact HTTPS issuer".to_owned())?;
        if issuer_url.scheme() != "https"
            || issuer_url.host_str().is_none()
            || !issuer_url.username().is_empty()
            || issuer_url.password().is_some()
            || issuer_url.query().is_some()
            || issuer_url.fragment().is_some()
            || issuer.len() > 2_048
            || !issuer.is_ascii()
            || (issuer_url
                .host_str()
                .is_some_and(|host| host.eq_ignore_ascii_case("accounts.google.com"))
                && issuer_url.port_or_known_default() == Some(443)
                && issuer_url.path() == "/")
        {
            return Err("user token configuration is invalid".to_owned());
        }
        let valid_claim_name = |value: &str| {
            !value.is_empty()
                && value.len() <= 128
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        };
        if !valid_claim_name(&user_id_claim)
            || !valid_claim_name(&email_claim)
            || matches!(
                user_id_claim.as_str(),
                "iss" | "aud" | "azp" | "client_id" | "exp" | "iat" | "nbf" | "name"
            )
            || matches!(
                email_claim.as_str(),
                "iss" | "sub" | "aud" | "azp" | "client_id" | "exp" | "iat" | "nbf" | "name"
            )
            || user_id_claim == email_claim
        {
            return Err("user token configuration is invalid".to_owned());
        }
        let valid_value = |value: &str| {
            !value.is_empty()
                && value.len() <= 512
                && value.bytes().all(|byte| byte.is_ascii_graphic())
        };
        if !valid_value(&api_audience)
            || portal_audience
                .as_deref()
                .is_some_and(|value| !valid_value(value))
            || frontend_client_ids.is_empty()
            || frontend_client_ids.len() > MAX_USER_TOKEN_CLIENTS
            || maximum_token_age_seconds == 0
            || maximum_token_age_seconds > MAX_USER_TOKEN_AGE_SECONDS
        {
            return Err("user token configuration is invalid".to_owned());
        }
        let required_typ = required_typ
            .filter(|value| !value.is_empty())
            .map(|value| {
                if value.len() > 64 || !value.bytes().all(|byte| byte.is_ascii_graphic()) {
                    Err("user token configuration is invalid".to_owned())
                } else {
                    Ok(value)
                }
            })
            .transpose()?;
        let requested_client_count = frontend_client_ids.len();
        let frontend_client_ids = frontend_client_ids.into_iter().collect::<BTreeSet<_>>();
        let valid_client_id = |value: &str| {
            !value.is_empty()
                && value.len() <= MAX_USER_TOKEN_CLIENT_ID_BYTES
                && value.chars().all(|character| {
                    character.is_ascii_alphanumeric()
                        || matches!(
                            character,
                            '.' | '_' | '-' | '#' | '/' | ':' | '@' | '+' | '(' | ')'
                        )
                })
        };
        if frontend_client_ids.len() != requested_client_count
            || frontend_client_ids.len() > MAX_USER_TOKEN_CLIENTS
            || frontend_client_ids
                .iter()
                .any(|value| !valid_client_id(value))
            || frontend_client_ids.contains(&api_audience)
            || portal_audience
                .as_ref()
                .is_some_and(|audience| frontend_client_ids.contains(audience))
        {
            return Err("user token configuration is invalid".to_owned());
        }
        let identity = UserTokenIdentityConfig {
            discovery_endpoint: format!(
                "{}/.well-known/openid-configuration",
                issuer.trim_end_matches('/')
            ),
            identity_policy: OrganizationIdentityPolicy::new_configured_oidc(
                issuer.clone(),
                organization_claim.clone(),
                organization_id,
            )?,
            issuer,
            organization_claim,
            user_id_claim,
            email_claim,
            trust_email,
        };
        Ok(Self {
            identity,
            api_audience,
            portal_audience,
            frontend_client_ids,
            maximum_token_age_seconds,
            required_typ,
        })
    }
}

#[derive(Clone)]
struct OidcKeyProvider {
    transport: Arc<dyn OidcHttpTransport>,
    now: Arc<dyn Fn() -> u64 + Send + Sync>,
    cache: Arc<Mutex<ProviderCache>>,
    refresh_gate: Arc<RefreshGate>,
    discovery_endpoint: String,
    issuer: String,
    allowed_algorithms: Arc<[Algorithm]>,
}

impl OidcKeyProvider {
    fn new(
        discovery_endpoint: String,
        issuer: String,
        allowed_algorithms: Vec<Algorithm>,
        transport: Arc<dyn OidcHttpTransport>,
        now: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> Self {
        Self {
            transport,
            now,
            cache: Arc::new(Mutex::new(ProviderCache::default())),
            refresh_gate: Arc::new(RefreshGate::default()),
            discovery_endpoint,
            issuer,
            allowed_algorithms: allowed_algorithms.into(),
        }
    }

    async fn jwks_for_token(&self, token: &str) -> Result<JwkSet, BrowserAuthFailure> {
        let header = decode_header(token).map_err(|_| BrowserAuthFailure::InvalidIdentity)?;
        if !self.allowed_algorithms.contains(&header.alg) {
            return Err(BrowserAuthFailure::InvalidIdentity);
        }
        let kid = header
            .kid
            .filter(|kid| !kid.is_empty() && kid.len() <= 128 && kid.is_ascii())
            .ok_or(BrowserAuthFailure::InvalidIdentity)?;
        let jwks = self.jwks(None, None).await?;
        let generation = self
            .cache
            .lock()
            .map_err(|_| BrowserAuthFailure::ProviderUnavailable)?
            .jwks_generation;
        match select_user_verification_key(&jwks, &kid, header.alg) {
            Ok(_) => Ok(jwks),
            Err(IdTokenError::UnknownKeyId) => {
                self.jwks(Some(generation), Some((&kid, header.alg))).await
            }
            Err(error) => Err(map_id_token_error(error)),
        }
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
            .get(&self.discovery_endpoint, MAX_DISCOVERY_BYTES)
            .await
            .map_err(map_http_failure)?;
        if response.body.len() > MAX_DISCOVERY_BYTES {
            return Err(BrowserAuthFailure::ProviderUnavailable);
        }
        let ttl = validate_json_response(&response)?;
        let document: DiscoveryDocument = serde_json::from_slice(&response.body)
            .map_err(|_| BrowserAuthFailure::ProviderUnavailable)?;
        let document = document.validate_issuer(&self.issuer)?;
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

    async fn jwks(
        &self,
        force_after_generation: Option<u64>,
        required_key: Option<(&str, Algorithm)>,
    ) -> Result<JwkSet, BrowserAuthFailure> {
        let now = (self.now)();
        let cached = self
            .cache
            .lock()
            .map_err(|_| BrowserAuthFailure::ProviderUnavailable)?
            .jwks
            .as_ref()
            .filter(|cached| cached.expires_at > now)
            .map(|cached| cached.value.clone());
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
                match required_key {
                    Some((kid, algorithm)) => {
                        match select_user_verification_key(&jwks, kid, algorithm) {
                            Ok(_) => return Ok(jwks),
                            Err(IdTokenError::UnknownKeyId) => {}
                            Err(error) => return Err(map_id_token_error(error)),
                        }
                    }
                    None => return Ok(jwks),
                }
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
            validate_oidc_jwks(&jwks, &self.allowed_algorithms)?;
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
                match required_key {
                    Some((kid, algorithm)) => {
                        match select_user_verification_key(&jwks, kid, algorithm) {
                            Ok(_) => Ok(jwks),
                            Err(IdTokenError::UnknownKeyId) => {
                                cache.jwks_refresh_failure = Some(JwksRefreshFailure {
                                    observed_generation: cache.jwks_generation,
                                    retry_at: stored_at
                                        .saturating_add(JWKS_REFRESH_FAILURE_BACKOFF_SECONDS),
                                });
                                Err(BrowserAuthFailure::InvalidIdentity)
                            }
                            Err(error) => Err(map_id_token_error(error)),
                        }
                    }
                    None => Ok(jwks),
                }
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

#[derive(Clone)]
pub struct OidcUserTokenVerifier {
    keys: OidcKeyProvider,
    config: UserTokenConfig,
}

impl OidcUserTokenVerifier {
    pub fn new(config: UserTokenConfig) -> Result<Self, String> {
        Ok(Self::from_parts(
            config,
            Arc::new(ReqwestOidcTransport::new()?),
            Arc::new(crate::browser_auth::epoch_seconds),
        ))
    }

    fn from_parts(
        config: UserTokenConfig,
        transport: Arc<dyn OidcHttpTransport>,
        now: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> Self {
        let keys = OidcKeyProvider::new(
            config.identity.discovery_endpoint.clone(),
            config.identity.issuer.clone(),
            vec![Algorithm::RS256, Algorithm::ES256],
            transport,
            now,
        );
        Self { keys, config }
    }

    #[cfg(test)]
    fn with_transport<F>(
        config: UserTokenConfig,
        transport: Arc<dyn OidcHttpTransport>,
        now: F,
    ) -> Self
    where
        F: Fn() -> u64 + Send + Sync + 'static,
    {
        Self::from_parts(config, transport, Arc::new(now))
    }
}

#[cfg(test)]
pub(crate) fn fixture_user_access_token() -> String {
    format!(
        "{USER_TOKEN_FIXTURE_HEADER}.{USER_TOKEN_FIXTURE_PAYLOAD}.{USER_TOKEN_FIXTURE_SIGNATURE}"
    )
}

#[cfg(test)]
pub(crate) fn fixture_user_id_token() -> String {
    format!(
        "{USER_ID_TOKEN_FIXTURE_HEADER}.{USER_ID_TOKEN_FIXTURE_PAYLOAD}.{USER_ID_TOKEN_FIXTURE_SIGNATURE}"
    )
}

#[cfg(test)]
pub(crate) fn fixture_user_token_verifier(
    trust_email: bool,
) -> Result<OidcUserTokenVerifier, String> {
    let config = UserTokenConfig::new(UserTokenSettings {
        issuer: "https://issuer.example.test".to_owned(),
        organization_claim: "example.com".to_owned(),
        organization_id: steward_types::OrganizationId::parse("org_example")?,
        user_id_claim: "sub".to_owned(),
        email_claim: "email".to_owned(),
        trust_email,
        api_audience: "steward-api".to_owned(),
        portal_audience: Some("portal-audience".to_owned()),
        frontend_client_ids: vec!["portal-client".to_owned()],
        maximum_token_age_seconds: 300,
        required_typ: None,
    })?;
    let verifier = OidcUserTokenVerifier::from_parts(
        config,
        Arc::new(ReqwestOidcTransport::new()?),
        Arc::new(|| 1_100),
    );
    let jwks: JwkSet = serde_json::from_value(serde_json::json!({
        "keys": [{
            "kty": "RSA",
            "use": "sig",
            "alg": "RS256",
            "kid": "user-token-fixture-key",
            "n": USER_TOKEN_FIXTURE_MODULUS,
            "e": "AQAB"
        }]
    }))
    .map_err(|error| format!("parse user token fixture JWKS: {error}"))?;
    verifier
        .keys
        .cache
        .lock()
        .map_err(|_| "cache user token fixture JWKS".to_owned())?
        .jwks = Some(Cached {
        value: jwks,
        expires_at: 2_000,
    });
    Ok(verifier)
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
    select_user_verification_key(jwks, kid, Algorithm::RS256)
}

fn select_user_verification_key<'a>(
    jwks: &'a JwkSet,
    kid: &str,
    algorithm: Algorithm,
) -> Result<&'a Jwk, IdTokenError> {
    let mut matching = jwks
        .keys
        .iter()
        .filter(|key| key.common.key_id.as_deref() == Some(kid));
    let key = matching.next().ok_or(IdTokenError::UnknownKeyId)?;
    if matching.next().is_some() {
        return Err(IdTokenError::DuplicateKeyId);
    }
    if verification_key_algorithm(key, &[algorithm]) != Some(algorithm) {
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

fn validate_user_token_claims(
    claims: UserTokenClaims,
    config: &UserTokenConfig,
    now: u64,
) -> Result<VerifiedUserTokenClaims, IdTokenError> {
    let valid_value = |value: &str, maximum: usize| {
        !value.is_empty()
            && value.len() <= maximum
            && value.bytes().all(|byte| byte.is_ascii_graphic())
    };
    let audience_matches = match &claims.aud {
        Audience::Single(audience) => {
            valid_value(audience, 512)
                && (audience == &config.api_audience
                    || config.portal_audience.as_ref() == Some(audience))
        }
        Audience::Multiple(audiences) => {
            !audiences.is_empty()
                && audiences.len() <= 16
                && audiences.iter().all(|audience| valid_value(audience, 512))
                && audiences.iter().any(|audience| {
                    audience == &config.api_audience
                        || config.portal_audience.as_ref() == Some(audience)
                })
        }
    };
    let frontend_client_id = match (&claims.azp, &claims.client_id) {
        (Some(azp), Some(client_id)) if azp == client_id => azp,
        (Some(azp), None) => azp,
        (None, Some(client_id)) => client_id,
        _ => return Err(IdTokenError::InvalidClaims),
    };
    let subject_is_bounded = valid_value(&claims.sub, 255);
    let email = claims
        .provider_claims
        .get(&config.identity.email_claim)
        .and_then(serde_json::Value::as_str)
        .ok_or(IdTokenError::InvalidClaims)?;
    let email_is_bounded = !email.is_empty() && email.len() <= 320;
    let email_is_trusted = config.identity.trust_email
        || claims
            .provider_claims
            .get("email_verified")
            .and_then(serde_json::Value::as_bool)
            == Some(true);
    let display_name_is_bounded = claims.name.as_ref().is_none_or(|name| {
        let trimmed = name.trim();
        !trimmed.is_empty() && trimmed.len() <= 256 && !trimmed.chars().any(char::is_control)
    });
    let expiration_is_valid =
        claims.exp > claims.iat && claims.exp.saturating_add(CLOCK_SKEW_SECONDS) >= now;
    let issued_at_is_valid = claims.iat <= now.saturating_add(CLOCK_SKEW_SECONDS)
        && now <= claims.iat.saturating_add(config.maximum_token_age_seconds);
    let not_before_is_valid = claims
        .nbf
        .is_none_or(|not_before| not_before <= now.saturating_add(CLOCK_SKEW_SECONDS));
    if claims.iss != config.identity.issuer
        || config.identity.user_id_claim != "sub"
        || !audience_matches
        || !config.frontend_client_ids.contains(frontend_client_id)
        || !expiration_is_valid
        || !issued_at_is_valid
        || !not_before_is_valid
        || !subject_is_bounded
        || !email_is_bounded
        || !display_name_is_bounded
        || !email_is_trusted
    {
        return Err(IdTokenError::InvalidClaims);
    }
    let identity = config
        .identity
        .identity_policy
        .validate(
            &claims.iss,
            &claims.sub,
            &config.identity.organization_claim,
            email,
            true,
        )
        .map_err(|_| IdTokenError::InvalidClaims)?;
    Ok(VerifiedUserTokenClaims {
        identity,
        display_name: claims.name.map(|name| name.trim().to_owned()),
        frontend_client_id: frontend_client_id.to_owned(),
        email_linking_allowed: config.identity.trust_email,
    })
}

fn verify_user_token(
    token: &str,
    jwks: &JwkSet,
    config: &UserTokenConfig,
    now: u64,
) -> Result<VerifiedUserTokenClaims, IdTokenError> {
    if token.is_empty() || token.len() > MAX_ID_TOKEN_BYTES {
        return Err(IdTokenError::Malformed);
    }
    let header = decode_header(token).map_err(|_| IdTokenError::Malformed)?;
    if !matches!(header.alg, Algorithm::RS256 | Algorithm::ES256)
        || config
            .required_typ
            .as_deref()
            .is_some_and(|required| header.typ.as_deref() != Some(required))
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
    let key = select_user_verification_key(jwks, &kid, header.alg)?;
    let decoding_key = DecodingKey::from_jwk(key).map_err(|_| IdTokenError::InvalidKey)?;
    let mut validation = Validation::new(header.alg);
    validation.required_spec_claims.clear();
    validation.validate_exp = false;
    validation.validate_aud = false;
    let token = decode::<UserTokenClaims>(token, &decoding_key, &validation).map_err(|error| {
        if matches!(error.kind(), ErrorKind::InvalidSignature) {
            IdTokenError::InvalidSignature
        } else {
            IdTokenError::Malformed
        }
    })?;
    validate_user_token_claims(token.claims, config, now)
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
    #[serde(default)]
    authorization_endpoint: Option<String>,
    #[serde(default)]
    token_endpoint: Option<String>,
    jwks_uri: String,
    #[serde(default)]
    response_types_supported: Vec<String>,
    #[serde(default)]
    id_token_signing_alg_values_supported: Vec<String>,
    #[serde(default)]
    code_challenge_methods_supported: Vec<String>,
}

impl DiscoveryDocument {
    fn validate_issuer(self, expected_issuer: &str) -> Result<Self, BrowserAuthFailure> {
        if self.issuer != expected_issuer || validate_https_url(&self.jwks_uri).is_err() {
            return Err(BrowserAuthFailure::ProviderUnavailable);
        }
        Ok(self)
    }

    fn validate_google(self) -> Result<Self, BrowserAuthFailure> {
        if self.issuer != steward_types::GOOGLE_ORGANIZATION_ISSUER
            || self.authorization_endpoint.as_deref() != Some(GOOGLE_AUTHORIZATION_ENDPOINT)
            || self.token_endpoint.as_deref() != Some(GOOGLE_TOKEN_ENDPOINT)
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

trait OidcHttpTransport: Send + Sync + 'static {
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

struct ReqwestOidcTransport {
    client: reqwest::Client,
}

impl ReqwestOidcTransport {
    fn new() -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(5))
            .user_agent("steward-oidc/1")
            .build()
            .map_err(|_| "build bounded OIDC HTTP client".to_owned())?;
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

impl OidcHttpTransport for ReqwestOidcTransport {
    fn get<'a>(&'a self, url: &'a str, max_body: usize) -> HttpFuture<'a> {
        Box::pin(async move {
            let url = reqwest::Url::parse(url).map_err(|_| HttpFailure::InvalidResponse)?;
            if url.scheme() != "https"
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.fragment().is_some()
            {
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
    transport: Arc<dyn OidcHttpTransport>,
    keys: OidcKeyProvider,
}

impl GoogleOidcProvider {
    pub fn new(config: GoogleOidcConfig, client_secret: String) -> Result<Self, String> {
        Self::from_parts(
            config,
            client_secret,
            Arc::new(ReqwestOidcTransport::new()?),
            Arc::new(crate::browser_auth::epoch_seconds),
        )
    }

    fn from_parts(
        config: GoogleOidcConfig,
        client_secret: String,
        transport: Arc<dyn OidcHttpTransport>,
        now: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> Result<Self, String> {
        let keys = OidcKeyProvider::new(
            GOOGLE_DISCOVERY_ENDPOINT.to_owned(),
            steward_types::GOOGLE_ORGANIZATION_ISSUER.to_owned(),
            vec![Algorithm::RS256],
            Arc::clone(&transport),
            Arc::clone(&now),
        );
        Ok(Self {
            config,
            client_secret: Arc::new(GoogleClientSecret::new(client_secret)?),
            transport,
            keys,
        })
    }

    #[cfg(test)]
    fn with_transport<F>(
        config: GoogleOidcConfig,
        client_secret: String,
        transport: Arc<dyn OidcHttpTransport>,
        now: F,
    ) -> Result<Self, String>
    where
        F: Fn() -> u64 + Send + Sync + 'static,
    {
        Self::from_parts(config, client_secret, transport, Arc::new(now))
    }

    async fn discovery(&self) -> Result<DiscoveryDocument, BrowserAuthFailure> {
        self.keys.discovery().await?.validate_google()
    }
}

impl BrowserUserTokenVerifier for OidcUserTokenVerifier {
    fn verify<'a>(
        &'a self,
        token: &'a str,
    ) -> BrowserFuture<'a, Result<VerifiedUserTokenClaims, BrowserAuthFailure>> {
        Box::pin(async move {
            let jwks = self.keys.jwks_for_token(token).await?;
            verify_user_token(token, &jwks, &self.config, (self.keys.now)())
                .map_err(map_id_token_error)
        })
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
            let token_endpoint = discovery
                .token_endpoint
                .as_deref()
                .ok_or(BrowserAuthFailure::ProviderUnavailable)?;
            let response = self
                .transport
                .post_token(
                    token_endpoint,
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
            let jwks = self.keys.jwks_for_token(&token.id_token).await?;
            verify_id_token(
                &token.id_token,
                &jwks,
                self.config.client_id(),
                self.config.hosted_domain(),
                expected_nonce,
                (self.keys.now)(),
            )
            .map_err(map_id_token_error)
        })
    }
}

fn map_http_failure(_error: HttpFailure) -> BrowserAuthFailure {
    BrowserAuthFailure::ProviderUnavailable
}

fn validate_https_url(value: &str) -> Result<(), BrowserAuthFailure> {
    let url = reqwest::Url::parse(value).map_err(|_| BrowserAuthFailure::ProviderUnavailable)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || value.len() > 2_048
    {
        return Err(BrowserAuthFailure::ProviderUnavailable);
    }
    Ok(())
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

fn validate_oidc_jwks(
    jwks: &JwkSet,
    allowed_algorithms: &[Algorithm],
) -> Result<(), BrowserAuthFailure> {
    if jwks.keys.is_empty() || jwks.keys.len() > MAX_USER_TOKEN_KEYS {
        return Err(BrowserAuthFailure::ProviderUnavailable);
    }
    let mut kids = std::collections::HashSet::with_capacity(jwks.keys.len());
    let mut usable = 0_usize;
    for key in &jwks.keys {
        let Some(algorithm) = verification_key_algorithm(key, allowed_algorithms) else {
            continue;
        };
        let kid = key
            .common
            .key_id
            .as_deref()
            .filter(|kid| !kid.is_empty() && kid.len() <= 128 && kid.is_ascii())
            .ok_or(BrowserAuthFailure::ProviderUnavailable)?;
        if !kids.insert(kid)
            || select_user_verification_key(jwks, kid, algorithm).is_err()
            || DecodingKey::from_jwk(key).is_err()
        {
            return Err(BrowserAuthFailure::ProviderUnavailable);
        }
        usable = usable.saturating_add(1);
    }
    (usable > 0)
        .then_some(())
        .ok_or(BrowserAuthFailure::ProviderUnavailable)
}

fn verification_key_algorithm(key: &Jwk, allowed: &[Algorithm]) -> Option<Algorithm> {
    if !matches!(
        key.common.public_key_use,
        None | Some(PublicKeyUse::Signature)
    ) || !key.common.key_operations.as_ref().is_none_or(|operations| {
        !operations.is_empty()
            && operations
                .iter()
                .all(|operation| matches!(operation, KeyOperations::Verify))
    }) {
        return None;
    }
    let algorithm = match (&key.algorithm, key.common.key_algorithm) {
        (AlgorithmParameters::RSA(_), None | Some(KeyAlgorithm::RS256)) => Algorithm::RS256,
        (AlgorithmParameters::EllipticCurve(_), None | Some(KeyAlgorithm::ES256)) => {
            Algorithm::ES256
        }
        _ => return None,
    };
    allowed.contains(&algorithm).then_some(algorithm)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use tokio::sync::Notify;

    use jsonwebtoken::jwk::JwkSet;

    use crate::browser_auth::{
        BrowserAuthFailure, BrowserOidcProvider, BrowserUserTokenVerifier, GoogleOidcConfig,
    };
    use steward_types::OrganizationId;

    use super::{
        Audience, Cached, DiscoveryDocument, GoogleClientSecret, GoogleOidcProvider, HttpFailure,
        HttpFuture, HttpResponse, IdTokenClaims, IdTokenError,
        JWKS_REFRESH_FAILURE_BACKOFF_SECONDS, OidcHttpTransport, OidcUserTokenVerifier,
        TokenExchangeRequest, USER_ID_TOKEN_FIXTURE_HEADER, USER_ID_TOKEN_FIXTURE_PAYLOAD,
        USER_ID_TOKEN_FIXTURE_SIGNATURE, USER_TOKEN_FIXTURE_HEADER, USER_TOKEN_FIXTURE_MODULUS,
        USER_TOKEN_FIXTURE_PAYLOAD, USER_TOKEN_FIXTURE_SIGNATURE, UserTokenClaims, UserTokenConfig,
        UserTokenSettings, cache_ttl, validate_claims, validate_oidc_jwks,
        validate_user_token_claims, verify_id_token, verify_user_token,
    };

    const FIXTURE_HEADER: &str = "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCIsImtpZCI6ImZpeHR1cmUta2V5In0";
    const FIXTURE_PAYLOAD: &str = "eyJpc3MiOiJodHRwczovL2FjY291bnRzLmdvb2dsZS5jb20iLCJzdWIiOiJmaXh0dXJlLXN1YmplY3QiLCJhdWQiOiJmaXh0dXJlLWNsaWVudCIsImF6cCI6ImZpeHR1cmUtY2xpZW50IiwiZXhwIjoyMDAwLCJpYXQiOjEwMDAsIm5vbmNlIjoiZml4dHVyZS1ub25jZSIsImVtYWlsIjoiYWxpY2VAZXhhbXBsZS5jb20iLCJlbWFpbF92ZXJpZmllZCI6dHJ1ZSwiaGQiOiJleGFtcGxlLmNvbSJ9";
    const FIXTURE_SIGNATURE: &str = "zM7KyP19QiF_mYofXn7XRXMbLJaC9PNwbs5rVUmq-MU0RH752hE4JtJXwKViKp9jc5jKqqB-rhlqTNLpp3JKjPeIv_ZMwtI4-dbIfvZXkGtl2biQ27i6CA0eYotZm_KOuIO8etRseG89IdRRHjCOeV2LgBN_3WswM_Yvun9fejzbHlPOgmF149Q4gDxsmtB5hOVoFcgU8uaDVYDo-rJwebLsUUEpCA1lEAQIuARh9kuJw-xD9kVsRwAM4hc3h4puTmRNCoeSAeak5DBtlRY6Y4arwUcRt6dkA-Rr1xfgzEp3DW6rldIMAp_dAS9ZWUKz_e1hlG3X4twnEetvO_-JKg";
    const FIXTURE_MODULUS: &str = "7lwICTwFDPsqKp-jQTx5kGWbea0KNnIWYGRDqfqiK7ultoTvn4oyiUtT5il14OkSr7JTz-gFFKsIg9_Vcsby12Cyc86xbttYL8LJHSUM4OLwB_x1NzMBuijWcOuoGACqLWUi8upA2yM_7z8XyDeQONxNA880w4MJ0xQyXk6Rk-rVmU7SJJgBgDC1lAsfc5j0k4e3wo4L5zBV9_XdhqyqtOZdsrKSuVLHA-lBT-xSKWAl_pPMTIi7NMWvpiRTQ_2vu0bty1EqgitxQ164U-UZChY_ILX_ooweeOODS7l3vcQuyUQ3rbA-b7VFOQpeIE134NYnhWhlJRNN8Bc2MrzYew";
    const USER_TOKEN_EC_FIXTURE_HEADER: &str =
        "eyJhbGciOiJFUzI1NiIsImtpZCI6InVzZXItdG9rZW4tZWMtZml4dHVyZS1rZXkifQ";
    const USER_TOKEN_EC_FIXTURE_PAYLOAD: &str = "eyJpc3MiOiJodHRwczovL2lzc3Vlci5leGFtcGxlLnRlc3QiLCJzdWIiOiJmaXh0dXJlLWVjLXN1YmplY3QiLCJhdWQiOiJzdGV3YXJkLWFwaSIsImNsaWVudF9pZCI6InBvcnRhbC1jbGllbnQiLCJleHAiOjIwMDAsImlhdCI6MTAwMCwiZW1haWwiOiJhbGljZUBleGFtcGxlLmNvbSIsImVtYWlsX3ZlcmlmaWVkIjp0cnVlfQ";
    const USER_TOKEN_EC_FIXTURE_SIGNATURE: &str =
        "pU2MD53VsZNMtfJwkgop7AE3y2bonZRE9rGadYju88Y8HiTo3EZLJs6ym-jvL4LMpACNj2rOhLxf-rQZ7HXdKQ";

    fn fixture_token() -> String {
        format!("{FIXTURE_HEADER}.{FIXTURE_PAYLOAD}.{FIXTURE_SIGNATURE}")
    }

    fn fixture_user_access_token() -> String {
        format!(
            "{USER_TOKEN_FIXTURE_HEADER}.{USER_TOKEN_FIXTURE_PAYLOAD}.{USER_TOKEN_FIXTURE_SIGNATURE}"
        )
    }

    fn fixture_user_token_jwks() -> Result<JwkSet, String> {
        serde_json::from_value(serde_json::json!({
            "keys": [{
                "kty": "RSA",
                "use": "sig",
                "alg": "RS256",
                "kid": "user-token-fixture-key",
                "n": USER_TOKEN_FIXTURE_MODULUS,
                "e": "AQAB"
            }]
        }))
        .map_err(|error| format!("parse neutral user-token JWKS fixture: {error}"))
    }

    fn fixture_user_token_ec_jwks() -> Result<JwkSet, String> {
        serde_json::from_value(serde_json::json!({
            "keys": [{
                "kty": "EC",
                "use": "sig",
                "alg": "ES256",
                "kid": "user-token-ec-fixture-key",
                "crv": "P-256",
                "x": "ixeYSNkzZWYQwqvizPEdKOR95tUCx3LFMymIJ7l3mxA",
                "y": "BgijYGLVVS3p48Tha-4lY74J0RBLFOzivyonHWykTUs"
            }]
        }))
        .map_err(|error| format!("parse neutral EC user-token JWKS fixture: {error}"))
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

    fn fixture_user_token_config(
        portal_audience: Option<&str>,
        required_typ: Option<&str>,
    ) -> Result<UserTokenConfig, String> {
        UserTokenConfig::new(UserTokenSettings {
            issuer: "https://issuer.example.test".to_owned(),
            organization_claim: "example.com".to_owned(),
            organization_id: OrganizationId::parse("org_example")?,
            user_id_claim: "sub".to_owned(),
            email_claim: "email".to_owned(),
            trust_email: false,
            api_audience: "steward-api".to_owned(),
            portal_audience: portal_audience.map(str::to_owned),
            frontend_client_ids: vec!["portal-client".to_owned()],
            maximum_token_age_seconds: 300,
            required_typ: required_typ.map(str::to_owned),
        })
    }

    struct FixtureTransport {
        gets: Mutex<VecDeque<HttpResponse>>,
        posts: Mutex<VecDeque<HttpResponse>>,
        get_calls: AtomicUsize,
        blocked_get_call: Option<usize>,
        get_started: Notify,
        release_get: Notify,
    }

    impl FixtureTransport {
        fn new(gets: Vec<HttpResponse>, posts: Vec<HttpResponse>) -> Self {
            Self {
                gets: Mutex::new(gets.into()),
                posts: Mutex::new(posts.into()),
                get_calls: AtomicUsize::new(0),
                blocked_get_call: None,
                get_started: Notify::new(),
                release_get: Notify::new(),
            }
        }

        fn blocking_get(gets: Vec<HttpResponse>, blocked_get_call: usize) -> Self {
            Self {
                gets: Mutex::new(gets.into()),
                posts: Mutex::new(VecDeque::new()),
                get_calls: AtomicUsize::new(0),
                blocked_get_call: Some(blocked_get_call),
                get_started: Notify::new(),
                release_get: Notify::new(),
            }
        }
    }

    impl OidcHttpTransport for FixtureTransport {
        fn get<'a>(&'a self, _url: &'a str, _max_body: usize) -> HttpFuture<'a> {
            Box::pin(async move {
                let call = self.get_calls.fetch_add(1, Ordering::SeqCst);
                let response = self
                    .gets
                    .lock()
                    .map_err(|_| HttpFailure::Unavailable)?
                    .pop_front()
                    .ok_or(HttpFailure::Unavailable)?;
                if self.blocked_get_call == Some(call) {
                    self.get_started.notify_one();
                    self.release_get.notified().await;
                }
                Ok(response)
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

    impl OidcHttpTransport for BlockingFailureTransport {
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

    impl OidcHttpTransport for CountingFailureTransport {
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
            .validate_google()
            .map_err(|error| format!("validate discovery fixture: {error:?}"))?;
        let mut cache = provider
            .keys
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
            provider.keys.jwks(Some(7), None).await,
            Err(BrowserAuthFailure::ProviderUnavailable)
        ));
        assert_eq!(transport.calls.load(Ordering::SeqCst), 1);

        clock.store(
            1_100 + JWKS_REFRESH_FAILURE_BACKOFF_SECONDS,
            Ordering::SeqCst,
        );
        assert!(provider.keys.jwks(Some(7), None).await.is_err());
        assert_eq!(
            transport.calls.load(Ordering::SeqCst),
            2,
            "the same generation may retry only after the bounded backoff"
        );

        provider
            .keys
            .cache
            .lock()
            .map_err(|_| "lock provider generation".to_owned())?
            .jwks_generation = 8;
        assert!(provider.keys.jwks(Some(8), None).await.is_err());
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
            .keys
            .cache
            .lock()
            .map_err(|_| "expire discovery fixture".to_owned())?
            .discovery
            .as_mut()
            .ok_or_else(|| "missing discovery fixture".to_owned())?
            .expires_at = 1_100;

        let first_provider = provider.clone();
        let first = tokio::spawn(async move { first_provider.keys.jwks(Some(7), None).await });
        transport.started.notified().await;
        let second_provider = provider.clone();
        let second = tokio::spawn(async move { second_provider.keys.jwks(Some(7), None).await });
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
            .keys
            .cache
            .lock()
            .map_err(|_| "read refresh failure".to_owned())?
            .jwks_refresh_failure
            .ok_or_else(|| "discovery failure was not recorded".to_owned())?;
        assert_eq!(failure.observed_generation, 7);
        assert!(provider.keys.jwks(Some(7), None).await.is_err());
        assert_eq!(transport.calls.load(Ordering::SeqCst), 1);

        provider
            .keys
            .cache
            .lock()
            .map_err(|_| "advance JWKS generation".to_owned())?
            .jwks_generation = 8;
        assert!(provider.keys.jwks(Some(8), None).await.is_err());
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
            .keys
            .refresh_gate
            .acquire()
            .await
            .map_err(|error| format!("acquire fixture refresh gate: {error:?}"))?;
        let queued_provider = provider.clone();
        let queued = tokio::spawn(async move { queued_provider.keys.jwks(Some(7), None).await });
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

    fn fixture_user_token_claims() -> UserTokenClaims {
        UserTokenClaims {
            iss: "https://issuer.example.test".to_owned(),
            sub: "fixture-subject".to_owned(),
            aud: Audience::Single("steward-api".to_owned()),
            azp: Some("portal-client".to_owned()),
            client_id: None,
            exp: 2_000,
            iat: 1_000,
            nbf: Some(1_000),
            name: Some("Alice Example".to_owned()),
            provider_claims: serde_json::Map::from_iter([
                (
                    "email".to_owned(),
                    serde_json::Value::String("alice@example.com".to_owned()),
                ),
                ("email_verified".to_owned(), serde_json::Value::Bool(true)),
            ]),
        }
    }

    #[test]
    fn user_token_configuration_refuses_identity_token_and_non_ascii_values() -> Result<(), String>
    {
        for (api_audience, portal_audience, clients) in [
            ("portal-client", None, vec!["portal-client"]),
            ("steward-π", None, vec!["portal-client"]),
            ("steward-api", Some("portal-client"), vec!["portal-client"]),
            ("steward-api", None, vec!["portal-π"]),
        ] {
            assert!(
                UserTokenConfig::new(UserTokenSettings {
                    issuer: "https://issuer.example.test".to_owned(),
                    organization_claim: "example.com".to_owned(),
                    organization_id: OrganizationId::parse("org_example")?,
                    user_id_claim: "sub".to_owned(),
                    email_claim: "email".to_owned(),
                    trust_email: false,
                    api_audience: api_audience.to_owned(),
                    portal_audience: portal_audience.map(str::to_owned),
                    frontend_client_ids: clients.into_iter().map(str::to_owned).collect(),
                    maximum_token_age_seconds: 300,
                    required_typ: None,
                })
                .is_err()
            );
        }
        assert!(
            UserTokenConfig::new(UserTokenSettings {
                issuer: "https://issuer.example.test/π".to_owned(),
                organization_claim: "example.com".to_owned(),
                organization_id: OrganizationId::parse("org_example")?,
                user_id_claim: "sub".to_owned(),
                email_claim: "email".to_owned(),
                trust_email: false,
                api_audience: "steward-api".to_owned(),
                portal_audience: None,
                frontend_client_ids: vec!["portal-client".to_owned()],
                maximum_token_age_seconds: 300,
                required_typ: None,
            })
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn user_token_claims_refuse_wrong_issuer_audience_expiry_and_disabled_portal_audience()
    -> Result<(), String> {
        let config = fixture_user_token_config(None, None)?;
        let verified = validate_user_token_claims(fixture_user_token_claims(), &config, 1_100)
            .map_err(|error| format!("verify user token fixture: {error:?}"))?;
        assert_eq!(verified.frontend_client_id, "portal-client");

        let mut cases = Vec::new();
        let mut claims = fixture_user_token_claims();
        claims.iss = "https://other-issuer.example.test".to_owned();
        cases.push(claims);
        let mut claims = fixture_user_token_claims();
        claims.aud = Audience::Single("other-api".to_owned());
        cases.push(claims);
        let mut claims = fixture_user_token_claims();
        claims.exp = 1_039;
        cases.push(claims);
        let mut claims = fixture_user_token_claims();
        claims.iat = 799;
        cases.push(claims);
        let mut claims = fixture_user_token_claims();
        claims.aud = Audience::Single("portal-audience".to_owned());
        cases.push(claims);
        let mut claims = fixture_user_token_claims();
        claims.azp = Some("other-client".to_owned());
        cases.push(claims);
        let mut claims = fixture_user_token_claims();
        claims.nbf = Some(1_161);
        cases.push(claims);
        let mut claims = fixture_user_token_claims();
        claims.sub.clear();
        cases.push(claims);
        for claims in cases {
            assert!(matches!(
                validate_user_token_claims(claims, &config, 1_100),
                Err(IdTokenError::InvalidClaims)
            ));
        }

        let portal_config = fixture_user_token_config(Some("portal-audience"), None)?;
        let mut portal_claims = fixture_user_token_claims();
        portal_claims.aud = Audience::Single("portal-audience".to_owned());
        assert!(validate_user_token_claims(portal_claims, &portal_config, 1_100).is_ok());

        let claims_with_provider_role: UserTokenClaims =
            serde_json::from_value(serde_json::json!({
                "iss": "https://issuer.example.test",
                "sub": "fixture-subject",
                "aud": "steward-api",
                "azp": "portal-client",
                "exp": 2_000,
                "iat": 1_000,
                "nbf": 1_000,
                "email": "alice@example.com",
                "email_verified": true,
                "groups": ["admin"]
            }))
            .map_err(|error| format!("parse hostile user-token claims: {error}"))?;
        assert!(
            validate_user_token_claims(claims_with_provider_role, &config, 1_100).is_ok(),
            "provider groups are ignored because roles come only from local RBAC"
        );
        let mut skewed = fixture_user_token_claims();
        skewed.nbf = Some(1_160);
        skewed.exp = 1_040;
        assert!(
            validate_user_token_claims(skewed, &config, 1_100).is_ok(),
            "nbf and exp use the same bounded clock skew as iat"
        );
        Ok(())
    }

    #[test]
    fn user_token_email_claim_and_verification_policy_are_per_issuer() -> Result<(), String> {
        let config = |email_claim: &str, trust_email: bool| {
            UserTokenConfig::new(UserTokenSettings {
                issuer: "https://issuer.example.test".to_owned(),
                organization_claim: "example.com".to_owned(),
                organization_id: OrganizationId::parse("org_example")?,
                user_id_claim: "sub".to_owned(),
                email_claim: email_claim.to_owned(),
                trust_email,
                api_audience: "steward-api".to_owned(),
                portal_audience: None,
                frontend_client_ids: vec!["portal-client".to_owned()],
                maximum_token_age_seconds: 300,
                required_typ: None,
            })
        };
        let mut claims = fixture_user_token_claims();
        let untrusted = validate_user_token_claims(claims.clone(), &config("email", false)?, 1_100)
            .map_err(|error| format!("verify issuer requiring email_verified: {error:?}"))?;
        assert!(!untrusted.email_linking_allowed);
        claims.provider_claims.remove("email_verified");
        assert!(matches!(
            validate_user_token_claims(claims.clone(), &config("email", false)?, 1_100),
            Err(IdTokenError::InvalidClaims)
        ));
        assert!(
            validate_user_token_claims(claims.clone(), &config("email", true)?, 1_100).is_ok(),
            "an operator-trusted issuer may omit email_verified"
        );
        let email = claims
            .provider_claims
            .remove("email")
            .ok_or_else(|| "email fixture missing".to_owned())?;
        claims
            .provider_claims
            .insert("preferred_email".to_owned(), email);
        let verified = validate_user_token_claims(claims, &config("preferred_email", true)?, 1_100)
            .map_err(|error| format!("verify configured email claim: {error:?}"))?;
        assert!(verified.email_linking_allowed);
        let mut other_domain = fixture_user_token_claims();
        other_domain.provider_claims.insert(
            "email".to_owned(),
            serde_json::Value::String("alice@other.example".to_owned()),
        );
        assert!(matches!(
            validate_user_token_claims(other_domain, &config("email", true)?, 1_100),
            Err(IdTokenError::InvalidClaims)
        ));
        Ok(())
    }

    #[test]
    fn user_token_uses_the_configured_stable_user_id_claim() -> Result<(), String> {
        let mut config = fixture_user_token_config(None, None)?;
        config.identity.user_id_claim = "oid".to_owned();
        let mut claims = fixture_user_token_claims();
        claims.sub = "untrusted-default-subject".to_owned();
        claims.provider_claims.insert(
            "oid".to_owned(),
            serde_json::Value::String("fixture-user-subject".to_owned()),
        );

        let verified = validate_user_token_claims(claims, &config, 1_100)
            .map_err(|error| format!("verify configured stable user ID claim: {error:?}"))?;
        assert_eq!(verified.identity.subject(), "fixture-user-subject");

        let mut missing = fixture_user_token_claims();
        missing.sub = "untrusted-default-subject".to_owned();
        assert!(matches!(
            validate_user_token_claims(missing, &config, 1_100),
            Err(IdTokenError::InvalidClaims)
        ));
        Ok(())
    }

    #[test]
    fn mixed_jwks_ignores_non_signing_and_unsupported_keys() -> Result<(), String> {
        let jwks: JwkSet = serde_json::from_value(serde_json::json!({
            "keys": [
                {
                    "kty": "RSA",
                    "use": "enc",
                    "alg": "RSA-OAEP",
                    "kid": "encryption-key",
                    "n": USER_TOKEN_FIXTURE_MODULUS,
                    "e": "AQAB"
                },
                {
                    "kty": "RSA",
                    "use": "sig",
                    "alg": "RS256",
                    "kid": "user-token-fixture-key",
                    "n": USER_TOKEN_FIXTURE_MODULUS,
                    "e": "AQAB"
                }
            ]
        }))
        .map_err(|error| format!("parse mixed JWKS fixture: {error}"))?;
        validate_oidc_jwks(
            &jwks,
            &[
                jsonwebtoken::Algorithm::RS256,
                jsonwebtoken::Algorithm::ES256,
            ],
        )
        .map_err(|error| format!("mixed JWKS must retain its usable signing key: {error:?}"))
    }

    #[test]
    fn user_access_token_contract_refuses_a_browser_id_token() -> Result<(), String> {
        let config = fixture_user_token_config(None, None)?;
        let same_issuer_id_token = format!(
            "{USER_ID_TOKEN_FIXTURE_HEADER}.{USER_ID_TOKEN_FIXTURE_PAYLOAD}.{USER_ID_TOKEN_FIXTURE_SIGNATURE}"
        );
        assert!(matches!(
            verify_user_token(
                &same_issuer_id_token,
                &fixture_user_token_jwks()?,
                &config,
                1_100,
            ),
            Err(IdTokenError::InvalidClaims)
        ));
        assert!(
            verify_user_token(
                &fixture_user_access_token(),
                &fixture_user_token_jwks()?,
                &fixture_user_token_config(None, None)?,
                1_100,
            )
            .is_ok()
        );
        for google_issuer in [
            steward_types::GOOGLE_ORGANIZATION_ISSUER,
            "https://accounts.google.com/",
            "https://ACCOUNTS.GOOGLE.COM",
            "https://accounts.google.com:443",
        ] {
            assert!(
                UserTokenConfig::new(UserTokenSettings {
                    issuer: google_issuer.to_owned(),
                    organization_claim: "example.com".to_owned(),
                    organization_id: OrganizationId::parse("org_example")?,
                    user_id_claim: "sub".to_owned(),
                    email_claim: "email".to_owned(),
                    trust_email: false,
                    api_audience: "steward-api".to_owned(),
                    portal_audience: None,
                    frontend_client_ids: vec!["portal-client".to_owned()],
                    maximum_token_age_seconds: 300,
                    required_typ: None,
                })
                .is_err(),
                "Google browser credentials must not enter the JWT bearer verifier: {google_issuer}"
            );
        }
        Ok(())
    }

    #[test]
    fn user_access_token_allows_rs256_and_es256_and_refuses_type_and_algorithm_confusion()
    -> Result<(), String> {
        let config = fixture_user_token_config(None, None)?;
        let ec_token = format!(
            "{USER_TOKEN_EC_FIXTURE_HEADER}.{USER_TOKEN_EC_FIXTURE_PAYLOAD}.{USER_TOKEN_EC_FIXTURE_SIGNATURE}"
        );
        assert!(
            verify_user_token(&ec_token, &fixture_user_token_ec_jwks()?, &config, 1_100).is_ok()
        );
        assert!(
            verify_user_token(
                &fixture_user_access_token(),
                &fixture_user_token_jwks()?,
                &fixture_user_token_config(None, Some("at+jwt"))?,
                1_100,
            )
            .is_ok()
        );
        assert!(matches!(
            verify_user_token(
                &ec_token,
                &fixture_user_token_ec_jwks()?,
                &fixture_user_token_config(None, Some("at+jwt"))?,
                1_100,
            ),
            Err(IdTokenError::ForbiddenHeader)
        ));
        for algorithm in ["none", "HS256"] {
            let token = user_token_with_header(
                serde_json::json!({"alg":algorithm,"kid":"user-token-fixture-key"}),
            )?;
            assert!(
                verify_user_token(&token, &fixture_user_token_jwks()?, &config, 1_100).is_err(),
                "{algorithm} must never enter access-token verification"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn signed_user_token_reuses_cached_jwks_and_refuses_bad_signature_and_unknown_key()
    -> Result<(), String> {
        let transport = Arc::new(FixtureTransport::blocking_get(
            vec![
                json_response(serde_json::json!({
                    "issuer": "https://issuer.example.test",
                    "jwks_uri": "https://issuer.example.test/jwks"
                }))?,
                json_response(serde_json::json!({
                    "keys": [{
                        "kty": "RSA",
                        "use": "sig",
                        "alg": "RS256",
                        "kid": "user-token-fixture-key",
                        "n": USER_TOKEN_FIXTURE_MODULUS,
                        "e": "AQAB"
                    }]
                }))?,
                json_response(serde_json::json!({
                    "keys": [{
                        "kty": "RSA",
                        "use": "sig",
                        "alg": "RS256",
                        "kid": "user-token-fixture-key",
                        "n": USER_TOKEN_FIXTURE_MODULUS,
                        "e": "AQAB"
                    }]
                }))?,
            ],
            2,
        ));
        let verifier = OidcUserTokenVerifier::with_transport(
            fixture_user_token_config(None, None)?,
            transport.clone(),
            fixture_now,
        );
        let verified = verifier
            .verify(&fixture_user_access_token())
            .await
            .map_err(|error| format!("verify signed user token: {error:?}"))?;
        assert_eq!(verified.identity.subject(), "fixture-user-subject");

        let bad_signature = format!(
            "{USER_TOKEN_FIXTURE_HEADER}.{USER_TOKEN_FIXTURE_PAYLOAD}.A{}",
            &USER_TOKEN_FIXTURE_SIGNATURE[1..]
        );
        assert!(matches!(
            verifier.verify(&bad_signature).await,
            Err(BrowserAuthFailure::InvalidIdentity)
        ));
        let unknown_key = token_with_header(
            serde_json::json!({"alg":"RS256","typ":"at+jwt","kid":"unknown-key"}),
        )?;
        let another_unknown_key = user_token_with_header(
            serde_json::json!({"alg":"RS256","typ":"at+jwt","kid":"another-unknown-key"}),
        )?;
        let first_verifier = verifier.clone();
        let first = tokio::spawn(async move { first_verifier.verify(&unknown_key).await });
        transport.get_started.notified().await;
        let second_verifier = verifier.clone();
        let second =
            tokio::spawn(async move { second_verifier.verify(&another_unknown_key).await });
        tokio::task::yield_now().await;
        transport.release_get.notify_one();
        assert!(matches!(
            first.await,
            Ok(Err(BrowserAuthFailure::InvalidIdentity))
        ));
        assert!(matches!(
            second.await,
            Ok(Err(BrowserAuthFailure::ProviderUnavailable))
        ));
        assert_eq!(
            transport.get_calls.load(Ordering::SeqCst),
            3,
            "a concurrent distinct unknown key must observe the cooldown before the gate opens"
        );
        assert!(matches!(
            verifier.verify("opaque-token").await,
            Err(BrowserAuthFailure::InvalidIdentity)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn user_token_discovery_requires_the_exact_configured_issuer() -> Result<(), String> {
        let verifier = OidcUserTokenVerifier::with_transport(
            fixture_user_token_config(None, None)?,
            Arc::new(FixtureTransport::new(
                vec![json_response(serde_json::json!({
                    "issuer": "https://other-issuer.example.test",
                    "jwks_uri": "https://issuer.example.test/jwks"
                }))?],
                Vec::new(),
            )),
            fixture_now,
        );
        assert!(matches!(
            verifier.verify(&fixture_user_access_token()).await,
            Err(BrowserAuthFailure::ProviderUnavailable)
        ));
        Ok(())
    }

    fn token_with_header(header: serde_json::Value) -> Result<String, String> {
        let header = serde_json::to_vec(&header)
            .map_err(|error| format!("serialize hostile JOSE header: {error}"))?;
        Ok(format!(
            "{}.{FIXTURE_PAYLOAD}.{FIXTURE_SIGNATURE}",
            crate::browser_auth::base64_url(&header)
        ))
    }

    fn user_token_with_header(header: serde_json::Value) -> Result<String, String> {
        let header = serde_json::to_vec(&header)
            .map_err(|error| format!("serialize hostile user-token JOSE header: {error}"))?;
        Ok(format!(
            "{}.{USER_TOKEN_FIXTURE_PAYLOAD}.{USER_TOKEN_FIXTURE_SIGNATURE}",
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
