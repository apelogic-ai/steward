use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::hash::Hash;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use steward_store::{PgStore, StoreError};
use steward_types::{
    CanonicalUserId, Email, GOOGLE_ORGANIZATION_ISSUER, OrganizationId, OrganizationIdentity,
    OrganizationIdentityPolicy,
};
use uuid::Uuid;

const GOOGLE_AUTHORIZATION_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const CALLBACK_PATH: &str = "/admin/auth/callback";
const FLOW_TTL_SECONDS: u64 = 300;
const SESSION_TTL_SECONDS: u64 = 3_600;
const MAX_PENDING_AUTHORIZATIONS: usize = 256;
const MAX_BROWSER_SESSIONS: usize = 4_096;
const BROWSER_SESSION_API_VERSION: &str = "steward.browser-session/v1";
const SIGN_IN_HTML: &str = include_str!("../assets/admin/sign-in.html");
const SESSION_READY_HTML: &str = include_str!("../assets/admin/session-ready.html");

pub(crate) type BrowserFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoogleOidcConfig {
    client_id: String,
    browser_origin: String,
    callback_uri: String,
    hosted_domain: String,
    organization_id: OrganizationId,
}

impl GoogleOidcConfig {
    pub fn new(
        client_id: impl Into<String>,
        browser_origin: impl Into<String>,
        callback_uri: impl Into<String>,
        hosted_domain: impl Into<String>,
        organization_id: OrganizationId,
    ) -> Result<Self, String> {
        let client_id = client_id.into();
        let browser_origin = normalize_https_origin(&browser_origin.into())?;
        let callback_uri = callback_uri.into();
        let hosted_domain = hosted_domain.into();
        if client_id.trim().is_empty() {
            return Err("Google OIDC client ID must be configured".to_owned());
        }
        if callback_uri != format!("{browser_origin}{CALLBACK_PATH}") {
            return Err(
                "Google OIDC callback must be the exact same-origin callback route".to_owned(),
            );
        }
        OrganizationIdentityPolicy::new(
            GOOGLE_ORGANIZATION_ISSUER,
            hosted_domain.clone(),
            organization_id.clone(),
        )?;
        Ok(Self {
            client_id,
            browser_origin,
            callback_uri,
            hosted_domain,
            organization_id,
        })
    }

    fn authorization_url(&self, flow: &PendingAuthorization) -> String {
        let pairs = [
            ("client_id", self.client_id.as_str()),
            ("redirect_uri", self.callback_uri.as_str()),
            ("response_type", "code"),
            ("scope", "openid email profile"),
            ("state", flow.state.as_str()),
            ("nonce", flow.nonce.as_str()),
            ("code_challenge", flow.code_challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("hd", self.hosted_domain.as_str()),
        ];
        let query = pairs
            .into_iter()
            .map(|(name, value)| format!("{name}={}", percent_encode(value)))
            .collect::<Vec<_>>()
            .join("&");
        format!("{GOOGLE_AUTHORIZATION_ENDPOINT}?{query}")
    }

    fn identity_policy(&self) -> Result<OrganizationIdentityPolicy, String> {
        OrganizationIdentityPolicy::new(
            GOOGLE_ORGANIZATION_ISSUER,
            self.hosted_domain.clone(),
            self.organization_id.clone(),
        )
    }

    pub(crate) fn client_id(&self) -> &str {
        &self.client_id
    }

    pub(crate) fn callback_uri(&self) -> &str {
        &self.callback_uri
    }

    pub(crate) fn hosted_domain(&self) -> &str {
        &self.hosted_domain
    }

    pub(crate) fn authorization_request_url(&self, flow: &BrowserAuthorizationRequest) -> String {
        self.authorization_url(&flow.pending)
    }
}

fn normalize_https_origin(value: &str) -> Result<String, String> {
    if value.trim() != value || !value.starts_with("https://") {
        return Err("browser origin must be one exact HTTPS origin".to_owned());
    }
    let authority_and_path = &value["https://".len()..];
    if authority_and_path
        .find('/')
        .is_some_and(|index| &authority_and_path[index..] != "/")
    {
        return Err("browser origin must be one exact HTTPS origin".to_owned());
    }
    let url = reqwest::Url::parse(value)
        .map_err(|_| "browser origin must be one exact HTTPS origin".to_owned())?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("browser origin must be one exact HTTPS origin".to_owned());
    }
    let origin = url.origin().ascii_serialization();
    if origin == "null" {
        return Err("browser origin must be one exact HTTPS origin".to_owned());
    }
    Ok(origin)
}

#[derive(Clone)]
pub struct GoogleAuthorizationOnlyProvider {
    config: GoogleOidcConfig,
}

impl GoogleAuthorizationOnlyProvider {
    pub fn new(config: GoogleOidcConfig) -> Self {
        Self { config }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum BrowserRole {
    User,
    Admin,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrowserPrincipal {
    pub canonical_user_id: CanonicalUserId,
    pub display_email: Email,
    pub role: BrowserRole,
}

#[derive(Clone, Eq, Hash, PartialEq)]
pub struct BrowserSessionBinding(String);

#[cfg(test)]
impl BrowserSessionBinding {
    pub(crate) fn from_test_value(value: &str) -> Self {
        Self(value.to_owned())
    }
}

#[derive(Clone)]
pub struct BrowserSessionContext {
    pub principal: BrowserPrincipal,
    pub binding: BrowserSessionBinding,
}

#[derive(Clone)]
pub struct BrowserAdminAuthority {
    principal: BrowserPrincipal,
    binding: BrowserSessionBinding,
}

impl BrowserAdminAuthority {
    pub fn principal(&self) -> &BrowserPrincipal {
        &self.principal
    }

    pub fn binding(&self) -> &BrowserSessionBinding {
        &self.binding
    }
}

#[derive(Clone, Copy)]
pub struct BrowserMutationProof(());

#[cfg(test)]
impl BrowserMutationProof {
    pub(crate) fn for_test() -> Self {
        Self(())
    }
}

pub struct VerifiedOrganizationClaims {
    pub(crate) issuer: String,
    pub(crate) subject: String,
    pub(crate) hosted_domain: String,
    pub(crate) email: String,
    pub(crate) email_verified: bool,
    pub(crate) nonce: String,
}

pub trait BrowserOidcProvider: Send + Sync + 'static {
    fn authorization_url(
        &self,
        flow: &BrowserAuthorizationRequest,
    ) -> Result<String, BrowserAuthFailure>;

    fn exchange_code<'a>(
        &'a self,
        code: &'a str,
        pkce_verifier: &'a str,
        callback_uri: &'a str,
        expected_nonce: &'a str,
    ) -> BrowserFuture<'a, Result<VerifiedOrganizationClaims, BrowserAuthFailure>>;

    fn local_authorize<'a>(
        &'a self,
        _state: &'a str,
        _nonce: &'a str,
    ) -> BrowserFuture<'a, Result<String, BrowserAuthFailure>> {
        Box::pin(async { Err(BrowserAuthFailure::ProviderUnavailable) })
    }
}

impl BrowserOidcProvider for GoogleAuthorizationOnlyProvider {
    fn authorization_url(
        &self,
        flow: &BrowserAuthorizationRequest,
    ) -> Result<String, BrowserAuthFailure> {
        Ok(self.config.authorization_url(&flow.pending))
    }

    fn exchange_code<'a>(
        &'a self,
        _code: &'a str,
        _pkce_verifier: &'a str,
        _callback_uri: &'a str,
        _expected_nonce: &'a str,
    ) -> BrowserFuture<'a, Result<VerifiedOrganizationClaims, BrowserAuthFailure>> {
        // Deliberately fail closed until a reviewed OIDC/JWKS verifier is wired. Redirecting to
        // Google without verifying its signed ID token would be worse than having no login.
        Box::pin(async { Err(BrowserAuthFailure::ProviderUnavailable) })
    }
}

pub trait BrowserIdentityResolver: Send + Sync + 'static {
    fn resolve_or_register<'a>(
        &'a self,
        identity: &'a OrganizationIdentity,
    ) -> BrowserFuture<'a, Result<BrowserPrincipal, BrowserAuthFailure>>;
}

#[derive(Clone)]
pub struct PgBrowserIdentityResolver {
    store: PgStore,
    admin_user_ids: HashSet<CanonicalUserId>,
}

impl PgBrowserIdentityResolver {
    pub fn new(store: PgStore, admin_user_ids: HashSet<CanonicalUserId>) -> Self {
        Self {
            store,
            admin_user_ids,
        }
    }
}

impl BrowserIdentityResolver for PgBrowserIdentityResolver {
    fn resolve_or_register<'a>(
        &'a self,
        identity: &'a OrganizationIdentity,
    ) -> BrowserFuture<'a, Result<BrowserPrincipal, BrowserAuthFailure>> {
        Box::pin(async move {
            let principal = match self.store.resolve_canonical_identity(identity).await {
                Ok(principal) => principal,
                Err(StoreError::CanonicalIdentityNotFound) => self
                    .store
                    .register_canonical_identity(identity, "browser-oidc")
                    .await
                    .map_err(map_store_error)?,
                Err(error) => return Err(map_store_error(error)),
            };
            let role = if self.admin_user_ids.contains(&principal.user_id) {
                BrowserRole::Admin
            } else {
                BrowserRole::User
            };
            Ok(BrowserPrincipal {
                canonical_user_id: principal.user_id,
                display_email: principal.display_email,
                role,
            })
        })
    }
}

fn map_store_error(error: StoreError) -> BrowserAuthFailure {
    match error {
        StoreError::Database(_) => BrowserAuthFailure::IdentityUnavailable,
        _ => BrowserAuthFailure::InvalidIdentity,
    }
}

#[derive(Clone)]
pub struct BrowserAuthorizationRequest {
    pending: PendingAuthorization,
}

impl BrowserAuthorizationRequest {
    pub fn state(&self) -> &str {
        &self.pending.state
    }

    pub fn nonce(&self) -> &str {
        &self.pending.nonce
    }

    pub fn code_challenge(&self) -> &str {
        &self.pending.code_challenge
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserAuthFailure {
    InvalidRequest,
    InvalidFlow,
    InvalidIdentity,
    InvalidSession,
    InvalidMutation,
    InsufficientAuthority,
    ProviderUnavailable,
    IdentityUnavailable,
    SessionUnavailable,
}

impl BrowserAuthFailure {
    fn into_response(self) -> Response {
        let status = match self {
            Self::InvalidRequest | Self::InvalidFlow | Self::InvalidIdentity => {
                StatusCode::BAD_REQUEST
            }
            Self::InvalidSession => StatusCode::UNAUTHORIZED,
            Self::InvalidMutation | Self::InsufficientAuthority => StatusCode::FORBIDDEN,
            Self::ProviderUnavailable | Self::IdentityUnavailable | Self::SessionUnavailable => {
                StatusCode::SERVICE_UNAVAILABLE
            }
        };
        (
            status,
            Json(serde_json::json!({ "error": "browser authentication failed" })),
        )
            .into_response()
    }
}

#[derive(Clone)]
struct BrowserAuthConfig {
    browser_origin: String,
    callback_uri: String,
    policy: OrganizationIdentityPolicy,
    secure_cookies: bool,
    session_cookie: &'static str,
    flow_cookie: &'static str,
}

#[derive(Clone)]
pub struct BrowserAuthService {
    provider: Arc<dyn BrowserOidcProvider>,
    identities: Arc<dyn BrowserIdentityResolver>,
    registry: BrowserSessionRegistry,
    config: BrowserAuthConfig,
}

impl BrowserAuthService {
    pub fn google(
        config: GoogleOidcConfig,
        provider: Arc<dyn BrowserOidcProvider>,
        identities: Arc<dyn BrowserIdentityResolver>,
    ) -> Result<Self, String> {
        Ok(Self {
            provider,
            identities,
            registry: BrowserSessionRegistry::default(),
            config: BrowserAuthConfig {
                browser_origin: config.browser_origin.clone(),
                callback_uri: config.callback_uri.clone(),
                policy: config.identity_policy()?,
                secure_cookies: true,
                session_cookie: "__Host-steward-session",
                flow_cookie: "__Secure-steward-oidc-flow",
            },
        })
    }

    #[cfg(any(test, feature = "admin-demo"))]
    pub fn local_fake(
        origin: &str,
        provider: Arc<dyn BrowserOidcProvider>,
        identities: Arc<dyn BrowserIdentityResolver>,
    ) -> Result<Self, String> {
        if !origin.strip_prefix("http://").is_some_and(|authority| {
            authority.starts_with("127.0.0.1:") || authority.starts_with("[::1]:")
        }) {
            return Err(
                "local fake OIDC origin must be an explicit loopback HTTP origin".to_owned(),
            );
        }
        Ok(Self {
            provider,
            identities,
            registry: BrowserSessionRegistry::default(),
            config: BrowserAuthConfig {
                browser_origin: origin.to_owned(),
                callback_uri: format!("{origin}{CALLBACK_PATH}"),
                policy: OrganizationIdentityPolicy::new(
                    GOOGLE_ORGANIZATION_ISSUER,
                    "example.com",
                    OrganizationId::parse("org_example")?,
                )?,
                secure_cookies: false,
                session_cookie: "steward-local-session",
                flow_cookie: "steward-local-oidc-flow",
            },
        })
    }
}

#[cfg(any(test, feature = "admin-demo"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalFakeIdentity {
    User,
    Admin,
    WrongTenant,
}

#[cfg(any(test, feature = "admin-demo"))]
#[derive(Clone)]
pub struct LocalFakeOidcProvider {
    identity: LocalFakeIdentity,
    codes: Arc<Mutex<HashMap<String, VerifiedOrganizationClaims>>>,
}

#[cfg(any(test, feature = "admin-demo"))]
impl LocalFakeOidcProvider {
    pub fn new(identity: LocalFakeIdentity) -> Self {
        Self {
            identity,
            codes: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[cfg(any(test, feature = "admin-demo"))]
impl BrowserOidcProvider for LocalFakeOidcProvider {
    fn authorization_url(
        &self,
        flow: &BrowserAuthorizationRequest,
    ) -> Result<String, BrowserAuthFailure> {
        Ok(format!(
            "/admin/auth/fake/authorize?state={}&nonce={}",
            percent_encode(flow.state()),
            percent_encode(flow.nonce())
        ))
    }

    fn exchange_code<'a>(
        &'a self,
        code: &'a str,
        pkce_verifier: &'a str,
        callback_uri: &'a str,
        expected_nonce: &'a str,
    ) -> BrowserFuture<'a, Result<VerifiedOrganizationClaims, BrowserAuthFailure>> {
        Box::pin(async move {
            if pkce_verifier.is_empty() || !callback_uri.ends_with(CALLBACK_PATH) {
                return Err(BrowserAuthFailure::InvalidRequest);
            }
            let claims = self
                .codes
                .lock()
                .map_err(|_| BrowserAuthFailure::ProviderUnavailable)?
                .remove(code)
                .ok_or(BrowserAuthFailure::InvalidIdentity)?;
            if !secret_eq(&claims.nonce, expected_nonce) {
                return Err(BrowserAuthFailure::InvalidIdentity);
            }
            Ok(claims)
        })
    }

    fn local_authorize<'a>(
        &'a self,
        state: &'a str,
        nonce: &'a str,
    ) -> BrowserFuture<'a, Result<String, BrowserAuthFailure>> {
        Box::pin(async move {
            if state.is_empty() || nonce.is_empty() {
                return Err(BrowserAuthFailure::InvalidRequest);
            }
            let code = random_secret();
            let (subject, email, hosted_domain) = match self.identity {
                LocalFakeIdentity::User => ("fake-user", "alice@example.com", "example.com"),
                LocalFakeIdentity::Admin => ("fake-admin", "bob@example.com", "example.com"),
                LocalFakeIdentity::WrongTenant => {
                    ("fake-wrong-tenant", "alice@example.org", "example.org")
                }
            };
            self.codes
                .lock()
                .map_err(|_| BrowserAuthFailure::ProviderUnavailable)?
                .insert(
                    code.clone(),
                    VerifiedOrganizationClaims {
                        issuer: GOOGLE_ORGANIZATION_ISSUER.to_owned(),
                        subject: subject.to_owned(),
                        hosted_domain: hosted_domain.to_owned(),
                        email: email.to_owned(),
                        email_verified: true,
                        nonce: nonce.to_owned(),
                    },
                );
            Ok(format!(
                "{CALLBACK_PATH}?code={}&state={}",
                percent_encode(&code),
                percent_encode(state)
            ))
        })
    }
}

#[cfg(any(test, feature = "admin-demo"))]
#[derive(Clone, Copy)]
pub struct LocalFakeIdentityResolver;

#[cfg(any(test, feature = "admin-demo"))]
impl BrowserIdentityResolver for LocalFakeIdentityResolver {
    fn resolve_or_register<'a>(
        &'a self,
        identity: &'a OrganizationIdentity,
    ) -> BrowserFuture<'a, Result<BrowserPrincipal, BrowserAuthFailure>> {
        Box::pin(async move {
            let (user_id, role) = match identity.subject() {
                "fake-user" => ("usr_0123456789abcdef0123456789abcdef", BrowserRole::User),
                "fake-admin" => ("usr_abcdef0123456789abcdef0123456789", BrowserRole::Admin),
                _ => return Err(BrowserAuthFailure::InvalidIdentity),
            };
            Ok(BrowserPrincipal {
                canonical_user_id: CanonicalUserId::parse(user_id)
                    .map_err(|_| BrowserAuthFailure::IdentityUnavailable)?,
                display_email: identity.verified_email().clone(),
                role,
            })
        })
    }
}

#[cfg(any(test, feature = "admin-demo"))]
pub fn local_fake_browser_auth_service(
    origin: &str,
    identity: LocalFakeIdentity,
) -> Result<BrowserAuthService, String> {
    BrowserAuthService::local_fake(
        origin,
        Arc::new(LocalFakeOidcProvider::new(identity)),
        Arc::new(LocalFakeIdentityResolver),
    )
}

pub fn browser_auth_router(service: BrowserAuthService) -> Router {
    Router::new()
        .route("/admin/sign-in", get(sign_in))
        .route("/admin/session-ready", get(session_ready))
        .route("/admin/auth/login", get(login))
        .route("/admin/auth/callback", get(callback))
        .route("/admin/auth/logout", post(logout))
        .route("/admin/api/v1/session", get(session))
        .route("/admin/auth/fake/authorize", get(local_fake_authorize))
        .merge(crate::admin_ui::asset_router())
        .route_layer(middleware::from_fn(
            crate::admin_ui::add_browser_security_headers,
        ))
        .with_state(service)
}

pub fn protect_browser_routes(routes: Router, service: BrowserAuthService) -> Router {
    routes.route_layer(middleware::from_fn_with_state(
        service,
        authenticate_browser_session,
    ))
}

pub fn protect_browser_admin_routes(routes: Router, service: BrowserAuthService) -> Router {
    routes.route_layer(middleware::from_fn_with_state(
        service,
        authenticate_browser_admin,
    ))
}

async fn sign_in() -> Html<&'static str> {
    Html(SIGN_IN_HTML)
}

async fn session_ready(State(service): State<BrowserAuthService>, headers: HeaderMap) -> Response {
    match resolve_session(&service, &headers) {
        Ok(_) => Html(SESSION_READY_HTML).into_response(),
        Err(_) => Redirect::to("/admin/sign-in").into_response(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LoginQuery {
    #[serde(default = "default_return_to")]
    return_to: String,
}

fn default_return_to() -> String {
    "/admin/connections".to_owned()
}

async fn login(
    State(service): State<BrowserAuthService>,
    Query(query): Query<LoginQuery>,
) -> Response {
    let flow = match service.registry.begin(&query.return_to, epoch_seconds()) {
        Ok(flow) => flow,
        Err(error) => return map_registry_error(error).into_response(),
    };
    let authorization = BrowserAuthorizationRequest {
        pending: flow.clone(),
    };
    let location = match service.provider.authorization_url(&authorization) {
        Ok(location) => location,
        Err(error) => return error.into_response(),
    };
    response_with_cookie(
        StatusCode::SEE_OTHER,
        Some(&location),
        &flow_cookie(&service.config, &flow.flow_id),
        Body::empty(),
    )
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: String,
    state: String,
    iss: Option<String>,
}

async fn callback(
    State(service): State<BrowserAuthService>,
    query: Result<Query<CallbackQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return BrowserAuthFailure::InvalidRequest.into_response(),
    };
    if query
        .iss
        .as_deref()
        .is_some_and(|issuer| issuer != GOOGLE_ORGANIZATION_ISSUER)
    {
        return BrowserAuthFailure::InvalidIdentity.into_response();
    }
    let Some(flow_id) = cookie_value(&headers, service.config.flow_cookie) else {
        return BrowserAuthFailure::InvalidFlow.into_response();
    };
    let flow = match service
        .registry
        .consume_flow(&flow_id, &query.state, epoch_seconds())
    {
        Ok(flow) => flow,
        Err(error) => return map_registry_error(error).into_response(),
    };
    let claims = match service
        .provider
        .exchange_code(
            &query.code,
            &flow.pkce_verifier,
            &service.config.callback_uri,
            &flow.nonce,
        )
        .await
    {
        Ok(claims) => claims,
        Err(error) => return error.into_response(),
    };
    if !secret_eq(&claims.nonce, &flow.nonce) {
        return BrowserAuthFailure::InvalidIdentity.into_response();
    }
    let identity = match service.config.policy.validate(
        &claims.issuer,
        &claims.subject,
        &claims.hosted_domain,
        &claims.email,
        claims.email_verified,
    ) {
        Ok(identity) => identity,
        Err(_) => return BrowserAuthFailure::InvalidIdentity.into_response(),
    };
    let principal = match service.identities.resolve_or_register(&identity).await {
        Ok(principal) => principal,
        Err(error) => return error.into_response(),
    };
    if let Some(previous) = cookie_value(&headers, service.config.session_cookie)
        && let Err(error) = service.registry.revoke(&previous)
    {
        return map_registry_error(error).into_response();
    }
    let session = match service.registry.issue(principal, epoch_seconds()) {
        Ok(session) => session,
        Err(error) => return map_registry_error(error).into_response(),
    };
    let mut response = response_with_cookie(
        StatusCode::SEE_OTHER,
        Some(&flow.return_to),
        &session_cookie(&service.config, &session.token),
        Body::empty(),
    );
    if let Ok(expired) = HeaderValue::from_str(&expire_cookie(
        service.config.flow_cookie,
        service.config.secure_cookies,
        "/admin/auth",
    )) {
        response.headers_mut().append(header::SET_COOKIE, expired);
    }
    response
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionPrincipalResponse<'a> {
    user_id: &'a CanonicalUserId,
    display_email: &'a Email,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionResponse<'a> {
    api_version: &'static str,
    principal: SessionPrincipalResponse<'a>,
    role: BrowserRole,
    surfaces: &'static [&'static str],
    csrf: &'a str,
}

async fn session(State(service): State<BrowserAuthService>, headers: HeaderMap) -> Response {
    let session = match resolve_session(&service, &headers) {
        Ok(session) => session,
        Err(error) => return error.into_response(),
    };
    let surfaces = match session.principal.role {
        BrowserRole::User => &["connections", "envelopeRequests", "agentRuns"][..],
        BrowserRole::Admin => &[
            "connections",
            "envelopeRequests",
            "agentRuns",
            "envelopeTemplates",
            "approvals",
            "fleet",
        ][..],
    };
    Json(SessionResponse {
        api_version: BROWSER_SESSION_API_VERSION,
        principal: SessionPrincipalResponse {
            user_id: &session.principal.canonical_user_id,
            display_email: &session.principal.display_email,
        },
        role: session.principal.role,
        surfaces,
        csrf: &session.csrf,
    })
    .into_response()
}

async fn logout(
    State(service): State<BrowserAuthService>,
    headers: HeaderMap,
    method: Method,
) -> Response {
    let session = match resolve_session(&service, &headers) {
        Ok(session) => session,
        Err(error) => return error.into_response(),
    };
    if !valid_mutation(&service.config, &headers, &method, &session.csrf) {
        return BrowserAuthFailure::InvalidMutation.into_response();
    }
    if let Err(error) = service.registry.revoke(&session.token) {
        return map_registry_error(error).into_response();
    }
    response_with_cookie(
        StatusCode::NO_CONTENT,
        None,
        &expire_cookie(
            service.config.session_cookie,
            service.config.secure_cookies,
            "/",
        ),
        Body::empty(),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalAuthorizeQuery {
    state: String,
    nonce: String,
}

async fn local_fake_authorize(
    State(service): State<BrowserAuthService>,
    Query(query): Query<LocalAuthorizeQuery>,
) -> Response {
    if service.config.secure_cookies {
        return StatusCode::NOT_FOUND.into_response();
    }
    match service
        .provider
        .local_authorize(&query.state, &query.nonce)
        .await
    {
        Ok(location) => Redirect::to(&location).into_response(),
        Err(error) => error.into_response(),
    }
}

pub async fn authenticate_browser_session(
    State(service): State<BrowserAuthService>,
    request: Request,
    next: Next,
) -> Response {
    authenticate_browser_request(service, request, next, false).await
}

pub async fn authenticate_browser_admin(
    State(service): State<BrowserAuthService>,
    request: Request,
    next: Next,
) -> Response {
    authenticate_browser_request(service, request, next, true).await
}

async fn authenticate_browser_request(
    service: BrowserAuthService,
    mut request: Request,
    next: Next,
    require_admin: bool,
) -> Response {
    let session = match resolve_session(&service, request.headers()) {
        Ok(session) => session,
        Err(error) => return error.into_response(),
    };
    if require_admin && session.principal.role != BrowserRole::Admin {
        return BrowserAuthFailure::InsufficientAuthority.into_response();
    }
    let is_mutation = matches!(
        *request.method(),
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    );
    if is_mutation {
        if !valid_mutation(
            &service.config,
            request.headers(),
            request.method(),
            &session.csrf,
        ) {
            return BrowserAuthFailure::InvalidMutation.into_response();
        }
        request.extensions_mut().insert(BrowserMutationProof(()));
    }
    let context = BrowserSessionContext {
        principal: session.principal,
        binding: BrowserSessionBinding(session.token),
    };
    if require_admin {
        request.extensions_mut().insert(BrowserAdminAuthority {
            principal: context.principal.clone(),
            binding: context.binding.clone(),
        });
    }
    request.extensions_mut().insert(context);
    next.run(request).await
}

fn valid_mutation(
    config: &BrowserAuthConfig,
    headers: &HeaderMap,
    method: &Method,
    csrf: &str,
) -> bool {
    if !matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    ) {
        return true;
    }
    headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        == Some(config.browser_origin.as_str())
        && headers
            .get("sec-fetch-site")
            .and_then(|value| value.to_str().ok())
            == Some("same-origin")
        && headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
        && headers
            .get("x-steward-csrf")
            .and_then(|value| value.to_str().ok())
            == Some(csrf)
}

fn resolve_session(
    service: &BrowserAuthService,
    headers: &HeaderMap,
) -> Result<BrowserSession, BrowserAuthFailure> {
    let token = cookie_value(headers, service.config.session_cookie)
        .ok_or(BrowserAuthFailure::InvalidSession)?;
    service
        .registry
        .resolve(&token, epoch_seconds())
        .map_err(map_registry_error)
}

fn map_registry_error(error: BrowserAuthError) -> BrowserAuthFailure {
    match error {
        BrowserAuthError::InvalidFlow
        | BrowserAuthError::ExpiredFlow
        | BrowserAuthError::InvalidRedirect => BrowserAuthFailure::InvalidFlow,
        BrowserAuthError::InvalidSession | BrowserAuthError::ExpiredSession => {
            BrowserAuthFailure::InvalidSession
        }
        BrowserAuthError::CapacityExceeded | BrowserAuthError::StoreUnavailable => {
            BrowserAuthFailure::SessionUnavailable
        }
    }
}

fn response_with_cookie(
    status: StatusCode,
    location: Option<&str>,
    cookie: &str,
    body: Body,
) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    if let Ok(value) = HeaderValue::from_str(cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    } else {
        *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    }
    if let Some(location) = location {
        if let Ok(value) = HeaderValue::from_str(location) {
            response.headers_mut().insert(header::LOCATION, value);
        } else {
            *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
        }
    }
    response
}

fn flow_cookie(config: &BrowserAuthConfig, value: &str) -> String {
    cookie(
        config.flow_cookie,
        value,
        config.secure_cookies,
        "/admin/auth",
        FLOW_TTL_SECONDS,
    )
}

fn session_cookie(config: &BrowserAuthConfig, value: &str) -> String {
    cookie(
        config.session_cookie,
        value,
        config.secure_cookies,
        "/",
        SESSION_TTL_SECONDS,
    )
}

fn cookie(name: &str, value: &str, secure: bool, path: &str, ttl: u64) -> String {
    format!(
        "{name}={value}; Path={path}; HttpOnly; SameSite=Lax; Max-Age={ttl}{}",
        if secure { "; Secure" } else { "" }
    )
}

fn expire_cookie(name: &str, secure: bool, path: &str) -> String {
    format!(
        "{name}=; Path={path}; HttpOnly; SameSite=Lax; Max-Age=0{}",
        if secure { "; Secure" } else { "" }
    )
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find_map(|(candidate, value)| (candidate == name).then(|| value.to_owned()))
        .filter(|value| !value.is_empty())
}

pub(crate) fn epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingAuthorization {
    flow_id: String,
    state: String,
    nonce: String,
    pkce_verifier: String,
    code_challenge: String,
    return_to: String,
    expires_at: u64,
}

impl PendingAuthorization {
    fn new(return_to: &str, now: u64) -> Result<Self, BrowserAuthError> {
        if !matches!(return_to, "/admin/connections" | "/admin/session-ready") {
            return Err(BrowserAuthError::InvalidRedirect);
        }
        let pkce_verifier = random_secret();
        Ok(Self {
            flow_id: random_secret(),
            state: random_secret(),
            nonce: random_secret(),
            code_challenge: base64_url(&Sha256::digest(pkce_verifier.as_bytes())),
            pkce_verifier,
            return_to: return_to.to_owned(),
            expires_at: now.saturating_add(FLOW_TTL_SECONDS),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct BrowserSession {
    token: String,
    csrf: String,
    principal: BrowserPrincipal,
    expires_at: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BrowserAuthError {
    InvalidFlow,
    ExpiredFlow,
    InvalidRedirect,
    InvalidSession,
    ExpiredSession,
    CapacityExceeded,
    StoreUnavailable,
}

#[derive(Default)]
struct BrowserSessionState {
    pending: HashMap<String, PendingAuthorization>,
    sessions: HashMap<String, BrowserSession>,
}

#[derive(Clone, Default)]
struct BrowserSessionRegistry {
    state: Arc<Mutex<BrowserSessionState>>,
}

impl BrowserSessionRegistry {
    fn begin(&self, return_to: &str, now: u64) -> Result<PendingAuthorization, BrowserAuthError> {
        let flow = PendingAuthorization::new(return_to, now)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| BrowserAuthError::StoreUnavailable)?;
        state.pending.retain(|_, flow| flow.expires_at > now);
        if state.pending.len() >= MAX_PENDING_AUTHORIZATIONS {
            return Err(BrowserAuthError::CapacityExceeded);
        }
        state.pending.insert(flow.flow_id.clone(), flow.clone());
        Ok(flow)
    }

    fn consume_flow(
        &self,
        flow_id: &str,
        state: &str,
        now: u64,
    ) -> Result<PendingAuthorization, BrowserAuthError> {
        let flow = self
            .state
            .lock()
            .map_err(|_| BrowserAuthError::StoreUnavailable)?
            .pending
            .remove(flow_id)
            .ok_or(BrowserAuthError::InvalidFlow)?;
        if flow.expires_at <= now {
            return Err(BrowserAuthError::ExpiredFlow);
        }
        if !secret_eq(&flow.state, state) {
            return Err(BrowserAuthError::InvalidFlow);
        }
        Ok(flow)
    }

    fn issue(
        &self,
        principal: BrowserPrincipal,
        now: u64,
    ) -> Result<BrowserSession, BrowserAuthError> {
        let session = BrowserSession {
            token: random_secret(),
            csrf: random_secret(),
            principal,
            expires_at: now.saturating_add(SESSION_TTL_SECONDS),
        };
        let mut state = self
            .state
            .lock()
            .map_err(|_| BrowserAuthError::StoreUnavailable)?;
        state.sessions.retain(|_, session| session.expires_at > now);
        if state.sessions.len() >= MAX_BROWSER_SESSIONS {
            return Err(BrowserAuthError::CapacityExceeded);
        }
        state
            .sessions
            .insert(session.token.clone(), session.clone());
        Ok(session)
    }

    fn resolve(&self, token: &str, now: u64) -> Result<BrowserSession, BrowserAuthError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| BrowserAuthError::StoreUnavailable)?;
        let session = state
            .sessions
            .get(token)
            .cloned()
            .ok_or(BrowserAuthError::InvalidSession)?;
        if session.expires_at <= now {
            state.sessions.remove(token);
            return Err(BrowserAuthError::ExpiredSession);
        }
        Ok(session)
    }

    fn revoke(&self, token: &str) -> Result<(), BrowserAuthError> {
        self.state
            .lock()
            .map_err(|_| BrowserAuthError::StoreUnavailable)?
            .sessions
            .remove(token);
        Ok(())
    }
}

fn random_secret() -> String {
    let first = Uuid::new_v4().simple().to_string();
    let second = Uuid::new_v4().simple().to_string();
    format!("{first}{second}")
}

pub(crate) fn secret_eq(left: &str, right: &str) -> bool {
    let left = Sha256::digest(left.as_bytes());
    let right = Sha256::digest(right.as_bytes());
    left.iter()
        .zip(right.iter())
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

pub(crate) fn base64_url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0] as u32;
        let second = chunk.get(1).copied().unwrap_or_default() as u32;
        let third = chunk.get(2).copied().unwrap_or_default() as u32;
        let value = (first << 16) | (second << 8) | third;
        encoded.push(ALPHABET[((value >> 18) & 63) as usize] as char);
        encoded.push(ALPHABET[((value >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            encoded.push(ALPHABET[((value >> 6) & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            encoded.push(ALPHABET[(value & 63) as usize] as char);
        }
    }
    encoded
}

fn percent_encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use axum::routing::post;
    use axum::{Extension, Json, Router};
    use steward_types::{CanonicalUserId, Email, OrganizationId};
    use tower::ServiceExt;

    use super::{
        BrowserAdminAuthority, BrowserAuthError, BrowserAuthFailure, BrowserMutationProof,
        BrowserOidcProvider, BrowserPrincipal, BrowserRole, BrowserSessionContext,
        BrowserSessionRegistry, GoogleOidcConfig, LocalFakeIdentity, LocalFakeOidcProvider,
        MAX_BROWSER_SESSIONS, MAX_PENDING_AUTHORIZATIONS, PendingAuthorization,
        browser_auth_router, local_fake_browser_auth_service, protect_browser_admin_routes,
        protect_browser_routes,
    };

    #[test]
    fn sign_in_journey_lands_on_connections_instead_of_the_session_fixture() {
        assert!(
            super::SIGN_IN_HTML.contains("returnTo=%2Fadmin%2Fconnections"),
            "the user-bound credential journey must continue directly to Connections"
        );
        assert!(
            !super::SIGN_IN_HTML.contains("returnTo=%2Fadmin%2Fsession-ready"),
            "the sign-in call to action must not strand users on a fixture page"
        );
    }

    fn google_config() -> Result<GoogleOidcConfig, String> {
        GoogleOidcConfig::new(
            "test-client-id",
            "https://steward.example.test",
            "https://steward.example.test/admin/auth/callback",
            "example.com",
            OrganizationId::parse("org_example")?,
        )
    }

    fn principal() -> Result<BrowserPrincipal, String> {
        Ok(BrowserPrincipal {
            canonical_user_id: CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?,
            display_email: Email::parse("alice@example.com")?,
            role: BrowserRole::User,
        })
    }

    #[test]
    fn production_oidc_contract_rejects_non_google_or_ambiguous_redirect_boundaries()
    -> Result<(), String> {
        let organization_id = OrganizationId::parse("org_example")?;
        for (origin, callback) in [
            (
                "http://steward.example.test",
                "http://steward.example.test/admin/auth/callback",
            ),
            (
                "https://steward.example.test",
                "https://other.example.test/admin/auth/callback",
            ),
            (
                "https://steward.example.test",
                "https://steward.example.test/admin/auth/callback?next=/admin",
            ),
        ] {
            assert!(
                GoogleOidcConfig::new(
                    "test-client-id",
                    origin,
                    callback,
                    "example.com",
                    organization_id.clone(),
                )
                .is_err(),
                "an insecure or non-exact callback must fail closed"
            );
        }
        assert!(
            GoogleOidcConfig::new(
                "test-client-id",
                "https://steward.example.test",
                "https://steward.example.test/admin/auth/callback",
                "",
                organization_id,
            )
            .is_err(),
            "an empty hosted-domain boundary must fail closed"
        );
        Ok(())
    }

    #[test]
    fn production_oidc_origin_is_parsed_normalized_and_has_no_url_components() -> Result<(), String>
    {
        let organization_id = OrganizationId::parse("org_example")?;
        for origin in [
            "https:steward.example.test",
            "https://",
            "https://user@steward.example.test",
            "https://steward.example.test/admin",
            "https://steward.example.test/.",
            "https://steward.example.test/%2e",
            "https://steward.example.test//",
            "https://steward.example.test/?query",
            "https://steward.example.test/#fragment",
            " https://steward.example.test",
            "https://steward.example.test ",
            "https://steward.example.test:invalid",
        ] {
            assert!(
                GoogleOidcConfig::new(
                    "test-client-id",
                    origin,
                    "https://steward.example.test/admin/auth/callback",
                    "example.com",
                    organization_id.clone(),
                )
                .is_err(),
                "non-origin value must fail closed: {origin:?}"
            );
        }

        let normalized = GoogleOidcConfig::new(
            "test-client-id",
            "https://STEWARD.example.test:443/",
            "https://steward.example.test/admin/auth/callback",
            "example.com",
            organization_id,
        )?;
        assert_eq!(normalized.browser_origin, "https://steward.example.test");
        Ok(())
    }

    #[test]
    fn authorization_request_contains_pkce_state_nonce_and_exact_google_tenant()
    -> Result<(), String> {
        let flow = PendingAuthorization::new("/admin/connections", 100)
            .map_err(|error| format!("begin flow: {error:?}"))?;
        let url = google_config()?.authorization_url(&flow);
        for component in [
            "https://accounts.google.com/o/oauth2/v2/auth?",
            "response_type=code",
            "scope=openid%20email%20profile",
            "code_challenge_method=S256",
            "hd=example.com",
            "state=",
            "nonce=",
            "code_challenge=",
        ] {
            assert!(
                url.contains(component),
                "authorization URL omitted {component}"
            );
        }
        assert!(
            !url.contains(&flow.pkce_verifier),
            "PKCE verifier must stay server-side"
        );
        assert!(
            !url.contains("secret"),
            "authorization URL must never contain a client secret"
        );
        Ok(())
    }

    #[test]
    fn deployed_cookie_prefixes_match_their_required_paths() -> Result<(), String> {
        let config = google_config()?;
        let service = super::BrowserAuthService::google(
            config.clone(),
            std::sync::Arc::new(super::GoogleAuthorizationOnlyProvider::new(config)),
            std::sync::Arc::new(super::LocalFakeIdentityResolver),
        )?;
        let flow = PendingAuthorization::new("/admin/connections", 100)
            .map_err(|error| format!("begin deployed cookie flow: {error:?}"))?;
        let flow_cookie = super::flow_cookie(&service.config, &flow.flow_id);
        assert!(flow_cookie.starts_with("__Secure-steward-oidc-flow="));
        assert!(flow_cookie.contains("; Path=/admin/auth;"));
        assert!(flow_cookie.contains("; Secure"));
        assert!(flow_cookie.contains("; HttpOnly;"));

        let session_cookie = super::session_cookie(&service.config, "opaque-session");
        assert!(session_cookie.starts_with("__Host-steward-session="));
        assert!(session_cookie.contains("; Path=/;"));
        assert!(session_cookie.contains("; Secure"));
        assert!(session_cookie.contains("; HttpOnly;"));
        Ok(())
    }

    #[test]
    fn authorization_flow_is_one_time_state_bound_expiring_and_redirect_allowlisted()
    -> Result<(), String> {
        let registry = BrowserSessionRegistry::default();
        assert_eq!(
            registry.begin("https://other.example.test", 100),
            Err(BrowserAuthError::InvalidRedirect)
        );
        let wrong_state = registry
            .begin("/admin/connections", 100)
            .map_err(|error| format!("begin state-negative flow: {error:?}"))?;
        assert_eq!(
            registry.consume_flow(&wrong_state.flow_id, "wrong-state", 101),
            Err(BrowserAuthError::InvalidFlow)
        );
        assert_eq!(
            registry.consume_flow(&wrong_state.flow_id, &wrong_state.state, 101),
            Err(BrowserAuthError::InvalidFlow),
            "a rejected callback must not leave a replayable flow"
        );
        let expired = registry
            .begin("/admin/connections", 100)
            .map_err(|error| format!("begin expiry flow: {error:?}"))?;
        assert_eq!(
            registry.consume_flow(&expired.flow_id, &expired.state, expired.expires_at),
            Err(BrowserAuthError::ExpiredFlow)
        );
        let valid = registry
            .begin("/admin/connections", 100)
            .map_err(|error| format!("begin valid flow: {error:?}"))?;
        assert_eq!(
            registry
                .consume_flow(&valid.flow_id, &valid.state, 101)
                .map_err(|error| format!("consume valid flow: {error:?}"))?,
            valid
        );
        assert_eq!(
            registry.consume_flow(&valid.flow_id, &valid.state, 101),
            Err(BrowserAuthError::InvalidFlow),
            "a successful callback must not be replayable"
        );
        Ok(())
    }

    #[test]
    fn opaque_session_has_independent_csrf_expires_and_revokes() -> Result<(), String> {
        let registry = BrowserSessionRegistry::default();
        let expected_principal = principal()?;
        let session = registry
            .issue(expected_principal.clone(), 100)
            .map_err(|error| format!("issue session: {error:?}"))?;
        assert_ne!(session.token, session.csrf);
        assert!(
            !session
                .token
                .contains(expected_principal.display_email.as_str())
        );
        assert_eq!(
            registry
                .resolve(&session.token, 101)
                .map_err(|error| format!("resolve session: {error:?}"))?
                .principal,
            expected_principal
        );
        assert_eq!(
            registry.resolve(&session.token, session.expires_at),
            Err(BrowserAuthError::ExpiredSession)
        );
        let revoked = registry
            .issue(principal()?, 200)
            .map_err(|error| format!("issue revocation session: {error:?}"))?;
        registry
            .revoke(&revoked.token)
            .map_err(|error| format!("revoke session: {error:?}"))?;
        assert_eq!(
            registry.resolve(&revoked.token, 201),
            Err(BrowserAuthError::InvalidSession)
        );
        Ok(())
    }

    #[test]
    fn registries_fail_closed_at_capacity_and_reclaim_exact_expiry() -> Result<(), String> {
        let flows = BrowserSessionRegistry::default();
        for _ in 0..MAX_PENDING_AUTHORIZATIONS {
            flows
                .begin("/admin/connections", 100)
                .map_err(|error| format!("fill pending registry: {error:?}"))?;
        }
        assert_eq!(
            flows.begin("/admin/connections", 100),
            Err(BrowserAuthError::CapacityExceeded)
        );
        assert_eq!(
            flows
                .state
                .lock()
                .map_err(|_| "lock pending registry".to_owned())?
                .pending
                .len(),
            MAX_PENDING_AUTHORIZATIONS
        );
        flows
            .begin("/admin/connections", 100 + super::FLOW_TTL_SECONDS)
            .map_err(|error| format!("reclaim expired pending entries: {error:?}"))?;
        assert_eq!(
            flows
                .state
                .lock()
                .map_err(|_| "lock reclaimed pending registry".to_owned())?
                .pending
                .len(),
            1,
            "entries expiring exactly now must be reclaimed under the insertion lock"
        );

        let sessions = BrowserSessionRegistry::default();
        let expected_principal = principal()?;
        for _ in 0..MAX_BROWSER_SESSIONS {
            sessions
                .issue(expected_principal.clone(), 200)
                .map_err(|error| format!("fill session registry: {error:?}"))?;
        }
        assert_eq!(
            sessions.issue(expected_principal.clone(), 200),
            Err(BrowserAuthError::CapacityExceeded)
        );
        sessions
            .issue(expected_principal, 200 + super::SESSION_TTL_SECONDS)
            .map_err(|error| format!("reclaim expired session entries: {error:?}"))?;
        assert_eq!(
            sessions
                .state
                .lock()
                .map_err(|_| "lock reclaimed session registry".to_owned())?
                .sessions
                .len(),
            1
        );
        Ok(())
    }

    #[test]
    fn concurrent_flow_insertions_never_exceed_capacity() -> Result<(), String> {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let registry = BrowserSessionRegistry::default();
        let accepted = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let registry = registry.clone();
                let accepted = &accepted;
                scope.spawn(move || {
                    for _ in 0..(MAX_PENDING_AUTHORIZATIONS / 8 + 16) {
                        if registry.begin("/admin/connections", 100).is_ok() {
                            accepted.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                });
            }
        });
        assert_eq!(accepted.load(Ordering::SeqCst), MAX_PENDING_AUTHORIZATIONS);
        assert_eq!(
            registry
                .state
                .lock()
                .map_err(|_| "lock concurrent registry".to_owned())?
                .pending
                .len(),
            MAX_PENDING_AUTHORIZATIONS
        );

        let sessions = BrowserSessionRegistry::default();
        let accepted = AtomicUsize::new(0);
        let expected_principal = principal()?;
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let sessions = sessions.clone();
                let accepted = &accepted;
                let expected_principal = expected_principal.clone();
                scope.spawn(move || {
                    for _ in 0..(MAX_BROWSER_SESSIONS / 8 + 16) {
                        if sessions.issue(expected_principal.clone(), 100).is_ok() {
                            accepted.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                });
            }
        });
        assert_eq!(accepted.load(Ordering::SeqCst), MAX_BROWSER_SESSIONS);
        assert_eq!(
            sessions
                .state
                .lock()
                .map_err(|_| "lock concurrent session registry".to_owned())?
                .sessions
                .len(),
            MAX_BROWSER_SESSIONS
        );
        Ok(())
    }

    #[tokio::test]
    async fn repeated_login_requests_are_bounded_at_the_router() -> Result<(), String> {
        let service =
            local_fake_browser_auth_service("http://127.0.0.1:33001", LocalFakeIdentity::User)?;
        for _ in 0..MAX_PENDING_AUTHORIZATIONS {
            let response = browser_auth_router(service.clone())
                .oneshot(
                    Request::builder()
                        .uri("/admin/auth/login")
                        .body(Body::empty())
                        .map_err(|error| format!("build bounded login request: {error}"))?,
                )
                .await
                .map_err(|error| format!("execute bounded login request: {error}"))?;
            assert_eq!(response.status(), StatusCode::SEE_OTHER);
        }
        let rejected = browser_auth_router(service.clone())
            .oneshot(
                Request::builder()
                    .uri("/admin/auth/login")
                    .body(Body::empty())
                    .map_err(|error| format!("build over-capacity login request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute over-capacity login request: {error}"))?;
        assert_eq!(rejected.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            service
                .registry
                .state
                .lock()
                .map_err(|_| "lock router registry".to_owned())?
                .pending
                .len(),
            MAX_PENDING_AUTHORIZATIONS
        );
        Ok(())
    }

    #[test]
    fn process_restart_invalidates_opaque_sessions_and_pending_flows() -> Result<(), String> {
        let before_restart = BrowserSessionRegistry::default();
        let session = before_restart
            .issue(principal()?, 100)
            .map_err(|error| format!("issue pre-restart session: {error:?}"))?;
        let flow = before_restart
            .begin("/admin/connections", 100)
            .map_err(|error| format!("begin pre-restart flow: {error:?}"))?;

        let after_restart = BrowserSessionRegistry::default();
        assert_eq!(
            after_restart.resolve(&session.token, 101),
            Err(BrowserAuthError::InvalidSession),
            "a fresh process must not accept an opaque handle from the prior process"
        );
        assert_eq!(
            after_restart.consume_flow(&flow.flow_id, &flow.state, 101),
            Err(BrowserAuthError::InvalidFlow),
            "a fresh process must not accept an authorization flow from the prior process"
        );

        let replacement = after_restart
            .issue(principal()?, 101)
            .map_err(|error| format!("issue post-restart session: {error:?}"))?;
        assert_ne!(replacement.token, session.token);
        assert_ne!(replacement.csrf, session.csrf);
        Ok(())
    }

    fn cookie_pair(response: &axum::response::Response, name: &str) -> Result<String, String> {
        response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find(|value| {
                value.starts_with(&format!("{name}=")) && !value.starts_with(&format!("{name}=;"))
            })
            .and_then(|value| value.split(';').next())
            .map(str::to_owned)
            .ok_or_else(|| format!("response omitted {name} cookie"))
    }

    async fn start_local_flow(
        identity: LocalFakeIdentity,
    ) -> Result<(super::BrowserAuthService, String, String), String> {
        let service = local_fake_browser_auth_service("http://127.0.0.1:33001", identity)?;
        let login = browser_auth_router(service.clone())
            .oneshot(
                Request::builder()
                    .uri("/admin/auth/login")
                    .body(Body::empty())
                    .map_err(|error| format!("build login request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute login request: {error}"))?;
        assert_eq!(login.status(), StatusCode::SEE_OTHER);
        let flow_cookie = cookie_pair(&login, "steward-local-oidc-flow")?;
        let authorization = login
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| "login omitted authorization redirect".to_owned())?
            .to_owned();
        let authorized = browser_auth_router(service.clone())
            .oneshot(
                Request::builder()
                    .uri(&authorization)
                    .body(Body::empty())
                    .map_err(|error| format!("build fake authorization request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute fake authorization request: {error}"))?;
        assert!(authorized.status().is_redirection());
        let callback = authorized
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| "fake authorization omitted callback redirect".to_owned())?
            .to_owned();
        Ok((service, flow_cookie, callback))
    }

    #[tokio::test]
    async fn provider_exchange_rejects_a_code_bound_to_another_nonce() -> Result<(), String> {
        let provider = LocalFakeOidcProvider::new(LocalFakeIdentity::User);
        let callback = provider
            .local_authorize("state", "expected-nonce")
            .await
            .map_err(|error| format!("authorize fake identity: {error:?}"))?;
        let code = callback
            .split("code=")
            .nth(1)
            .and_then(|value| value.split('&').next())
            .ok_or_else(|| "fake authorization omitted code".to_owned())?;

        assert!(matches!(
            provider
                .exchange_code(
                    code,
                    "pkce-verifier",
                    "http://127.0.0.1:33001/admin/auth/callback",
                    "different-nonce",
                )
                .await,
            Err(BrowserAuthFailure::InvalidIdentity)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn callback_accepts_only_the_optional_exact_google_issuer_without_query_leakage()
    -> Result<(), String> {
        let (service, flow_cookie, callback_uri) =
            start_local_flow(LocalFakeIdentity::User).await?;
        for suffix in [
            "&iss=",
            "&iss=https%3A%2F%2Fissuer.example.test",
            "&iss=https%3A%2F%2Faccounts.google.com&iss=https%3A%2F%2Faccounts.google.com",
        ] {
            let response = browser_auth_router(service.clone())
                .oneshot(
                    Request::builder()
                        .uri(format!("{callback_uri}{suffix}"))
                        .header(header::COOKIE, &flow_cookie)
                        .body(Body::empty())
                        .map_err(|error| format!("build rejected callback request: {error}"))?,
                )
                .await
                .map_err(|error| format!("execute rejected callback request: {error}"))?;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let body = to_bytes(response.into_body(), 4096)
                .await
                .map_err(|error| format!("read rejected callback response: {error}"))?;
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&body)
                    .map_err(|error| format!("parse rejected callback response: {error}"))?,
                serde_json::json!({ "error": "browser authentication failed" })
            );
        }

        let accepted = browser_auth_router(service)
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "{callback_uri}&iss=https%3A%2F%2Faccounts.google.com&scope=openid%20email&authuser=0&prompt=consent&hd=example.com"
                    ))
                    .header(header::COOKIE, flow_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build accepted callback request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute accepted callback request: {error}"))?;
        assert_eq!(accepted.status(), StatusCode::SEE_OTHER);
        Ok(())
    }

    #[tokio::test]
    async fn local_fake_uses_real_callback_session_router_and_rotates_fixation_cookie()
    -> Result<(), String> {
        let (service, flow_cookie, callback_uri) =
            start_local_flow(LocalFakeIdentity::User).await?;
        let prior = service
            .registry
            .issue(principal()?, super::epoch_seconds())
            .map_err(|error| format!("issue prior session: {error:?}"))?;
        let callback_uri = format!("{callback_uri}&iss=https%3A%2F%2Faccounts.google.com");
        let callback = browser_auth_router(service.clone())
            .oneshot(
                Request::builder()
                    .uri(&callback_uri)
                    .header(
                        header::COOKIE,
                        format!("{flow_cookie}; steward-local-session={}", prior.token),
                    )
                    .body(Body::empty())
                    .map_err(|error| format!("build callback request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute callback request: {error}"))?;
        assert_eq!(callback.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            callback.headers().get(header::LOCATION),
            Some(&header::HeaderValue::from_static("/admin/connections"))
        );
        let session_cookie = cookie_pair(&callback, "steward-local-session")?;
        assert!(!session_cookie.ends_with(&prior.token));
        assert_eq!(
            service
                .registry
                .resolve(&prior.token, super::epoch_seconds()),
            Err(BrowserAuthError::InvalidSession),
            "a successful login must revoke the previous session rather than only overwrite the browser cookie"
        );
        for set_cookie in callback.headers().get_all(header::SET_COOKIE) {
            let value = set_cookie
                .to_str()
                .map_err(|error| format!("read Set-Cookie: {error}"))?;
            assert!(value.contains("HttpOnly"));
            assert!(value.contains("SameSite=Lax"));
            assert!(!value.contains("alice@example.com"));
            assert!(
                !value.contains("Secure"),
                "loopback HTTP fixture cannot use Secure"
            );
        }
        let session = browser_auth_router(service.clone())
            .oneshot(
                Request::builder()
                    .uri("/admin/api/v1/session")
                    .header(header::COOKIE, &session_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build session request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute session request: {error}"))?;
        assert_eq!(session.status(), StatusCode::OK);
        assert_eq!(
            session.headers().get(header::CACHE_CONTROL),
            Some(&header::HeaderValue::from_static("no-store"))
        );
        let body = to_bytes(session.into_body(), 16 * 1024)
            .await
            .map_err(|error| format!("read session body: {error}"))?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("parse session response: {error}"))?;
        assert_eq!(value["apiVersion"], "steward.browser-session/v1");
        assert_eq!(value["principal"]["displayEmail"], "alice@example.com");
        assert_eq!(value["role"], "user");
        assert_eq!(value["surfaces"][0], "connections");
        assert!(
            value["csrf"]
                .as_str()
                .is_some_and(|value| value.len() >= 64)
        );

        let replay = browser_auth_router(service)
            .oneshot(
                Request::builder()
                    .uri(&callback_uri)
                    .header(header::COOKIE, flow_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build replay request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute replay request: {error}"))?;
        assert_eq!(replay.status(), StatusCode::BAD_REQUEST);
        Ok(())
    }

    #[tokio::test]
    async fn wrong_tenant_fails_before_session_and_logout_requires_session_csrf_and_origin()
    -> Result<(), String> {
        let (wrong_service, wrong_cookie, wrong_callback) =
            start_local_flow(LocalFakeIdentity::WrongTenant).await?;
        let wrong = browser_auth_router(wrong_service)
            .oneshot(
                Request::builder()
                    .uri(wrong_callback)
                    .header(header::COOKIE, wrong_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build wrong-tenant callback: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute wrong-tenant callback: {error}"))?;
        assert_eq!(wrong.status(), StatusCode::BAD_REQUEST);
        assert!(cookie_pair(&wrong, "steward-local-session").is_err());

        let (service, flow_cookie, callback_uri) =
            start_local_flow(LocalFakeIdentity::Admin).await?;
        let callback = browser_auth_router(service.clone())
            .oneshot(
                Request::builder()
                    .uri(callback_uri)
                    .header(header::COOKIE, flow_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build admin callback: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute admin callback: {error}"))?;
        let session_cookie = cookie_pair(&callback, "steward-local-session")?;
        let session = browser_auth_router(service.clone())
            .oneshot(
                Request::builder()
                    .uri("/admin/api/v1/session")
                    .header(header::COOKIE, &session_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build admin session request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute admin session request: {error}"))?;
        let body = to_bytes(session.into_body(), 16 * 1024)
            .await
            .map_err(|error| format!("read admin session: {error}"))?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("parse admin session: {error}"))?;
        assert_eq!(value["role"], "admin");
        let csrf = value["csrf"]
            .as_str()
            .ok_or_else(|| "session omitted CSRF proof".to_owned())?;

        let rejected = browser_auth_router(service.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/auth/logout")
                    .header(header::COOKIE, &session_cookie)
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("x-steward-csrf", csrf)
                    .body(Body::empty())
                    .map_err(|error| format!("build cross-origin logout: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute cross-origin logout: {error}"))?;
        assert_eq!(rejected.status(), StatusCode::FORBIDDEN);

        let logged_out = browser_auth_router(service.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/auth/logout")
                    .header(header::COOKIE, &session_cookie)
                    .header(header::ORIGIN, "http://127.0.0.1:33001")
                    .header("sec-fetch-site", "same-origin")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("x-steward-csrf", csrf)
                    .body(Body::empty())
                    .map_err(|error| format!("build valid logout: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute valid logout: {error}"))?;
        assert_eq!(logged_out.status(), StatusCode::NO_CONTENT);
        let revoked = browser_auth_router(service)
            .oneshot(
                Request::builder()
                    .uri("/admin/api/v1/session")
                    .header(header::COOKIE, session_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build revoked session request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute revoked session request: {error}"))?;
        assert_eq!(revoked.status(), StatusCode::UNAUTHORIZED);
        Ok(())
    }

    #[tokio::test]
    async fn protected_mutation_receives_canonical_session_and_validated_proof_extensions()
    -> Result<(), String> {
        async fn probe(
            Extension(session): Extension<BrowserSessionContext>,
            Extension(_proof): Extension<BrowserMutationProof>,
        ) -> Json<serde_json::Value> {
            Json(serde_json::json!({
                "userId": session.principal.canonical_user_id,
                "role": session.principal.role,
            }))
        }

        let service =
            local_fake_browser_auth_service("http://127.0.0.1:33001", LocalFakeIdentity::User)?;
        let issued = service
            .registry
            .issue(principal()?, super::epoch_seconds())
            .map_err(|error| format!("issue protected-route session: {error:?}"))?;
        let routes =
            || protect_browser_routes(Router::new().route("/probe", post(probe)), service.clone());
        let rejected = routes()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/probe")
                    .header(
                        header::COOKIE,
                        format!("steward-local-session={}", issued.token),
                    )
                    .header(header::ORIGIN, "http://127.0.0.1:33001")
                    .header("sec-fetch-site", "same-origin")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::empty())
                    .map_err(|error| format!("build missing-proof request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute missing-proof request: {error}"))?;
        assert_eq!(rejected.status(), StatusCode::FORBIDDEN);

        let accepted = routes()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/probe")
                    .header(
                        header::COOKIE,
                        format!("steward-local-session={}", issued.token),
                    )
                    .header(header::ORIGIN, "http://127.0.0.1:33001")
                    .header("sec-fetch-site", "same-origin")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("x-steward-csrf", issued.csrf)
                    .body(Body::empty())
                    .map_err(|error| format!("build validated mutation request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute validated mutation request: {error}"))?;
        assert_eq!(accepted.status(), StatusCode::OK);
        let body = to_bytes(accepted.into_body(), 16 * 1024)
            .await
            .map_err(|error| format!("read protected mutation response: {error}"))?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("parse protected mutation response: {error}"))?;
        assert_eq!(value["userId"], "usr_0123456789abcdef0123456789abcdef");
        assert_eq!(value["role"], "user");
        Ok(())
    }

    #[tokio::test]
    async fn typed_browser_admin_guard_denies_user_and_bearer_but_accepts_admin()
    -> Result<(), String> {
        async fn admin_probe(
            Extension(authority): Extension<BrowserAdminAuthority>,
        ) -> Json<serde_json::Value> {
            Json(serde_json::json!({
                "role": authority.principal().role,
                "userId": authority.principal().canonical_user_id,
            }))
        }

        let service =
            local_fake_browser_auth_service("http://127.0.0.1:33001", LocalFakeIdentity::User)?;
        let routes = || {
            protect_browser_admin_routes(
                Router::new().route("/admin-probe", axum::routing::get(admin_probe)),
                service.clone(),
            )
        };
        let user = service
            .registry
            .issue(principal()?, super::epoch_seconds())
            .map_err(|error| format!("issue ordinary browser session: {error:?}"))?;
        let mut admin_principal = principal()?;
        admin_principal.role = BrowserRole::Admin;
        let admin = service
            .registry
            .issue(admin_principal, super::epoch_seconds())
            .map_err(|error| format!("issue administrator browser session: {error:?}"))?;

        let unauthenticated = routes()
            .oneshot(
                Request::builder()
                    .uri("/admin-probe")
                    .body(Body::empty())
                    .map_err(|error| format!("build unauthenticated admin request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute unauthenticated admin request: {error}"))?;
        assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

        let token_review_bearer = routes()
            .oneshot(
                Request::builder()
                    .uri("/admin-probe")
                    .header(header::AUTHORIZATION, "Bearer operator-token")
                    .body(Body::empty())
                    .map_err(|error| format!("build bearer-only browser request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute bearer-only browser request: {error}"))?;
        assert_eq!(token_review_bearer.status(), StatusCode::UNAUTHORIZED);

        let forbidden = routes()
            .oneshot(
                Request::builder()
                    .uri("/admin-probe")
                    .header(
                        header::COOKIE,
                        format!("steward-local-session={}", user.token),
                    )
                    .body(Body::empty())
                    .map_err(|error| format!("build ordinary-user admin request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute ordinary-user admin request: {error}"))?;
        assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);

        let accepted = routes()
            .oneshot(
                Request::builder()
                    .uri("/admin-probe")
                    .header(
                        header::COOKIE,
                        format!("steward-local-session={}", admin.token),
                    )
                    .body(Body::empty())
                    .map_err(|error| format!("build administrator browser request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute administrator browser request: {error}"))?;
        assert_eq!(accepted.status(), StatusCode::OK);
        Ok(())
    }
}
