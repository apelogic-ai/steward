//! Browser-protected user workspace navigation.

use axum::Router;
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;

const USER_WORKSPACE_HTML: &str = include_str!("../assets/user/workspace.html");
const USER_WORKSPACE_JS: &str = include_str!("../assets/user/workspace.js");

pub(crate) fn router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::<S>::new()
        .route("/app", get(shell))
        .route("/app/envelopes", get(shell))
        .route("/app/envelopes/new", get(shell))
        .route("/app/runs", get(shell))
        .route("/app/assets/workspace.js", get(script))
}

async fn shell() -> Html<&'static str> {
    Html(USER_WORKSPACE_HTML)
}

async fn script() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        USER_WORKSPACE_JS,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::{USER_WORKSPACE_HTML, USER_WORKSPACE_JS};

    #[test]
    fn workspace_navigation_has_exact_user_routes_and_no_browser_storage() {
        for route in [
            "/app/envelopes",
            "/app/envelopes/new",
            "/app/runs",
            "/admin/connections",
        ] {
            assert!(USER_WORKSPACE_HTML.contains(route));
        }
        for source in [USER_WORKSPACE_HTML, USER_WORKSPACE_JS] {
            assert!(!source.contains("localStorage"));
            assert!(!source.contains("sessionStorage"));
            assert!(!source.contains("innerHTML"));
        }
    }
}
