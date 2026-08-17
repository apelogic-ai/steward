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
        .route("/envelopes", get(shell))
        .route("/envelopes/new", get(shell))
        .route("/envelopes/{request_id}", get(shell))
        .route("/envelopes/{request_id}/runs", get(shell))
        .route("/app", get(shell))
        .route("/app/envelopes", get(shell))
        .route("/app/envelopes/new", get(shell))
        .route("/app/envelopes/{request_id}", get(shell))
        .route("/app/runs", get(shell))
        .route("/app/assets/workspace.js", get(script))
        .route("/envelopes/assets/workspace.js", get(script))
        .route("/envelopes/assets/workspace.css", get(stylesheet))
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

async fn stylesheet() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../assets/user/workspace.css"),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::{USER_WORKSPACE_HTML, USER_WORKSPACE_JS};

    #[test]
    fn workspace_navigation_has_exact_user_routes_and_limited_preference_storage() {
        for route in [
            "/envelopes",
            "/envelopes/new",
            "/envelopes/{request_id}",
            "/envelopes/{request_id}/runs",
            "/admin/connections",
        ] {
            assert!(USER_WORKSPACE_HTML.contains(route));
        }
        for source in [USER_WORKSPACE_HTML, USER_WORKSPACE_JS] {
            assert!(!source.contains("sessionStorage"));
            assert!(!source.contains("innerHTML"));
        }
        assert!(
            USER_WORKSPACE_JS.contains("steward.ui.envelope-accordion."),
            "only the envelope accordion visibility preference may use local storage"
        );
    }

    #[test]
    fn shared_navigation_and_envelope_groups_are_explicit_and_start_collapsed() {
        for required in [
            "Envelopes",
            "Runs",
            "Connections",
            "Settings",
            "signed-in-email",
            "href=\"/envelopes\"",
            "data-accordion=\"templates\"",
            "data-accordion=\"drafts\"",
            "data-accordion=\"approved\"",
            "data-accordion=\"in-review\"",
            "Recent runs",
        ] {
            assert!(
                USER_WORKSPACE_HTML.contains(required),
                "shared envelope UI is missing {required}"
            );
        }
        assert!(
            !USER_WORKSPACE_HTML.contains("data-accordion=\"templates\" open"),
            "envelope groups must default to collapsed"
        );
    }
}
