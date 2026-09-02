//! User-bound provider connection status and consent BFF.

use std::hash::Hash;

use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use steward_types::CanonicalUserId;

use crate::BoxFuture;
use crate::browser_auth::{
    BrowserAuthService, BrowserMutationProof, BrowserMutationRequest, BrowserSessionBinding,
    BrowserSessionContext, protect_browser_routes,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionPhase {
    Disconnected,
    Connecting,
    Connected,
    ReauthRequired,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConnectionStatus {
    pub phase: ConnectionPhase,
    pub account_email: Option<String>,
    pub scopes_required: Vec<String>,
    pub scopes_granted: Vec<String>,
    pub scopes_missing: Vec<String>,
    pub expires_at: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConnectionStatusResponse {
    api_version: &'static str,
    provider: &'static str,
    status: ProviderConnectionStatus,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DisconnectConnectionRequest {
    confirm: bool,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StartConnectionResponse {
    api_version: &'static str,
    provider: &'static str,
    /// One-time HTTPS destination. It must not be persisted or logged by clients.
    authorization_url: String,
    /// Conservative expiry for MCP-GW's pinned OAuth state lifetime plus clock skew.
    expires_at: String,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConnectionOperationErrorResponse {
    api_version: &'static str,
    error: &'static str,
}

/// One-time browser destination. It intentionally implements neither `Debug` nor `Display`.
pub struct AuthorizationUrl(String);

impl AuthorizationUrl {
    pub fn new(value: String) -> Result<Self, &'static str> {
        if value.len() > 4096 || value.bytes().any(|byte| byte.is_ascii_control()) {
            return Err("provider authorization URL is not a bounded URL scalar");
        }
        let parsed = Url::parse(&value)
            .map_err(|_| "provider authorization URL must be an absolute HTTPS URL")?;
        if parsed.scheme() != "https"
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err("provider authorization URL must be an absolute HTTPS URL");
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

pub struct StartedConnection {
    pub authorization_url: AuthorizationUrl,
    pub expires_at: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionBrokerError {
    OAuthFlowPending,
    Unavailable,
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
    ) -> BoxFuture<'a, Result<StartedConnection, ConnectionBrokerError>>;

    fn disconnect<'a>(
        &'a self,
        session: &'a ConnectionSession<B>,
    ) -> BoxFuture<'a, Result<(), ConnectionBrokerError>>;
}

#[derive(Clone)]
pub(crate) struct ConnectionsState<P> {
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
            "/admin/api/v1/connections/github/disconnect",
            post(disconnect_connection::<P, B>),
        )
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
pub(crate) fn test_router<P, B>(broker: P) -> Router
where
    P: ProviderConnectionBroker<B>,
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    inner_router(broker)
}

#[utoipa::path(
    post,
    path = "/admin/api/v1/connections/github/start",
    params(("X-Steward-CSRF" = String, Header)),
    request_body = BrowserMutationRequest,
    responses(
        (status = 200, body = StartConnectionResponse),
        (status = 400, description = "Mutation JSON is malformed"),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Origin, fetch metadata, or CSRF proof is invalid"),
        (status = 502, description = "Provider continuation is invalid"),
        (status = 503, description = "Connection broker is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn start_connection<P, B>(
    session: Option<Extension<ConnectionSession<B>>>,
    proof: Option<Extension<ConnectionMutationProof>>,
    State(state): State<ConnectionsState<P>>,
    Json(_request): Json<BrowserMutationRequest>,
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
        Ok(started) => Json(StartConnectionResponse {
            api_version: CONNECTIONS_API_VERSION,
            provider: "github",
            authorization_url: started.authorization_url.as_str().to_owned(),
            expires_at: started.expires_at,
        })
        .into_response(),
        Err(ConnectionBrokerError::OAuthFlowPending | ConnectionBrokerError::Unavailable) => {
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

#[utoipa::path(
    post,
    path = "/admin/api/v1/connections/github/disconnect",
    params(("X-Steward-CSRF" = String, Header)),
    request_body = DisconnectConnectionRequest,
    responses(
        (status = 204, description = "Connection was disconnected"),
        (status = 400, description = "Explicit confirmation is required"),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Origin, fetch metadata, or CSRF proof is invalid"),
        (status = 409, body = ConnectionOperationErrorResponse, description = "An OAuth flow is still pending"),
        (status = 502, description = "Provider state is invalid"),
        (status = 503, description = "Connection broker is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn disconnect_connection<P, B>(
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
        Err(ConnectionBrokerError::OAuthFlowPending) => oauth_flow_pending_response(),
        Err(ConnectionBrokerError::Unavailable) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

#[utoipa::path(
    get,
    path = "/admin/api/v1/connections/github",
    responses(
        (status = 200, body = ConnectionStatusResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 502, description = "Provider state is invalid"),
        (status = 503, body = ConnectionStatusResponse)
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn connection_status<P, B>(
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
        Err(ConnectionBrokerError::OAuthFlowPending | ConnectionBrokerError::Unavailable) => {
            unavailable_status_response()
        }
    }
}

fn oauth_flow_pending_response() -> Response {
    (
        StatusCode::CONFLICT,
        Json(ConnectionOperationErrorResponse {
            api_version: CONNECTIONS_API_VERSION,
            error: "oauth_flow_pending",
        }),
    )
        .into_response()
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    use super::*;

    #[test]
    fn authorization_url_is_a_bounded_absolute_https_destination_without_userinfo() {
        assert!(
            AuthorizationUrl::new(
                "https://github.test/login/oauth/authorize?state=one-time".to_owned()
            )
            .is_ok()
        );
        for invalid in [
            "https://".to_owned(),
            "https://alice@github.test/login/oauth/authorize".to_owned(),
            [
                "https://alice:",
                "not-a-secret",
                "@github.test/login/oauth/authorize",
            ]
            .concat(),
            "https://github.test/login/oauth/authorize\nX-Header: injected".to_owned(),
        ] {
            assert!(
                AuthorizationUrl::new(invalid.clone()).is_err(),
                "malformed authorization destination must fail closed: {invalid:?}"
            );
        }
        assert!(
            AuthorizationUrl::new(format!("https://github.test/{}", "a".repeat(4096))).is_err(),
            "authorization continuation must have a strict size bound"
        );
    }

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
        oauth_pending: bool,
        unavailable: bool,
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
        ) -> BoxFuture<'a, Result<StartedConnection, ConnectionBrokerError>> {
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
                let authorization_url = AuthorizationUrl::new(
                    "https://github.test/login/oauth/authorize?state=one-time".to_owned(),
                )
                .map_err(|_| ConnectionBrokerError::Unavailable)?;
                Ok(StartedConnection {
                    authorization_url,
                    expires_at: "2026-09-01T12:10:30Z".to_owned(),
                })
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
                if state.oauth_pending {
                    return Err(ConnectionBrokerError::OAuthFlowPending);
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
        let request = |body: &'static str| {
            Request::builder()
                .method("POST")
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .map_err(|error| format!("build connection start request: {error}"))
        };

        let unauthenticated = router(broker.clone())
            .oneshot(request("{}")?)
            .await
            .map_err(|error| format!("request unauthenticated connection start: {error}"))?;
        assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

        let missing_mutation_proof = router(broker.clone())
            .layer(axum::Extension(session()?))
            .oneshot(request("{}")?)
            .await
            .map_err(|error| format!("request unproved connection start: {error}"))?;
        assert_eq!(missing_mutation_proof.status(), StatusCode::FORBIDDEN);

        let hostile_runtime_fields = router(broker.clone())
            .layer(axum::Extension(ConnectionMutationProof))
            .layer(axum::Extension(session()?))
            .oneshot(request(
                r#"{"userId":"usr_hostile","issuer":"https://other.example.test","bearer":"hostile","runtime":"adopted","image":"hostile:latest","command":["sh"],"endpoint":"https://other.example.test","toolGrant":{"provider":"github","resource":"repository","action":"get_file_contents"}}"#,
            )?)
            .await
            .map_err(|error| format!("request browser-supplied runtime fields: {error}"))?;
        assert_eq!(
            hostile_runtime_fields.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "the browser may select only the fixed provider-control operation"
        );

        let allowed = router(broker)
            .layer(axum::Extension(ConnectionMutationProof))
            .layer(axum::Extension(session()?))
            .oneshot(request("{}")?)
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
    async fn steward_callback_route_is_unreachable_because_mcp_gw_owns_oauth_completion()
    -> Result<(), String> {
        let broker = FakeBroker::default();
        let response = router(broker)
            .layer(axum::Extension(session()?))
            .oneshot(
                Request::builder()
                    .uri("/admin/connections/github/callback?continuation=hostile")
                    .body(Body::empty())
                    .map_err(|error| format!("build obsolete callback request: {error}"))?,
            )
            .await
            .map_err(|error| format!("request obsolete callback route: {error}"))?;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
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
    async fn disconnect_reports_a_bounded_pending_oauth_conflict() -> Result<(), String> {
        let broker = FakeBroker::default();
        broker
            .state
            .lock()
            .map_err(|_| "fake broker state lock was poisoned".to_owned())?
            .oauth_pending = true;
        let response = router(broker)
            .layer(axum::Extension(ConnectionMutationProof))
            .layer(axum::Extension(session()?))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/api/v1/connections/github/disconnect")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{\"confirm\":true}"))
                    .map_err(|error| format!("build pending-flow disconnect request: {error}"))?,
            )
            .await
            .map_err(|error| format!("request pending-flow disconnect: {error}"))?;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = to_bytes(response.into_body(), 1024)
            .await
            .map_err(|error| format!("read pending-flow response: {error}"))?;
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body)
                .map_err(|error| format!("parse pending-flow response: {error}"))?,
            serde_json::json!({
                "apiVersion": CONNECTIONS_API_VERSION,
                "error": "oauth_flow_pending"
            })
        );
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
}
