//! Browser-facing inference credential custody without exposing credential material.

use std::hash::Hash;

use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use steward_store::{
    ManagedInferenceCredentialStatus, ManagedInferenceKeyCipher, PgStore, StoreError,
};
use steward_types::{CanonicalUserId, InferenceMode};
use zeroize::Zeroize;

use crate::BoxFuture;
use crate::browser_auth::{BrowserAuthService, BrowserSessionBinding, protect_browser_routes};
use crate::connections::{ConnectionMutationProof, ConnectionSession, adapt_browser_context};

pub const INFERENCE_CONNECTIONS_API_VERSION: &str = "steward.inference-connections/v1";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InferenceConnectionResponse {
    api_version: &'static str,
    mode: InferenceMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    credential: Option<InferenceCredentialView>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InferenceCredentialView {
    last_four: String,
    saved_at: String,
}

impl From<ManagedInferenceCredentialStatus> for InferenceCredentialView {
    fn from(status: ManagedInferenceCredentialStatus) -> Self {
        Self {
            last_four: status.last_four,
            saved_at: status.saved_at,
        }
    }
}

/// Secret-bearing request. It deliberately implements neither `Debug` nor `Serialize`.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SaveInferenceCredentialRequest {
    #[schema(min_length = 4, max_length = 8192)]
    api_key: String,
}

impl Drop for SaveInferenceCredentialRequest {
    fn drop(&mut self) {
        self.api_key.zeroize();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InferenceConnectionError {
    ManagedModeRequired,
    InvalidCredential,
    Unavailable,
}

pub trait InferenceConnectionBroker: Clone + Send + Sync + 'static {
    fn mode(&self) -> InferenceMode;

    fn status<'a>(
        &'a self,
        user_id: &'a CanonicalUserId,
    ) -> BoxFuture<'a, Result<Option<ManagedInferenceCredentialStatus>, InferenceConnectionError>>;

    fn save<'a>(
        &'a self,
        user_id: &'a CanonicalUserId,
        api_key: &'a str,
    ) -> BoxFuture<'a, Result<ManagedInferenceCredentialStatus, InferenceConnectionError>>;

    fn remove<'a>(
        &'a self,
        user_id: &'a CanonicalUserId,
    ) -> BoxFuture<'a, Result<bool, InferenceConnectionError>>;
}

#[derive(Clone)]
pub struct PgInferenceConnectionBroker {
    store: PgStore,
    mode: InferenceMode,
    cipher: Option<ManagedInferenceKeyCipher>,
}

impl PgInferenceConnectionBroker {
    pub fn new(
        store: PgStore,
        mode: InferenceMode,
        cipher: Option<ManagedInferenceKeyCipher>,
    ) -> Result<Self, &'static str> {
        if mode == InferenceMode::Managed && cipher.is_none() {
            return Err("managed inference mode requires a deployment encryption key");
        }
        if mode == InferenceMode::Stock && cipher.is_some() {
            return Err("stock inference mode must not load a managed credential encryption key");
        }
        Ok(Self {
            store,
            mode,
            cipher,
        })
    }

    fn cipher(&self) -> Result<&ManagedInferenceKeyCipher, InferenceConnectionError> {
        self.cipher
            .as_ref()
            .filter(|_| self.mode == InferenceMode::Managed)
            .ok_or(InferenceConnectionError::ManagedModeRequired)
    }
}

fn map_store_error(error: StoreError) -> InferenceConnectionError {
    match error {
        StoreError::InvalidManagedInferenceCredential => {
            InferenceConnectionError::InvalidCredential
        }
        _ => InferenceConnectionError::Unavailable,
    }
}

impl InferenceConnectionBroker for PgInferenceConnectionBroker {
    fn mode(&self) -> InferenceMode {
        self.mode
    }

    fn status<'a>(
        &'a self,
        user_id: &'a CanonicalUserId,
    ) -> BoxFuture<'a, Result<Option<ManagedInferenceCredentialStatus>, InferenceConnectionError>>
    {
        Box::pin(async move {
            self.store
                .managed_inference_credential_status(user_id)
                .await
                .map_err(map_store_error)
        })
    }

    fn save<'a>(
        &'a self,
        user_id: &'a CanonicalUserId,
        api_key: &'a str,
    ) -> BoxFuture<'a, Result<ManagedInferenceCredentialStatus, InferenceConnectionError>> {
        Box::pin(async move {
            let cipher = self.cipher()?;
            self.store
                .put_managed_inference_credential(user_id, api_key, user_id.as_str(), cipher)
                .await
                .map_err(map_store_error)
        })
    }

    fn remove<'a>(
        &'a self,
        user_id: &'a CanonicalUserId,
    ) -> BoxFuture<'a, Result<bool, InferenceConnectionError>> {
        Box::pin(async move {
            self.store
                .remove_managed_inference_credential(user_id, user_id.as_str())
                .await
                .map_err(map_store_error)
        })
    }
}

#[derive(Clone)]
pub(crate) struct InferenceConnectionsState<B> {
    broker: B,
}

fn inner_router<B, S>(broker: B) -> Router
where
    B: InferenceConnectionBroker,
    S: Clone + Eq + Hash + Send + Sync + 'static,
{
    Router::new()
        .route(
            "/app/api/v1/connections/inference",
            get(get_inference_connection::<B, S>)
                .put(save_inference_credential::<B, S>)
                .delete(remove_inference_credential::<B, S>),
        )
        .with_state(InferenceConnectionsState { broker })
}

pub fn protected_router<B>(broker: B, browser_auth: BrowserAuthService) -> Router
where
    B: InferenceConnectionBroker,
{
    let routes = inner_router::<B, BrowserSessionBinding>(broker)
        .route_layer(middleware::from_fn(adapt_browser_context));
    protect_browser_routes(routes, browser_auth)
}

fn response(
    mode: InferenceMode,
    status: Option<ManagedInferenceCredentialStatus>,
) -> InferenceConnectionResponse {
    InferenceConnectionResponse {
        api_version: INFERENCE_CONNECTIONS_API_VERSION,
        mode,
        credential: status.map(Into::into),
    }
}

fn error_response(error: InferenceConnectionError) -> Response {
    let (status, code, message) = match error {
        InferenceConnectionError::ManagedModeRequired => (
            StatusCode::CONFLICT,
            "managed_inference_disabled",
            "This Steward deployment uses administrator-managed inference.",
        ),
        InferenceConnectionError::InvalidCredential => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_inference_key",
            "The API key must be a bounded opaque bearer token.",
        ),
        InferenceConnectionError::Unavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "inference_connection_unavailable",
            "The inference credential service is unavailable.",
        ),
    };
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({"error": code, "message": message})),
    )
        .into_response()
}

#[utoipa::path(
    get,
    operation_id = "getInferenceConnection",
    path = "/app/api/v1/connections/inference",
    responses(
        (status = 200, body = InferenceConnectionResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 503, description = "Credential metadata is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn get_inference_connection<B, S>(
    session: Option<Extension<ConnectionSession<S>>>,
    State(state): State<InferenceConnectionsState<B>>,
) -> Response
where
    B: InferenceConnectionBroker,
    S: Clone + Eq + Hash + Send + Sync + 'static,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    match state
        .broker
        .status(&session.subject.canonical_user_id)
        .await
    {
        Ok(status) => (
            [(header::CACHE_CONTROL, "no-store")],
            Json(response(state.broker.mode(), status)),
        )
            .into_response(),
        Err(error) => error_response(error),
    }
}

#[utoipa::path(
    put,
    operation_id = "saveInferenceCredential",
    path = "/app/api/v1/connections/inference",
    params(("X-Steward-CSRF" = String, Header)),
    request_body = SaveInferenceCredentialRequest,
    responses(
        (status = 200, body = InferenceConnectionResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Origin, fetch metadata, or CSRF proof is invalid"),
        (status = 409, description = "Managed inference mode is not enabled"),
        (status = 422, description = "Credential is invalid"),
        (status = 503, description = "Credential storage is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn save_inference_credential<B, S>(
    session: Option<Extension<ConnectionSession<S>>>,
    proof: Option<Extension<ConnectionMutationProof>>,
    State(state): State<InferenceConnectionsState<B>>,
    Json(request): Json<SaveInferenceCredentialRequest>,
) -> Response
where
    B: InferenceConnectionBroker,
    S: Clone + Eq + Hash + Send + Sync + 'static,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if proof.is_none() {
        return StatusCode::FORBIDDEN.into_response();
    }
    match state
        .broker
        .save(&session.subject.canonical_user_id, &request.api_key)
        .await
    {
        Ok(status) => (
            [(header::CACHE_CONTROL, "no-store")],
            Json(response(state.broker.mode(), Some(status))),
        )
            .into_response(),
        Err(error) => error_response(error),
    }
}

#[utoipa::path(
    delete,
    operation_id = "removeInferenceCredential",
    path = "/app/api/v1/connections/inference",
    params(("X-Steward-CSRF" = String, Header)),
    responses(
        (status = 204, description = "Steward's credential copy was removed"),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Origin, fetch metadata, or CSRF proof is invalid"),
        (status = 409, description = "Managed inference mode is not enabled"),
        (status = 503, description = "Credential storage is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn remove_inference_credential<B, S>(
    session: Option<Extension<ConnectionSession<S>>>,
    proof: Option<Extension<ConnectionMutationProof>>,
    State(state): State<InferenceConnectionsState<B>>,
) -> Response
where
    B: InferenceConnectionBroker,
    S: Clone + Eq + Hash + Send + Sync + 'static,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if proof.is_none() {
        return StatusCode::FORBIDDEN.into_response();
    }
    match state
        .broker
        .remove(&session.subject.canonical_user_id)
        .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => error_response(error),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::body::to_bytes;

    use super::*;
    use crate::connections::ConnectionSubject;

    #[derive(Clone)]
    struct FakeBroker {
        mode: InferenceMode,
        status: Option<ManagedInferenceCredentialStatus>,
        saved: Arc<Mutex<Option<String>>>,
    }

    impl InferenceConnectionBroker for FakeBroker {
        fn mode(&self) -> InferenceMode {
            self.mode
        }

        fn status<'a>(
            &'a self,
            _user_id: &'a CanonicalUserId,
        ) -> BoxFuture<'a, Result<Option<ManagedInferenceCredentialStatus>, InferenceConnectionError>>
        {
            Box::pin(async move { Ok(self.status.clone()) })
        }

        fn save<'a>(
            &'a self,
            _user_id: &'a CanonicalUserId,
            api_key: &'a str,
        ) -> BoxFuture<'a, Result<ManagedInferenceCredentialStatus, InferenceConnectionError>>
        {
            Box::pin(async move {
                if self.mode != InferenceMode::Managed {
                    return Err(InferenceConnectionError::ManagedModeRequired);
                }
                *self
                    .saved
                    .lock()
                    .map_err(|_| InferenceConnectionError::Unavailable)? = Some(api_key.to_owned());
                Ok(ManagedInferenceCredentialStatus {
                    last_four: "wxyz".to_owned(),
                    saved_at: "2026-10-08T12:00:00.000000Z".to_owned(),
                })
            })
        }

        fn remove<'a>(
            &'a self,
            _user_id: &'a CanonicalUserId,
        ) -> BoxFuture<'a, Result<bool, InferenceConnectionError>> {
            Box::pin(async move {
                Ok(self
                    .saved
                    .lock()
                    .ok()
                    .and_then(|mut key| key.take())
                    .is_some())
            })
        }
    }

    fn session() -> Result<ConnectionSession<String>, String> {
        Ok(ConnectionSession {
            subject: ConnectionSubject {
                canonical_user_id: CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?,
                display_email: "alice@example.com".to_owned(),
            },
            binding: "session-a".to_owned(),
        })
    }

    #[tokio::test]
    async fn managed_save_returns_only_bounded_metadata() -> Result<(), String> {
        let saved = Arc::new(Mutex::new(None));
        let broker = FakeBroker {
            mode: InferenceMode::Managed,
            status: None,
            saved: saved.clone(),
        };
        let response = save_inference_credential(
            Some(Extension(session()?)),
            Some(Extension(ConnectionMutationProof)),
            State(InferenceConnectionsState { broker }),
            Json(SaveInferenceCredentialRequest {
                api_key: "fixture-managed-credential-wxyz".to_owned(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024)
            .await
            .map_err(|error| format!("read inference response: {error}"))?;
        let body = String::from_utf8(body.to_vec())
            .map_err(|error| format!("decode inference response: {error}"))?;
        assert!(body.contains("wxyz"));
        assert!(!body.contains("fixture-managed-credential"));
        assert_eq!(
            saved
                .lock()
                .map_err(|_| "fake credential lock was poisoned")?
                .as_deref(),
            Some("fixture-managed-credential-wxyz")
        );
        Ok(())
    }

    #[tokio::test]
    async fn stock_mode_is_read_only() -> Result<(), String> {
        let broker = FakeBroker {
            mode: InferenceMode::Stock,
            status: None,
            saved: Arc::new(Mutex::new(None)),
        };
        let response = save_inference_credential(
            Some(Extension(session()?)),
            Some(Extension(ConnectionMutationProof)),
            State(InferenceConnectionsState { broker }),
            Json(SaveInferenceCredentialRequest {
                api_key: "fixture-stock-credential-wxyz".to_owned(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        Ok(())
    }

    #[tokio::test]
    async fn stock_mode_still_allows_destroying_a_stored_managed_key() -> Result<(), String> {
        let saved = Arc::new(Mutex::new(Some(
            "fixture-managed-credential-wxyz".to_owned(),
        )));
        let broker = FakeBroker {
            mode: InferenceMode::Stock,
            status: None,
            saved: saved.clone(),
        };
        let response = remove_inference_credential(
            Some(Extension(session()?)),
            Some(Extension(ConnectionMutationProof)),
            State(InferenceConnectionsState { broker }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(
            saved
                .lock()
                .map_err(|_| "fake credential lock was poisoned")?
                .is_none(),
            "rolling back to stock mode must not strand encrypted user credentials"
        );
        Ok(())
    }

    #[tokio::test]
    async fn stock_mode_reports_stored_key_metadata_for_ui_cleanup() -> Result<(), String> {
        let broker = FakeBroker {
            mode: InferenceMode::Stock,
            status: Some(ManagedInferenceCredentialStatus {
                last_four: "wxyz".to_owned(),
                saved_at: "2026-10-09T00:00:00Z".to_owned(),
            }),
            saved: Arc::new(Mutex::new(Some(
                "fixture-managed-credential-wxyz".to_owned(),
            ))),
        };
        let response = get_inference_connection::<FakeBroker, String>(
            Some(Extension(session()?)),
            State(InferenceConnectionsState { broker }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024)
            .await
            .map_err(|error| format!("read stock inference response: {error}"))?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("decode stock inference response: {error}"))?;
        assert_eq!(value["mode"], "stock");
        assert_eq!(value["credential"]["lastFour"], "wxyz");
        Ok(())
    }
}
