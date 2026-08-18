//! User-bound provider connection status and consent BFF.

use std::hash::Hash;

use axum::extract::{Query, Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use steward_types::CanonicalUserId;

use crate::BoxFuture;
use crate::browser_auth::{
    BrowserAuthService, BrowserMutationProof, BrowserSessionBinding, BrowserSessionContext,
    protect_browser_routes,
};

pub const CONNECTIONS_API_VERSION: &str = "steward.connections/v1";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectionSubject {
    pub canonical_user_id: CanonicalUserId,
    pub display_email: String,
}

/// Authenticated browser context supplied only after the session middleware succeeds.
///
/// The binding is deliberately generic so the Connections BFF can consume the opaque,
/// non-serializable browser-session binding without learning its representation.
#[derive(Clone, Eq, PartialEq)]
pub struct ConnectionSession<B> {
    pub subject: ConnectionSubject,
    pub binding: B,
}

/// Proof inserted by browser-session middleware only after dynamic CSRF and origin validation.
#[derive(Clone, Copy)]
pub(crate) struct ConnectionMutationProof;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionPhase {
    Disconnected,
    Connecting,
    Connected,
    ReauthRequired,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConnectionStatus {
    pub phase: ConnectionPhase,
    pub account_email: Option<String>,
    pub scopes_required: Vec<String>,
    pub scopes_granted: Vec<String>,
    pub scopes_missing: Vec<String>,
    pub expires_at: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectionStatusResponse {
    api_version: &'static str,
    provider: &'static str,
    status: ProviderConnectionStatus,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConnectionCallbackQuery {
    continuation: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DisconnectConnectionRequest {
    confirm: bool,
}

/// One-time browser destination. It intentionally implements neither `Debug` nor `Display`.
pub struct AuthorizationUrl(String);

impl AuthorizationUrl {
    pub fn new(value: String) -> Result<Self, &'static str> {
        if !value.starts_with("https://") {
            return Err("provider authorization URL must use HTTPS");
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[cfg(feature = "admin-demo")]
    pub(crate) fn new_loopback(value: String) -> Result<Self, &'static str> {
        if !(value.starts_with("http://127.0.0.1:") || value.starts_with("http://[::1]:")) {
            return Err("local provider authorization URL must use an explicit loopback origin");
        }
        Ok(Self(value))
    }
}

/// Opaque one-time continuation returned after the provider callback boundary.
/// It intentionally implements neither `Debug` nor `Display`.
pub struct ConnectionContinuation(String);

impl ConnectionContinuation {
    pub fn new(value: String) -> Result<Self, &'static str> {
        if value.is_empty() || value.len() > 512 {
            return Err("provider continuation must be bounded");
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionBrokerError {
    InvalidOrExpiredContinuation,
    SessionMismatch,
    Unavailable,
    FastTrackUnavailable(FastTrackBffFailureStage),
}

/// Fixed diagnostic stages exposed only by the non-promotable attended preview.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FastTrackBffFailureStage {
    LifetimeExpired,
    SessionMismatch,
    TargetUrl,
    BridgeTransport,
    BridgeHttpStatus,
    ResponseSchema,
    ResponseSemantics,
}

impl FastTrackBffFailureStage {
    const fn as_str(self) -> &'static str {
        match self {
            Self::LifetimeExpired => "lifetime_expired",
            Self::SessionMismatch => "session_mismatch",
            Self::TargetUrl => "target_url",
            Self::BridgeTransport => "bridge_transport",
            Self::BridgeHttpStatus => "bridge_http_status",
            Self::ResponseSchema => "response_schema",
            Self::ResponseSemantics => "response_semantics",
        }
    }
}

pub trait ProviderConnectionBroker<B>: Clone + Send + Sync + 'static
where
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    fn status<'a>(
        &'a self,
        session: &'a ConnectionSession<B>,
    ) -> BoxFuture<'a, Result<ProviderConnectionStatus, ConnectionBrokerError>>;

    fn start<'a>(
        &'a self,
        session: &'a ConnectionSession<B>,
    ) -> BoxFuture<'a, Result<AuthorizationUrl, ConnectionBrokerError>>;

    fn complete<'a>(
        &'a self,
        session: &'a ConnectionSession<B>,
        continuation: &'a ConnectionContinuation,
    ) -> BoxFuture<'a, Result<(), ConnectionBrokerError>>;

    fn disconnect<'a>(
        &'a self,
        session: &'a ConnectionSession<B>,
    ) -> BoxFuture<'a, Result<(), ConnectionBrokerError>>;
}

#[derive(Clone)]
struct ConnectionsState<P> {
    broker: P,
}

fn inner_router<P, B>(broker: P) -> Router
where
    P: ProviderConnectionBroker<B>,
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    Router::new()
        .route(
            "/admin/api/v1/connections/github",
            get(connection_status::<P, B>),
        )
        .route(
            "/admin/api/v1/connections/github/start",
            post(start_connection::<P, B>),
        )
        .route(
            "/admin/connections/github/callback",
            get(complete_connection::<P, B>),
        )
        .route(
            "/admin/api/v1/connections/github/disconnect",
            post(disconnect_connection::<P, B>),
        )
        .merge(crate::connections_ui::router::<B, ConnectionsState<P>>())
        .with_state(ConnectionsState { broker })
}

/// Mount the Connections surface behind Steward's browser-session boundary.
///
/// Keeping the unprotected router private makes it impossible for production callers to
/// accidentally expose connection state or mutations without the session, origin, fetch-site,
/// JSON, and per-session CSRF checks enforced by `protect_browser_routes`.
pub fn protected_router<P>(broker: P, browser_auth: BrowserAuthService) -> Router
where
    P: ProviderConnectionBroker<BrowserSessionBinding>,
{
    let routes = inner_router(broker).route_layer(middleware::from_fn(adapt_browser_context));
    protect_browser_routes(routes, browser_auth)
}

async fn adapt_browser_context(mut request: Request, next: Next) -> Response {
    if let Some(context) = request.extensions().get::<BrowserSessionContext>().cloned() {
        request.extensions_mut().insert(ConnectionSession {
            subject: ConnectionSubject {
                canonical_user_id: context.principal.canonical_user_id,
                display_email: context.principal.display_email.as_str().to_owned(),
            },
            binding: context.binding,
        });
        if matches!(
            *request.method(),
            Method::POST | Method::PUT | Method::PATCH | Method::DELETE
        ) && request.extensions().get::<BrowserMutationProof>().is_some()
        {
            request.extensions_mut().insert(ConnectionMutationProof);
        }
    }
    next.run(request).await
}

#[cfg(test)]
fn test_router<P, B>(broker: P) -> Router
where
    P: ProviderConnectionBroker<B>,
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    inner_router(broker)
}

async fn start_connection<P, B>(
    session: Option<Extension<ConnectionSession<B>>>,
    proof: Option<Extension<ConnectionMutationProof>>,
    State(state): State<ConnectionsState<P>>,
) -> Response
where
    P: ProviderConnectionBroker<B>,
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if proof.is_none() {
        return StatusCode::FORBIDDEN.into_response();
    }
    match state.broker.start(&session).await {
        Ok(authorization_url) => Json(serde_json::json!({
            "apiVersion": CONNECTIONS_API_VERSION,
            "provider": "github",
            "authorizationUrl": authorization_url.as_str(),
        }))
        .into_response(),
        Err(ConnectionBrokerError::Unavailable) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        Err(ConnectionBrokerError::FastTrackUnavailable(stage)) => {
            fast_track_stage_response(StatusCode::SERVICE_UNAVAILABLE.into_response(), stage)
        }
        Err(
            ConnectionBrokerError::InvalidOrExpiredContinuation
            | ConnectionBrokerError::SessionMismatch,
        ) => StatusCode::BAD_GATEWAY.into_response(),
    }
}

async fn complete_connection<P, B>(
    session: Option<Extension<ConnectionSession<B>>>,
    State(state): State<ConnectionsState<P>>,
    Query(query): Query<ConnectionCallbackQuery>,
) -> Response
where
    P: ProviderConnectionBroker<B>,
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Ok(continuation) = ConnectionContinuation::new(query.continuation) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    match state.broker.complete(&session, &continuation).await {
        Ok(()) => (
            StatusCode::SEE_OTHER,
            [(header::LOCATION, "/admin/connections#github-connected")],
        )
            .into_response(),
        Err(ConnectionBrokerError::InvalidOrExpiredContinuation) => {
            StatusCode::BAD_REQUEST.into_response()
        }
        Err(ConnectionBrokerError::SessionMismatch) => StatusCode::FORBIDDEN.into_response(),
        Err(ConnectionBrokerError::Unavailable) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        Err(ConnectionBrokerError::FastTrackUnavailable(stage)) => {
            fast_track_stage_response(StatusCode::SERVICE_UNAVAILABLE.into_response(), stage)
        }
    }
}

async fn disconnect_connection<P, B>(
    session: Option<Extension<ConnectionSession<B>>>,
    proof: Option<Extension<ConnectionMutationProof>>,
    State(state): State<ConnectionsState<P>>,
    Json(request): Json<DisconnectConnectionRequest>,
) -> Response
where
    P: ProviderConnectionBroker<B>,
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if proof.is_none() {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !request.confirm {
        return StatusCode::BAD_REQUEST.into_response();
    }
    match state.broker.disconnect(&session).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(ConnectionBrokerError::Unavailable) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        Err(ConnectionBrokerError::FastTrackUnavailable(stage)) => {
            fast_track_stage_response(StatusCode::SERVICE_UNAVAILABLE.into_response(), stage)
        }
        Err(
            ConnectionBrokerError::InvalidOrExpiredContinuation
            | ConnectionBrokerError::SessionMismatch,
        ) => StatusCode::BAD_GATEWAY.into_response(),
    }
}

async fn connection_status<P, B>(
    session: Option<Extension<ConnectionSession<B>>>,
    State(state): State<ConnectionsState<P>>,
) -> Response
where
    P: ProviderConnectionBroker<B>,
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    match state.broker.status(&session).await {
        Ok(status) => Json(ConnectionStatusResponse {
            api_version: CONNECTIONS_API_VERSION,
            provider: "github",
            status,
        })
        .into_response(),
        Err(ConnectionBrokerError::Unavailable) => unavailable_status_response(),
        Err(ConnectionBrokerError::FastTrackUnavailable(stage)) => {
            fast_track_stage_response(unavailable_status_response(), stage)
        }
        Err(
            ConnectionBrokerError::InvalidOrExpiredContinuation
            | ConnectionBrokerError::SessionMismatch,
        ) => StatusCode::BAD_GATEWAY.into_response(),
    }
}

fn unavailable_status_response() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "apiVersion": CONNECTIONS_API_VERSION,
            "provider": "github",
            "status": {
                "phase": "unavailable",
                "accountEmail": null,
                "scopesRequired": [],
                "scopesGranted": [],
                "scopesMissing": [],
                "expiresAt": null
            }
        })),
    )
        .into_response()
}

fn fast_track_stage_response(mut response: Response, stage: FastTrackBffFailureStage) -> Response {
    response.headers_mut().insert(
        "x-steward-fast-track-bff-stage",
        axum::http::HeaderValue::from_static(stage.as_str()),
    );
    response
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    use super::*;

    fn router<P, B>(broker: P) -> Router
    where
        P: ProviderConnectionBroker<B>,
        B: Clone + Eq + Hash + Send + Sync + 'static,
    {
        test_router(broker)
    }

    #[derive(Clone, Eq, Hash, PartialEq)]
    struct TestSessionBinding(&'static str);

    #[derive(Default)]
    struct FakeBrokerState {
        flow: Option<(CanonicalUserId, TestSessionBinding)>,
        connected_user: Option<CanonicalUserId>,
        unavailable: bool,
        fast_track_stage: Option<FastTrackBffFailureStage>,
    }

    #[derive(Clone, Default)]
    struct FakeBroker {
        state: Arc<Mutex<FakeBrokerState>>,
    }

    impl ProviderConnectionBroker<TestSessionBinding> for FakeBroker {
        fn status<'a>(
            &'a self,
            session: &'a ConnectionSession<TestSessionBinding>,
        ) -> BoxFuture<'a, Result<ProviderConnectionStatus, ConnectionBrokerError>> {
            Box::pin(async move {
                let state = self
                    .state
                    .lock()
                    .map_err(|_| ConnectionBrokerError::Unavailable)?;
                if let Some(stage) = state.fast_track_stage {
                    return Err(ConnectionBrokerError::FastTrackUnavailable(stage));
                }
                if state.unavailable {
                    return Err(ConnectionBrokerError::Unavailable);
                }
                let connected =
                    state.connected_user.as_ref() == Some(&session.subject.canonical_user_id);
                Ok(ProviderConnectionStatus {
                    phase: if connected {
                        ConnectionPhase::Connected
                    } else {
                        ConnectionPhase::Disconnected
                    },
                    account_email: connected.then(|| "alice@example.com".to_owned()),
                    scopes_required: vec!["repo".to_owned()],
                    scopes_granted: connected.then(|| "repo".to_owned()).into_iter().collect(),
                    scopes_missing: (!connected)
                        .then(|| "repo".to_owned())
                        .into_iter()
                        .collect(),
                    expires_at: None,
                })
            })
        }

        fn start<'a>(
            &'a self,
            session: &'a ConnectionSession<TestSessionBinding>,
        ) -> BoxFuture<'a, Result<AuthorizationUrl, ConnectionBrokerError>> {
            Box::pin(async move {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| ConnectionBrokerError::Unavailable)?;
                if state.unavailable {
                    return Err(ConnectionBrokerError::Unavailable);
                }
                state.flow = Some((
                    session.subject.canonical_user_id.clone(),
                    session.binding.clone(),
                ));
                AuthorizationUrl::new(
                    "https://github.test/login/oauth/authorize?state=one-time".to_owned(),
                )
                .map_err(|_| ConnectionBrokerError::Unavailable)
            })
        }

        fn complete<'a>(
            &'a self,
            session: &'a ConnectionSession<TestSessionBinding>,
            continuation: &'a ConnectionContinuation,
        ) -> BoxFuture<'a, Result<(), ConnectionBrokerError>> {
            Box::pin(async move {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| ConnectionBrokerError::Unavailable)?;
                if state.unavailable {
                    return Err(ConnectionBrokerError::Unavailable);
                }
                if continuation.as_str() != "continuation-1" {
                    return Err(ConnectionBrokerError::InvalidOrExpiredContinuation);
                }
                let Some((user_id, binding)) = state.flow.as_ref() else {
                    return Err(ConnectionBrokerError::InvalidOrExpiredContinuation);
                };
                if user_id != &session.subject.canonical_user_id || binding != &session.binding {
                    return Err(ConnectionBrokerError::SessionMismatch);
                }
                state.flow = None;
                state.connected_user = Some(session.subject.canonical_user_id.clone());
                Ok(())
            })
        }

        fn disconnect<'a>(
            &'a self,
            session: &'a ConnectionSession<TestSessionBinding>,
        ) -> BoxFuture<'a, Result<(), ConnectionBrokerError>> {
            Box::pin(async move {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| ConnectionBrokerError::Unavailable)?;
                if state.unavailable {
                    return Err(ConnectionBrokerError::Unavailable);
                }
                if state.connected_user.as_ref() == Some(&session.subject.canonical_user_id) {
                    state.connected_user = None;
                }
                Ok(())
            })
        }
    }

    fn session() -> Result<ConnectionSession<TestSessionBinding>, String> {
        Ok(ConnectionSession {
            subject: ConnectionSubject {
                canonical_user_id: CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?,
                display_email: "alice@example.com".to_owned(),
            },
            binding: TestSessionBinding("session-a"),
        })
    }

    #[tokio::test]
    async fn unauthenticated_browser_cannot_read_connection_status() -> Result<(), String> {
        let response = router(FakeBroker::default())
            .oneshot(
                Request::builder()
                    .uri("/admin/api/v1/connections/github")
                    .body(Body::empty())
                    .map_err(|error| format!("build unauthenticated status request: {error}"))?,
            )
            .await
            .map_err(|error| format!("request unauthenticated connection status: {error}"))?;
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "connection state must not be observable without a browser session"
        );
        Ok(())
    }

    #[tokio::test]
    async fn authenticated_status_is_versioned_and_never_exposes_broker_material()
    -> Result<(), String> {
        let response = router(FakeBroker::default())
            .layer(axum::Extension(session()?))
            .oneshot(
                Request::builder()
                    .uri("/admin/api/v1/connections/github")
                    .body(Body::empty())
                    .map_err(|error| format!("build authenticated status request: {error}"))?,
            )
            .await
            .map_err(|error| format!("request authenticated connection status: {error}"))?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 16 * 1024)
            .await
            .map_err(|error| format!("read connection status body: {error}"))?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("parse connection status body: {error}"))?;
        assert_eq!(value["apiVersion"], CONNECTIONS_API_VERSION);
        assert_eq!(value["provider"], "github");
        assert_eq!(value["status"]["phase"], "disconnected");
        for forbidden in ["token", "secret", "authorization", "session-a"] {
            assert!(
                !String::from_utf8_lossy(&body)
                    .to_lowercase()
                    .contains(forbidden),
                "status response exposed forbidden broker/session material: {forbidden}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn start_requires_authenticated_session_and_mutation_proof_and_returns_only_one_time_url()
    -> Result<(), String> {
        let broker = FakeBroker::default();
        let uri = "/admin/api/v1/connections/github/start";
        let request = || {
            Request::builder()
                .method("POST")
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .map_err(|error| format!("build connection start request: {error}"))
        };

        let unauthenticated = router(broker.clone())
            .oneshot(request()?)
            .await
            .map_err(|error| format!("request unauthenticated connection start: {error}"))?;
        assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

        let missing_mutation_proof = router(broker.clone())
            .layer(axum::Extension(session()?))
            .oneshot(request()?)
            .await
            .map_err(|error| format!("request unproved connection start: {error}"))?;
        assert_eq!(missing_mutation_proof.status(), StatusCode::FORBIDDEN);

        let allowed = router(broker)
            .layer(axum::Extension(ConnectionMutationProof))
            .layer(axum::Extension(session()?))
            .oneshot(request()?)
            .await
            .map_err(|error| format!("request proved connection start: {error}"))?;
        assert_eq!(allowed.status(), StatusCode::OK);
        let body = to_bytes(allowed.into_body(), 16 * 1024)
            .await
            .map_err(|error| format!("read connection start response: {error}"))?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("parse connection start response: {error}"))?;
        assert_eq!(value["apiVersion"], CONNECTIONS_API_VERSION);
        assert_eq!(value["provider"], "github");
        assert_eq!(
            value["authorizationUrl"],
            "https://github.test/login/oauth/authorize?state=one-time"
        );
        let serialized = String::from_utf8_lossy(&body).to_lowercase();
        for forbidden in ["alice@example.com", "usr_", "session-a", "token", "secret"] {
            assert!(
                !serialized.contains(forbidden),
                "start response exposed forbidden identity/broker material: {forbidden}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn callback_continuation_is_one_time_and_bound_to_canonical_user_and_browser_session()
    -> Result<(), String> {
        let broker = FakeBroker::default();
        let start = Request::builder()
            .method("POST")
            .uri("/admin/api/v1/connections/github/start")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .map_err(|error| format!("build callback setup request: {error}"))?;
        let started = router(broker.clone())
            .layer(axum::Extension(ConnectionMutationProof))
            .layer(axum::Extension(session()?))
            .oneshot(start)
            .await
            .map_err(|error| format!("start callback setup flow: {error}"))?;
        assert_eq!(started.status(), StatusCode::OK);

        let callback = || {
            Request::builder()
                .uri("/admin/connections/github/callback?continuation=continuation-1")
                .body(Body::empty())
                .map_err(|error| format!("build connection callback request: {error}"))
        };
        let mut wrong_session = session()?;
        wrong_session.binding = TestSessionBinding("session-b");
        let rejected = router(broker.clone())
            .layer(axum::Extension(wrong_session))
            .oneshot(callback()?)
            .await
            .map_err(|error| format!("request wrong-session callback: {error}"))?;
        assert_eq!(rejected.status(), StatusCode::FORBIDDEN);

        let accepted = router(broker.clone())
            .layer(axum::Extension(session()?))
            .oneshot(callback()?)
            .await
            .map_err(|error| format!("request same-session callback: {error}"))?;
        assert_eq!(accepted.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            accepted.headers().get(header::LOCATION),
            Some(&header::HeaderValue::from_static(
                "/admin/connections#github-connected"
            )),
            "success must clear the continuation query from browser history"
        );

        let replay = router(broker)
            .layer(axum::Extension(session()?))
            .oneshot(callback()?)
            .await
            .map_err(|error| format!("replay connection callback: {error}"))?;
        assert_eq!(replay.status(), StatusCode::BAD_REQUEST);
        Ok(())
    }

    #[tokio::test]
    async fn disconnect_requires_confirmation_and_mutation_proof_and_is_idempotent()
    -> Result<(), String> {
        let broker = FakeBroker::default();
        {
            let mut state = broker
                .state
                .lock()
                .map_err(|_| "fake broker state lock was poisoned".to_owned())?;
            state.connected_user = Some(session()?.subject.canonical_user_id);
        }
        let request = |body: &'static str| {
            Request::builder()
                .method("POST")
                .uri("/admin/api/v1/connections/github/disconnect")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .map_err(|error| format!("build disconnect request: {error}"))
        };

        let unconfirmed = router(broker.clone())
            .layer(axum::Extension(ConnectionMutationProof))
            .layer(axum::Extension(session()?))
            .oneshot(request("{\"confirm\":false}")?)
            .await
            .map_err(|error| format!("request unconfirmed disconnect: {error}"))?;
        assert_eq!(unconfirmed.status(), StatusCode::BAD_REQUEST);

        let unproved = router(broker.clone())
            .layer(axum::Extension(session()?))
            .oneshot(request("{\"confirm\":true}")?)
            .await
            .map_err(|error| format!("request unproved disconnect: {error}"))?;
        assert_eq!(unproved.status(), StatusCode::FORBIDDEN);

        for attempt in ["first", "idempotent retry"] {
            let response = router(broker.clone())
                .layer(axum::Extension(ConnectionMutationProof))
                .layer(axum::Extension(session()?))
                .oneshot(request("{\"confirm\":true}")?)
                .await
                .map_err(|error| format!("request {attempt} disconnect: {error}"))?;
            assert_eq!(
                response.status(),
                StatusCode::NO_CONTENT,
                "{attempt} must converge without exposing broker material"
            );
        }

        let status = router(broker)
            .layer(axum::Extension(session()?))
            .oneshot(
                Request::builder()
                    .uri("/admin/api/v1/connections/github")
                    .body(Body::empty())
                    .map_err(|error| format!("build post-disconnect status request: {error}"))?,
            )
            .await
            .map_err(|error| format!("request post-disconnect status: {error}"))?;
        let body = to_bytes(status.into_body(), 16 * 1024)
            .await
            .map_err(|error| format!("read post-disconnect status: {error}"))?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("parse post-disconnect status: {error}"))?;
        assert_eq!(value["status"]["phase"], "disconnected");
        Ok(())
    }

    #[tokio::test]
    async fn unavailable_broker_is_explicit_and_returns_no_identity_or_credential_material()
    -> Result<(), String> {
        let broker = FakeBroker::default();
        broker
            .state
            .lock()
            .map_err(|_| "fake broker state lock was poisoned".to_owned())?
            .unavailable = true;
        let response = router(broker)
            .layer(axum::Extension(session()?))
            .oneshot(
                Request::builder()
                    .uri("/admin/api/v1/connections/github")
                    .body(Body::empty())
                    .map_err(|error| format!("build unavailable status request: {error}"))?,
            )
            .await
            .map_err(|error| format!("request unavailable connection status: {error}"))?;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), 16 * 1024)
            .await
            .map_err(|error| format!("read unavailable status: {error}"))?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("parse unavailable status: {error}"))?;
        assert_eq!(value["status"]["phase"], "unavailable");
        let serialized = String::from_utf8_lossy(&body).to_lowercase();
        for forbidden in ["alice@example.com", "usr_", "session-a", "token", "secret"] {
            assert!(!serialized.contains(forbidden));
        }
        Ok(())
    }

    #[tokio::test]
    async fn fast_track_status_failure_exposes_only_a_fixed_stage_header() -> Result<(), String> {
        let broker = FakeBroker::default();
        broker
            .state
            .lock()
            .map_err(|_| "lock fake broker")?
            .fast_track_stage = Some(FastTrackBffFailureStage::BridgeTransport);
        let response = router(broker)
            .layer(axum::Extension(session()?))
            .oneshot(
                Request::builder()
                    .uri("/admin/api/v1/connections/github")
                    .body(Body::empty())
                    .map_err(|error| format!("build diagnostic status request: {error}"))?,
            )
            .await
            .map_err(|error| format!("request diagnostic connection status: {error}"))?;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response.headers().get("x-steward-fast-track-bff-stage"),
            Some(&header::HeaderValue::from_static("bridge_transport"))
        );
        let body = to_bytes(response.into_body(), 16 * 1024)
            .await
            .map_err(|error| format!("read diagnostic status body: {error}"))?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("parse diagnostic status body: {error}"))?;
        assert_eq!(value["status"]["phase"], "unavailable");
        assert!(!String::from_utf8_lossy(&body).contains("bridge_transport"));
        Ok(())
    }
}
