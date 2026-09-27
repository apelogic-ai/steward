//! Canonical-user browser preferences shared across devices.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use steward_store::{BrowserPreferencesRecord, PgStore, StoreError};

use crate::browser_auth::{
    BrowserAuthService, BrowserMutationProof, BrowserSessionContext, protect_browser_routes,
};

const PREFERENCES_API_VERSION: &str = "steward.preferences/v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BrowserTheme {
    Light,
    Dark,
    System,
}

impl BrowserTheme {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
            Self::System => "system",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            "system" => Some(Self::System),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateBrowserPreferences {
    onboarding_dismissed: Option<bool>,
    workflow_acknowledged: Option<bool>,
    theme: Option<BrowserTheme>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BrowserPreferencesView {
    api_version: &'static str,
    revision: i64,
    onboarding_dismissed: bool,
    workflow_acknowledged: bool,
    theme: Option<BrowserTheme>,
}

fn view(record: BrowserPreferencesRecord) -> Result<BrowserPreferencesView, StoreError> {
    let theme = match record.theme.as_deref() {
        Some(value) => {
            Some(BrowserTheme::parse(value).ok_or(StoreError::InvalidBrowserPreferences)?)
        }
        None => None,
    };
    Ok(BrowserPreferencesView {
        api_version: PREFERENCES_API_VERSION,
        revision: record.revision,
        onboarding_dismissed: record.onboarding_dismissed,
        workflow_acknowledged: record.workflow_acknowledged,
        theme,
    })
}

pub fn protected_router(store: PgStore, browser_auth: BrowserAuthService) -> Router {
    protect_browser_routes(
        Router::new()
            .route(
                "/app/api/v1/preferences",
                get(get_preferences).put(update_preferences),
            )
            .with_state(store),
        browser_auth,
    )
}

#[utoipa::path(
    get,
    operation_id = "getBrowserPreferences",
    path = "/app/api/v1/preferences",
    responses(
        (status = 200, body = BrowserPreferencesView),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 503, description = "Preferences are unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn get_preferences(
    session: Option<Extension<BrowserSessionContext>>,
    State(store): State<PgStore>,
) -> Response {
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    match store
        .browser_preferences(&session.principal.canonical_user_id)
        .await
        .and_then(view)
    {
        Ok(preferences) => Json(preferences).into_response(),
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

#[utoipa::path(
    put,
    operation_id = "updateBrowserPreferences",
    path = "/app/api/v1/preferences",
    params(("X-Steward-CSRF" = String, Header)),
    request_body = UpdateBrowserPreferences,
    responses(
        (status = 200, body = BrowserPreferencesView),
        (status = 400, description = "At least one preference must be supplied"),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Origin, fetch metadata, or CSRF proof is invalid"),
        (status = 503, description = "Preferences are unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn update_preferences(
    session: Option<Extension<BrowserSessionContext>>,
    proof: Option<Extension<BrowserMutationProof>>,
    State(store): State<PgStore>,
    Json(request): Json<UpdateBrowserPreferences>,
) -> Response {
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if proof.is_none() {
        return StatusCode::FORBIDDEN.into_response();
    }
    if request.onboarding_dismissed.is_none()
        && request.workflow_acknowledged.is_none()
        && request.theme.is_none()
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    match store
        .write_browser_preferences(
            &session.principal.canonical_user_id,
            request.onboarding_dismissed,
            request.workflow_acknowledged,
            request.theme.map(|theme| Some(theme.as_str())),
            session.principal.canonical_user_id.as_str(),
        )
        .await
        .and_then(view)
    {
        Ok(preferences) => Json(preferences).into_response(),
        Err(StoreError::InvalidBrowserPreferences) => StatusCode::BAD_REQUEST.into_response(),
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}
