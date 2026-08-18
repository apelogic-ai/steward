//! FAST-TRACK / NON-PROMOTABLE bridge between Steward Connections and MCP-GW.
//!
//! This module is compiled only with the existing `admin-demo` feature. The bridge process is
//! intended to run inside one governed OpenShell sandbox. It never accepts a bearer token and
//! never reads a token file: its only outbound authorization value is OpenShell's documented
//! placeholder, which the sandbox supervisor replaces at the exact allowed egress boundary.

use std::hash::Hash;
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use steward_types::{CanonicalUserId, Email};

use crate::BoxFuture;
use crate::browser_auth::{
    BrowserAuthFailure, BrowserFuture, BrowserIdentityResolver, BrowserPrincipal, BrowserRole,
};
use crate::connections::{
    AuthorizationUrl, ConnectionBrokerError, ConnectionContinuation, ConnectionPhase,
    ConnectionSession, FastTrackBffFailureStage, ProviderConnectionBroker,
    ProviderConnectionStatus,
};

pub const BRIDGE_HEALTH_PATH: &str = "/healthz";
pub const BRIDGE_STATUS_PATH: &str = "/internal/fast-track/v1/github/status";
pub const BRIDGE_START_PATH: &str = "/internal/fast-track/v1/github/start";
pub const BRIDGE_DISCONNECT_PATH: &str = "/internal/fast-track/v1/github/disconnect";

const MCP_GW_STATUS_PATH: &str = "/oauth/github/status";
const MCP_GW_START_PATH: &str = "/oauth/github/start";
const MCP_GW_DISCONNECT_PATH: &str = "/oauth/github/disconnect";
const OPEN_SHELL_BEARER_PLACEHOLDER: &str = "openshell-token-grant-placeholder";
const COMPATIBILITY_ISSUER_HEADER: HeaderName =
    HeaderName::from_static("x-steward-compatibility-issuer");
const COMPATIBILITY_EMAIL_HEADER: HeaderName =
    HeaderName::from_static("x-steward-compatibility-email");
const MAX_RESPONSE_BYTES: usize = 32 * 1024;
const MAX_BRIDGE_TTL: Duration = Duration::from_secs(3_600);

#[derive(Clone)]
struct CompatibilityIdentity {
    email: Email,
    issuer_header: HeaderValue,
    email_header: HeaderValue,
}

impl CompatibilityIdentity {
    fn new(issuer: String, email: String) -> Result<Self, String> {
        let issuer = validate_compatibility_issuer(issuer)?;
        let email = Email::parse(email)?;
        let issuer_header = HeaderValue::from_str(&issuer)
            .map_err(|_| "compatibility issuer must fit one HTTP header".to_owned())?;
        let email_header = HeaderValue::from_str(email.as_str())
            .map_err(|_| "compatibility email must fit one HTTP header".to_owned())?;
        Ok(Self {
            email,
            issuer_header,
            email_header,
        })
    }

    fn matches_session(&self, email: &str) -> bool {
        self.email.as_str().eq_ignore_ascii_case(email)
    }

    fn matches_headers(&self, headers: &HeaderMap) -> bool {
        headers.get(&COMPATIBILITY_ISSUER_HEADER) == Some(&self.issuer_header)
            && headers.get(&COMPATIBILITY_EMAIL_HEADER) == Some(&self.email_header)
    }
}

#[derive(Clone)]
struct BridgeLifetime {
    expires_at: Instant,
    now: Arc<dyn Fn() -> Instant + Send + Sync>,
}

impl BridgeLifetime {
    fn new(ttl: Duration) -> Result<Self, String> {
        Self::with_clock(ttl, Arc::new(Instant::now))
    }

    fn with_clock(
        ttl: Duration,
        now: Arc<dyn Fn() -> Instant + Send + Sync>,
    ) -> Result<Self, String> {
        if ttl.is_zero() || ttl > MAX_BRIDGE_TTL {
            return Err("fast-track bridge TTL must be between 1 and 3600 seconds".to_owned());
        }
        let expires_at = now()
            .checked_add(ttl)
            .ok_or_else(|| "fast-track bridge TTL overflowed".to_owned())?;
        Ok(Self { expires_at, now })
    }

    fn is_active(&self) -> bool {
        (self.now)() < self.expires_at
    }
}

#[derive(Clone)]
pub struct FastTrackBridgeConfig {
    mcp_gateway_origin: Url,
    redirect_after: Url,
    identity: CompatibilityIdentity,
    lifetime: BridgeLifetime,
}

impl FastTrackBridgeConfig {
    pub fn new(
        mcp_gateway_origin: String,
        compatibility_issuer: String,
        compatibility_email: String,
        redirect_after: String,
        ttl: Duration,
    ) -> Result<Self, String> {
        Ok(Self {
            mcp_gateway_origin: validate_origin(&mcp_gateway_origin, "MCP-GW")?,
            redirect_after: validate_redirect_after(&redirect_after)?,
            identity: CompatibilityIdentity::new(compatibility_issuer, compatibility_email)?,
            lifetime: BridgeLifetime::new(ttl)?,
        })
    }

    #[cfg(test)]
    fn with_clock(
        mcp_gateway_origin: String,
        compatibility_issuer: String,
        compatibility_email: String,
        redirect_after: String,
        ttl: Duration,
        now: Arc<dyn Fn() -> Instant + Send + Sync>,
    ) -> Result<Self, String> {
        Ok(Self {
            mcp_gateway_origin: validate_origin(&mcp_gateway_origin, "MCP-GW")?,
            redirect_after: validate_redirect_after(&redirect_after)?,
            identity: CompatibilityIdentity::new(compatibility_issuer, compatibility_email)?,
            lifetime: BridgeLifetime::with_clock(ttl, now)?,
        })
    }
}

#[derive(Clone)]
pub struct FastTrackConnectionsBridge {
    config: FastTrackBridgeConfig,
    client: Client,
}

impl FastTrackConnectionsBridge {
    pub fn new(config: FastTrackBridgeConfig) -> Result<Self, String> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|_| "build bounded fast-track bridge HTTP client".to_owned())?;
        Ok(Self { config, client })
    }

    pub fn router(self) -> Router {
        Router::new()
            .route(BRIDGE_HEALTH_PATH, get(bridge_health))
            .route(BRIDGE_STATUS_PATH, get(bridge_status))
            .route(BRIDGE_START_PATH, post(bridge_start))
            .route(BRIDGE_DISCONNECT_PATH, post(bridge_disconnect))
            .with_state(self)
    }

    async fn forward(
        &self,
        method: reqwest::Method,
        path: &'static str,
        body: Option<&serde_json::Value>,
    ) -> Result<(StatusCode, Vec<u8>), ()> {
        if !self.config.lifetime.is_active() {
            return Err(());
        }
        let target = endpoint(&self.config.mcp_gateway_origin, path).map_err(|_| ())?;
        let mut request = self.client.request(method, target).header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {OPEN_SHELL_BEARER_PLACEHOLDER}"),
        );
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.map_err(|_| ())?;
        let status = StatusCode::from_u16(response.status().as_u16()).map_err(|_| ())?;
        let body = read_bounded(response).await?;
        Ok((status, body))
    }
}

#[derive(Clone)]
pub struct FastTrackConnectionsBff<B> {
    bridge_origin: Url,
    canonical_user_id: CanonicalUserId,
    identity: CompatibilityIdentity,
    lifetime: BridgeLifetime,
    client: Client,
    binding: PhantomData<fn() -> B>,
}

/// Fixed single-user resolver for the explicitly named Fast-Track preview only.
///
/// Google signature, issuer, hosted-domain, email-verification, nonce, and PKCE validation still
/// happen in the production provider before this resolver is called. This seam merely avoids a
/// database dependency by accepting one exact verified email and returning one configured
/// canonical ID for the bounded preview lifetime.
#[derive(Clone)]
pub struct FastTrackIdentityResolver {
    canonical_user_id: CanonicalUserId,
    compatibility_email: Email,
}

impl FastTrackIdentityResolver {
    pub fn new(
        canonical_user_id: CanonicalUserId,
        compatibility_email: String,
    ) -> Result<Self, String> {
        Ok(Self {
            canonical_user_id,
            compatibility_email: Email::parse(compatibility_email)?,
        })
    }
}

impl BrowserIdentityResolver for FastTrackIdentityResolver {
    fn resolve_or_register<'a>(
        &'a self,
        identity: &'a steward_types::OrganizationIdentity,
    ) -> BrowserFuture<'a, Result<BrowserPrincipal, BrowserAuthFailure>> {
        Box::pin(async move {
            if identity.issuer() != steward_types::GOOGLE_ORGANIZATION_ISSUER
                || !identity
                    .verified_email()
                    .as_str()
                    .eq_ignore_ascii_case(self.compatibility_email.as_str())
            {
                return Err(BrowserAuthFailure::InvalidIdentity);
            }
            Ok(BrowserPrincipal {
                canonical_user_id: self.canonical_user_id.clone(),
                display_email: self.compatibility_email.clone(),
                role: BrowserRole::User,
                member_roles: Vec::new(),
            })
        })
    }
}

impl<B> FastTrackConnectionsBff<B> {
    pub fn new(
        bridge_origin: String,
        canonical_user_id: CanonicalUserId,
        compatibility_issuer: String,
        compatibility_email: String,
        ttl: Duration,
    ) -> Result<Self, String> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| "build bounded fast-track BFF bridge client".to_owned())?;
        Ok(Self {
            bridge_origin: validate_origin(&bridge_origin, "bridge")?,
            canonical_user_id,
            identity: CompatibilityIdentity::new(compatibility_issuer, compatibility_email)?,
            lifetime: BridgeLifetime::new(ttl)?,
            client,
            binding: PhantomData,
        })
    }

    fn session_matches(&self, session: &ConnectionSession<B>) -> bool {
        session.subject.canonical_user_id == self.canonical_user_id
            && self
                .identity
                .matches_session(&session.subject.display_email)
    }

    async fn request(
        &self,
        method: reqwest::Method,
        path: &'static str,
    ) -> Result<(StatusCode, Vec<u8>), ConnectionBrokerError> {
        if !self.lifetime.is_active() {
            return Err(ConnectionBrokerError::FastTrackUnavailable(
                FastTrackBffFailureStage::LifetimeExpired,
            ));
        }
        let target = endpoint(&self.bridge_origin, path).map_err(|_| {
            ConnectionBrokerError::FastTrackUnavailable(FastTrackBffFailureStage::TargetUrl)
        })?;
        let response = self
            .client
            .request(method, target)
            .header(
                COMPATIBILITY_ISSUER_HEADER.as_str(),
                self.identity.issuer_header.clone(),
            )
            .header(
                COMPATIBILITY_EMAIL_HEADER.as_str(),
                self.identity.email_header.clone(),
            )
            .send()
            .await
            .map_err(|_| {
                ConnectionBrokerError::FastTrackUnavailable(
                    FastTrackBffFailureStage::BridgeTransport,
                )
            })?;
        let status = StatusCode::from_u16(response.status().as_u16()).map_err(|_| {
            ConnectionBrokerError::FastTrackUnavailable(FastTrackBffFailureStage::BridgeHttpStatus)
        })?;
        let body = read_bounded(response).await.map_err(|_| {
            ConnectionBrokerError::FastTrackUnavailable(FastTrackBffFailureStage::BridgeTransport)
        })?;
        Ok((status, body))
    }
}

impl<B> ProviderConnectionBroker<B> for FastTrackConnectionsBff<B>
where
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    fn status<'a>(
        &'a self,
        session: &'a ConnectionSession<B>,
    ) -> BoxFuture<'a, Result<ProviderConnectionStatus, ConnectionBrokerError>> {
        Box::pin(async move {
            if !self.session_matches(session) {
                return Err(ConnectionBrokerError::FastTrackUnavailable(
                    FastTrackBffFailureStage::SessionMismatch,
                ));
            }
            let (status, body) = self
                .request(reqwest::Method::GET, BRIDGE_STATUS_PATH)
                .await?;
            if status != StatusCode::OK {
                return Err(ConnectionBrokerError::FastTrackUnavailable(
                    FastTrackBffFailureStage::BridgeHttpStatus,
                ));
            }
            let status: GithubStatus = serde_json::from_slice(&body).map_err(|_| {
                ConnectionBrokerError::FastTrackUnavailable(
                    FastTrackBffFailureStage::ResponseSchema,
                )
            })?;
            status.into_provider_status()
        })
    }

    fn start<'a>(
        &'a self,
        session: &'a ConnectionSession<B>,
    ) -> BoxFuture<'a, Result<AuthorizationUrl, ConnectionBrokerError>> {
        Box::pin(async move {
            if !self.session_matches(session) {
                return Err(ConnectionBrokerError::FastTrackUnavailable(
                    FastTrackBffFailureStage::SessionMismatch,
                ));
            }
            let (status, body) = self
                .request(reqwest::Method::POST, BRIDGE_START_PATH)
                .await?;
            if status != StatusCode::OK {
                return Err(ConnectionBrokerError::FastTrackUnavailable(
                    FastTrackBffFailureStage::BridgeHttpStatus,
                ));
            }
            let started: GithubStart = serde_json::from_slice(&body).map_err(|_| {
                ConnectionBrokerError::FastTrackUnavailable(
                    FastTrackBffFailureStage::ResponseSchema,
                )
            })?;
            AuthorizationUrl::new(started.authorization_url).map_err(|_| {
                ConnectionBrokerError::FastTrackUnavailable(
                    FastTrackBffFailureStage::ResponseSemantics,
                )
            })
        })
    }

    fn complete<'a>(
        &'a self,
        _session: &'a ConnectionSession<B>,
        _continuation: &'a ConnectionContinuation,
    ) -> BoxFuture<'a, Result<(), ConnectionBrokerError>> {
        Box::pin(async { Err(ConnectionBrokerError::InvalidOrExpiredContinuation) })
    }

    fn disconnect<'a>(
        &'a self,
        session: &'a ConnectionSession<B>,
    ) -> BoxFuture<'a, Result<(), ConnectionBrokerError>> {
        Box::pin(async move {
            if !self.session_matches(session) {
                return Err(ConnectionBrokerError::FastTrackUnavailable(
                    FastTrackBffFailureStage::SessionMismatch,
                ));
            }
            let (status, body) = self
                .request(reqwest::Method::POST, BRIDGE_DISCONNECT_PATH)
                .await?;
            if status != StatusCode::NO_CONTENT || !body.is_empty() {
                return Err(ConnectionBrokerError::FastTrackUnavailable(
                    FastTrackBffFailureStage::BridgeHttpStatus,
                ));
            }
            Ok(())
        })
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GithubStatus {
    connected: bool,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    scopes_required: Vec<String>,
    #[serde(default)]
    scopes_granted: Vec<String>,
    #[serde(default)]
    missing_scopes: Vec<String>,
}

impl GithubStatus {
    fn into_provider_status(self) -> Result<ProviderConnectionStatus, ConnectionBrokerError> {
        if self.connected && (self.email.is_none() || !self.missing_scopes.is_empty()) {
            return Err(ConnectionBrokerError::FastTrackUnavailable(
                FastTrackBffFailureStage::ResponseSemantics,
            ));
        }
        let phase = if self.connected {
            ConnectionPhase::Connected
        } else if self.email.is_some() && !self.missing_scopes.is_empty() {
            ConnectionPhase::ReauthRequired
        } else {
            ConnectionPhase::Disconnected
        };
        Ok(ProviderConnectionStatus {
            phase,
            account_email: self.email,
            scopes_required: self.scopes_required,
            scopes_granted: self.scopes_granted,
            scopes_missing: self.missing_scopes,
            expires_at: None,
        })
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GithubStart {
    authorization_url: String,
}

async fn bridge_health(State(bridge): State<FastTrackConnectionsBridge>) -> Response {
    if bridge.config.lifetime.is_active() {
        StatusCode::NO_CONTENT.into_response()
    } else {
        StatusCode::GONE.into_response()
    }
}

fn authorize_internal(headers: &HeaderMap, bridge: &FastTrackConnectionsBridge) -> bool {
    bridge.config.lifetime.is_active()
        && !headers.contains_key(axum::http::header::AUTHORIZATION)
        && bridge.config.identity.matches_headers(headers)
}

async fn bridge_status(
    State(bridge): State<FastTrackConnectionsBridge>,
    headers: HeaderMap,
) -> Response {
    if !authorize_internal(&headers, &bridge) {
        return StatusCode::FORBIDDEN.into_response();
    }
    match bridge
        .forward(reqwest::Method::GET, MCP_GW_STATUS_PATH, None)
        .await
    {
        Ok((StatusCode::OK, body)) => match serde_json::from_slice::<GithubStatus>(&body) {
            Ok(status) => Json(status).into_response(),
            Err(_) => StatusCode::BAD_GATEWAY.into_response(),
        },
        _ => StatusCode::BAD_GATEWAY.into_response(),
    }
}

async fn bridge_start(
    State(bridge): State<FastTrackConnectionsBridge>,
    headers: HeaderMap,
) -> Response {
    if !authorize_internal(&headers, &bridge) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let request = serde_json::json!({
        "redirectAfter": bridge.config.redirect_after.as_str(),
    });
    match bridge
        .forward(reqwest::Method::POST, MCP_GW_START_PATH, Some(&request))
        .await
    {
        Ok((StatusCode::OK, body)) => match serde_json::from_slice::<GithubStart>(&body) {
            Ok(started) if AuthorizationUrl::new(started.authorization_url.clone()).is_ok() => {
                Json(started).into_response()
            }
            _ => StatusCode::BAD_GATEWAY.into_response(),
        },
        _ => StatusCode::BAD_GATEWAY.into_response(),
    }
}

async fn bridge_disconnect(
    State(bridge): State<FastTrackConnectionsBridge>,
    headers: HeaderMap,
) -> Response {
    if !authorize_internal(&headers, &bridge) {
        return StatusCode::FORBIDDEN.into_response();
    }
    match bridge
        .forward(reqwest::Method::POST, MCP_GW_DISCONNECT_PATH, None)
        .await
    {
        Ok((StatusCode::NO_CONTENT, body)) if body.is_empty() => {
            StatusCode::NO_CONTENT.into_response()
        }
        _ => StatusCode::BAD_GATEWAY.into_response(),
    }
}

async fn read_bounded(mut response: reqwest::Response) -> Result<Vec<u8>, ()> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn endpoint(origin: &Url, path: &'static str) -> Result<Url, String> {
    origin
        .join(path)
        .map_err(|_| "construct exact bridge endpoint".to_owned())
}

fn validate_origin(value: &str, label: &str) -> Result<Url, String> {
    let url = Url::parse(value).map_err(|_| format!("{label} origin must be one HTTP origin"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(url.path(), "" | "/")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(format!("{label} origin must be one HTTP origin"));
    }
    Ok(url)
}

fn validate_compatibility_issuer(value: String) -> Result<String, String> {
    let url = Url::parse(&value)
        .map_err(|_| "compatibility issuer must be one HTTPS issuer".to_owned())?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("compatibility issuer must be one HTTPS issuer".to_owned());
    }
    Ok(value)
}

fn validate_redirect_after(value: &str) -> Result<Url, String> {
    let url = Url::parse(value)
        .map_err(|_| "redirect-after must be the exact HTTPS Connections page".to_owned())?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/admin/connections"
        || url.query().is_some()
        || !matches!(url.fragment(), None | Some("github-connected"))
    {
        return Err("redirect-after must be the exact HTTPS Connections page".to_owned());
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::atomic::{AtomicU64, Ordering};

    use axum::body::{Body, to_bytes};
    use axum::extract::Request;
    use axum::http::header;
    use axum::routing::any;
    use tokio::net::TcpListener;
    use tower::ServiceExt;

    use super::*;
    use crate::connections::ConnectionSubject;
    use steward_types::{GOOGLE_ORGANIZATION_ISSUER, OrganizationId, OrganizationIdentityPolicy};

    const ISSUER: &str = "https://steward-mint.preview.example";
    const EMAIL: &str = "alice@example.com";

    fn config(origin: String) -> Result<FastTrackBridgeConfig, String> {
        FastTrackBridgeConfig::new(
            origin,
            ISSUER.to_owned(),
            EMAIL.to_owned(),
            "https://steward.preview.example/admin/connections#github-connected".to_owned(),
            Duration::from_secs(300),
        )
    }

    fn identity_headers(request: axum::http::request::Builder) -> axum::http::request::Builder {
        request
            .header(COMPATIBILITY_ISSUER_HEADER, ISSUER)
            .header(COMPATIBILITY_EMAIL_HEADER, EMAIL)
    }

    async fn spawn_gateway() -> Result<(String, tokio::task::JoinHandle<()>), String> {
        async fn handler(request: Request) -> Response {
            assert_eq!(
                request.headers().get(header::AUTHORIZATION),
                Some(&HeaderValue::from_static(
                    "Bearer openshell-token-grant-placeholder"
                )),
                "the bridge must emit only the OpenShell placeholder"
            );
            match (request.method().clone(), request.uri().path()) {
                (axum::http::Method::GET, MCP_GW_STATUS_PATH) => Json(serde_json::json!({
                    "connected": true,
                    "email": EMAIL,
                    "scopesRequired": ["repo"],
                    "scopesGranted": ["repo"],
                    "missingScopes": [],
                }))
                .into_response(),
                (axum::http::Method::POST, MCP_GW_START_PATH) => {
                    let body = to_bytes(request.into_body(), MAX_RESPONSE_BYTES)
                        .await
                        .map_err(|_| ())
                        .and_then(|body| {
                            serde_json::from_slice::<serde_json::Value>(&body).map_err(|_| ())
                        })
                        .unwrap_or(serde_json::Value::Null);
                    assert_eq!(
                        body["redirectAfter"],
                        "https://steward.preview.example/admin/connections#github-connected"
                    );
                    Json(serde_json::json!({
                        "authorizationUrl": "https://github.test/login/oauth/authorize?state=one-time"
                    }))
                    .into_response()
                }
                (axum::http::Method::POST, MCP_GW_DISCONNECT_PATH) => {
                    StatusCode::NO_CONTENT.into_response()
                }
                _ => StatusCode::NOT_FOUND.into_response(),
            }
        }

        let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .map_err(|error| format!("bind fake MCP-GW: {error}"))?;
        let address = listener
            .local_addr()
            .map_err(|error| format!("read fake MCP-GW address: {error}"))?;
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, Router::new().fallback(any(handler))).await;
        });
        Ok((format!("http://{address}"), task))
    }

    async fn spawn_bridge(
        gateway_origin: String,
    ) -> Result<(String, tokio::task::JoinHandle<()>), String> {
        let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .map_err(|error| format!("bind fast-track bridge: {error}"))?;
        let address = listener
            .local_addr()
            .map_err(|error| format!("read fast-track bridge address: {error}"))?;
        let app = FastTrackConnectionsBridge::new(config(gateway_origin)?)?.router();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok((format!("http://{address}"), task))
    }

    #[tokio::test]
    async fn bridge_exposes_only_fixed_routes_and_forwards_only_placeholder() -> Result<(), String>
    {
        let (origin, gateway) = spawn_gateway().await?;
        let app = FastTrackConnectionsBridge::new(config(origin)?)?.router();

        let forbidden = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(BRIDGE_STATUS_PATH)
                    .body(Body::empty())
                    .map_err(|error| format!("build identity-free bridge request: {error}"))?,
            )
            .await
            .map_err(|error| format!("request identity-free bridge status: {error}"))?;
        assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);

        let bearer_rejected = app
            .clone()
            .oneshot(
                identity_headers(Request::builder())
                    .uri(BRIDGE_STATUS_PATH)
                    .header(header::AUTHORIZATION, "Bearer must-not-enter-the-bridge")
                    .body(Body::empty())
                    .map_err(|error| format!("build bearer-bearing bridge request: {error}"))?,
            )
            .await
            .map_err(|error| format!("request bearer-bearing bridge status: {error}"))?;
        assert_eq!(bearer_rejected.status(), StatusCode::FORBIDDEN);

        let status = app
            .clone()
            .oneshot(
                identity_headers(Request::builder())
                    .uri(BRIDGE_STATUS_PATH)
                    .body(Body::empty())
                    .map_err(|error| format!("build bridge status: {error}"))?,
            )
            .await
            .map_err(|error| format!("request bridge status: {error}"))?;
        assert_eq!(status.status(), StatusCode::OK);

        let start = app
            .clone()
            .oneshot(
                identity_headers(Request::builder())
                    .method(axum::http::Method::POST)
                    .uri(BRIDGE_START_PATH)
                    .body(Body::empty())
                    .map_err(|error| format!("build bridge start: {error}"))?,
            )
            .await
            .map_err(|error| format!("request bridge start: {error}"))?;
        assert_eq!(start.status(), StatusCode::OK);

        let disconnect = app
            .clone()
            .oneshot(
                identity_headers(Request::builder())
                    .method(axum::http::Method::POST)
                    .uri(BRIDGE_DISCONNECT_PATH)
                    .body(Body::empty())
                    .map_err(|error| format!("build bridge disconnect: {error}"))?,
            )
            .await
            .map_err(|error| format!("request bridge disconnect: {error}"))?;
        assert_eq!(disconnect.status(), StatusCode::NO_CONTENT);

        for (method, path) in [
            (axum::http::Method::GET, "/oauth/github/callback"),
            (axum::http::Method::GET, "/oauth/github/start"),
            (
                axum::http::Method::POST,
                "/internal/fast-track/v1/github/callback",
            ),
        ] {
            let response = app
                .clone()
                .oneshot(
                    identity_headers(Request::builder())
                        .method(method)
                        .uri(path)
                        .body(Body::empty())
                        .map_err(|error| format!("build forbidden route request: {error}"))?,
                )
                .await
                .map_err(|error| format!("request forbidden bridge route: {error}"))?;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "route {path}");
        }
        let wrong_method = app
            .oneshot(
                identity_headers(Request::builder())
                    .method(axum::http::Method::GET)
                    .uri(BRIDGE_DISCONNECT_PATH)
                    .body(Body::empty())
                    .map_err(|error| format!("build wrong-method request: {error}"))?,
            )
            .await
            .map_err(|error| format!("request wrong-method bridge route: {error}"))?;
        assert_eq!(wrong_method.status(), StatusCode::METHOD_NOT_ALLOWED);
        gateway.abort();
        Ok(())
    }

    #[tokio::test]
    async fn bridge_expires_and_cannot_be_extended_by_callers() -> Result<(), String> {
        let (origin, gateway) = spawn_gateway().await?;
        let base = Instant::now();
        let elapsed = Arc::new(AtomicU64::new(0));
        let clock_elapsed = Arc::clone(&elapsed);
        let clock: Arc<dyn Fn() -> Instant + Send + Sync> =
            Arc::new(move || base + Duration::from_secs(clock_elapsed.load(Ordering::SeqCst)));
        let config = FastTrackBridgeConfig::with_clock(
            origin,
            ISSUER.to_owned(),
            EMAIL.to_owned(),
            "https://steward.preview.example/admin/connections".to_owned(),
            Duration::from_secs(30),
            clock,
        )?;
        let app = FastTrackConnectionsBridge::new(config)?.router();
        elapsed.store(31, Ordering::SeqCst);

        let health = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(BRIDGE_HEALTH_PATH)
                    .body(Body::empty())
                    .map_err(|error| format!("build expired health request: {error}"))?,
            )
            .await
            .map_err(|error| format!("request expired health: {error}"))?;
        assert_eq!(health.status(), StatusCode::GONE);

        let status = app
            .oneshot(
                identity_headers(Request::builder())
                    .uri(BRIDGE_STATUS_PATH)
                    .body(Body::empty())
                    .map_err(|error| format!("build expired status request: {error}"))?,
            )
            .await
            .map_err(|error| format!("request expired status: {error}"))?;
        assert_eq!(status.status(), StatusCode::FORBIDDEN);
        gateway.abort();
        Ok(())
    }

    #[tokio::test]
    async fn one_fixed_browser_identity_reaches_only_the_three_bridge_operations()
    -> Result<(), String> {
        #[derive(Clone, Eq, Hash, PartialEq)]
        struct Binding;

        let (gateway_origin, gateway) = spawn_gateway().await?;
        let (bridge_origin, bridge) = spawn_bridge(gateway_origin).await?;
        let canonical = CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?;
        let bff = FastTrackConnectionsBff::<Binding>::new(
            bridge_origin,
            canonical.clone(),
            ISSUER.to_owned(),
            EMAIL.to_owned(),
            Duration::from_secs(300),
        )?;
        let session = ConnectionSession {
            subject: ConnectionSubject {
                canonical_user_id: canonical,
                display_email: EMAIL.to_owned(),
            },
            binding: Binding,
        };

        let status = bff
            .status(&session)
            .await
            .map_err(|error| format!("read bridged status: {error:?}"))?;
        assert_eq!(status.phase, ConnectionPhase::Connected);
        assert_eq!(status.account_email.as_deref(), Some(EMAIL));
        assert_eq!(status.scopes_required, vec!["repo"]);
        assert_eq!(status.scopes_granted, vec!["repo"]);
        assert!(status.scopes_missing.is_empty());

        let authorization = bff
            .start(&session)
            .await
            .map_err(|error| format!("start bridged OAuth: {error:?}"))?;
        assert_eq!(
            authorization.as_str(),
            "https://github.test/login/oauth/authorize?state=one-time"
        );
        bff.disconnect(&session)
            .await
            .map_err(|error| format!("disconnect bridged OAuth: {error:?}"))?;

        gateway.abort();
        bridge.abort();
        Ok(())
    }

    #[test]
    fn config_rejects_unbounded_or_ambiguous_targets() {
        for origin in [
            "https://mcp.test/path",
            "https://user@mcp.test",
            "file:///tmp/mcp",
            "https://mcp.test?target=other",
        ] {
            assert!(config(origin.to_owned()).is_err(), "origin {origin}");
        }
        assert!(
            FastTrackBridgeConfig::new(
                "https://mcp.test".to_owned(),
                ISSUER.to_owned(),
                EMAIL.to_owned(),
                "https://steward.preview.example/admin/connections".to_owned(),
                Duration::from_secs(3_601),
            )
            .is_err()
        );
    }

    #[test]
    fn bff_fails_closed_for_every_noncanonical_browser_identity() -> Result<(), String> {
        #[derive(Clone, Eq, Hash, PartialEq)]
        struct Binding;

        let canonical = CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?;
        let bff = FastTrackConnectionsBff::<Binding>::new(
            "http://bridge.test".to_owned(),
            canonical.clone(),
            ISSUER.to_owned(),
            EMAIL.to_owned(),
            Duration::from_secs(300),
        )?;
        let wrong_email = ConnectionSession {
            subject: ConnectionSubject {
                canonical_user_id: canonical.clone(),
                display_email: "bob@example.org".to_owned(),
            },
            binding: Binding,
        };
        let wrong_user = ConnectionSession {
            subject: ConnectionSubject {
                canonical_user_id: CanonicalUserId::parse("usr_abcdef0123456789abcdef0123456789")?,
                display_email: EMAIL.to_owned(),
            },
            binding: Binding,
        };
        assert!(!bff.session_matches(&wrong_email));
        assert!(!bff.session_matches(&wrong_user));
        Ok(())
    }

    #[tokio::test]
    async fn fixed_resolver_accepts_only_the_current_verified_google_email() -> Result<(), String> {
        let policy = OrganizationIdentityPolicy::new(
            GOOGLE_ORGANIZATION_ISSUER,
            "example.com",
            OrganizationId::parse("org_example")?,
        )?;
        let resolver = FastTrackIdentityResolver::new(
            CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?,
            EMAIL.to_owned(),
        )?;
        let accepted = policy.validate(
            GOOGLE_ORGANIZATION_ISSUER,
            "google-subject-one",
            "example.com",
            EMAIL,
            true,
        )?;
        let principal = resolver
            .resolve_or_register(&accepted)
            .await
            .map_err(|error| format!("resolve fixed preview identity: {error:?}"))?;
        assert_eq!(principal.display_email.as_str(), EMAIL);
        assert_eq!(principal.role, BrowserRole::User);

        let rejected = policy.validate(
            GOOGLE_ORGANIZATION_ISSUER,
            "google-subject-two",
            "example.com",
            "bob@example.com",
            true,
        )?;
        assert!(
            resolver.resolve_or_register(&rejected).await.is_err(),
            "the preview must not admit a second verified organization user"
        );
        Ok(())
    }
}
