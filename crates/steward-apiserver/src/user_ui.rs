//! Browser-protected user workspace navigation.

use axum::Router;
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;

const USER_WORKSPACE_HTML: &str = include_str!("../assets/user/workspace.html");
const USER_WORKSPACE_JS: &str = include_str!("../assets/user/workspace.js");
#[cfg(test)]
const USER_WORKSPACE_CSS: &str = include_str!("../assets/user/workspace.css");

pub(crate) fn router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::<S>::new()
        .route("/envelopes", get(shell))
        .route("/envelopes/new", get(shell))
        .route("/envelopes/{request_id}", get(shell))
        .route("/envelopes/{request_id}/runs", get(shell))
        .route("/runs", get(shell))
        .route("/runs/{task_uid}", get(shell))
        .route("/settings", get(shell))
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
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use super::{USER_WORKSPACE_CSS, USER_WORKSPACE_HTML, USER_WORKSPACE_JS};

    #[test]
    fn workspace_navigation_has_exact_user_routes_and_limited_preference_storage() {
        for route in [
            "/envelopes",
            "/envelopes/new",
            "/envelopes/{request_id}",
            "/envelopes/{request_id}/runs",
            "/runs",
            "/runs/{task_uid}",
            "/settings",
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
        assert!(
            USER_WORKSPACE_JS.contains("document.title = PAGE_TITLES[activePage()]"),
            "each workspace route must expose its own document title"
        );
        assert!(
            USER_WORKSPACE_JS.contains("function primaryNavigationRoute()"),
            "nested envelope routes must resolve to the Envelopes primary navigation item"
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
            "href=\"/runs\"",
            "data-accordion=\"templates\"",
            "data-accordion=\"drafts\"",
            "data-accordion=\"approved\"",
            "data-accordion=\"in-review\"",
            "Recent runs",
            "Requested tools",
            "Approved tools",
            "Requested runner platform",
            "Approved runner platform",
            "Runner memory",
            "Runner compute",
            "Runner storage",
            "Canonical Steward user ID",
            "canonical-user-id",
        ] {
            assert!(
                USER_WORKSPACE_HTML.contains(required),
                "shared envelope UI is missing {required}"
            );
        }
        for non_compact in [
            "id=\"model-select\" name=\"models\" multiple",
            "id=\"tool-select\" name=\"tools\" multiple",
        ] {
            assert!(
                !USER_WORKSPACE_HTML.contains(non_compact),
                "single-choice template controls must remain compact dropdowns: {non_compact}"
            );
        }
        assert!(
            !USER_WORKSPACE_HTML.contains("data-accordion=\"templates\" open"),
            "envelope groups must default to collapsed"
        );
        for required in [
            ".envelope-group > summary::before",
            "content: \"›\"",
            ".envelope-group[open] > summary::before",
            "content: \"⌄\"",
        ] {
            assert!(
                USER_WORKSPACE_CSS.contains(required),
                "envelope accordion is missing the visible disclosure state {required}"
            );
        }
    }

    #[test]
    fn envelope_workspace_uses_neutral_groups_buttons_and_selectable_requested_authority() {
        for removed in ["Admin editable", "User only", "User and admin"] {
            assert!(
                !USER_WORKSPACE_HTML.contains(removed),
                "group visibility labels must not be rendered: {removed}"
            );
        }
        for required in [
            "class=\"button\" href=\"/envelopes/new\"",
            "id=\"model-select\"",
            "id=\"tool-select\"",
            "for=\"model-select\"",
            "for=\"tool-select\"",
        ] {
            assert!(
                USER_WORKSPACE_HTML.contains(required),
                "workspace is missing its requested-authority control {required}"
            );
        }
        for removed in [
            "No saved drafts are exposed by the authoritative envelope API.",
            "No approved envelopes.",
            "No envelopes require review.",
            "No runs are recorded for your identity.",
            "No recent runs are recorded for this envelope instance.",
        ] {
            assert!(
                !USER_WORKSPACE_JS.contains(removed),
                "empty state must be standardized instead of rendering {removed}"
            );
        }
        assert!(
            USER_WORKSPACE_JS.contains("No entries."),
            "all empty collections must use the same explicit empty state"
        );
        let new_envelope = USER_WORKSPACE_HTML
            .split("<section data-page=\"/envelopes/new\"")
            .nth(1)
            .and_then(|section| section.split("</section>").next())
            .unwrap_or("");
        assert!(!new_envelope.is_empty(), "new-envelope section must exist");
        for removed in ["Bounded request", "Back to Envelopes"] {
            assert!(
                !new_envelope.contains(removed),
                "new-envelope form must not render unnecessary chrome: {removed}"
            );
        }
    }

    #[test]
    fn provisioned_envelopes_expose_a_copyable_governed_github_actions_workflow() {
        for required in [
            "id=\"github-actions-workflow\"",
            "id=\"github-repository\"",
            "id=\"github-revision\"",
            "id=\"github-path\"",
            "id=\"generate-github-actions-workflow\"",
            "id=\"generated-github-actions-yaml\"",
            "id=\"copy-github-actions-workflow\"",
        ] {
            assert!(
                USER_WORKSPACE_HTML.contains(required),
                "the provisioned envelope detail page is missing {required}"
            );
        }
        for required in [
            "github-actions-workflow",
            "github-actions-workflow-form",
            "/github-actions-workflow",
            "navigator.clipboard.writeText",
        ] {
            assert!(
                USER_WORKSPACE_JS.contains(required),
                "the provisioned envelope workflow controller is missing {required}"
            );
        }
    }

    #[tokio::test]
    async fn settings_is_a_protected_workspace_destination() -> Result<(), String> {
        let response = super::router::<()>()
            .oneshot(
                Request::builder()
                    .uri("/settings")
                    .body(Body::empty())
                    .map_err(|error| format!("build Settings request: {error}"))?,
            )
            .await
            .map_err(|error| format!("request Settings workspace: {error}"))?;
        assert_eq!(response.status(), StatusCode::OK);
        Ok(())
    }
}
