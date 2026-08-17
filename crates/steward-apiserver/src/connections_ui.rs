//! Credential-free browser surface for user-bound provider connections.

use std::hash::Hash;

use axum::http::{StatusCode, header};
use axum::middleware;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Router};

use crate::connections::ConnectionSession;

const CONNECTIONS_HTML: &str = include_str!("../assets/connections/index.html");
const CONNECTIONS_CSS: &str = include_str!("../assets/connections/connections.css");
const CONNECTIONS_JS: &str = include_str!("../assets/connections/connections.js");

pub(crate) fn router<B, S>() -> Router<S>
where
    B: Clone + Eq + Hash + Send + Sync + 'static,
    S: Clone + Send + Sync + 'static,
{
    Router::<S>::new()
        .route("/admin/connections", get(connections_shell::<B>))
        .route("/admin/assets/connections.css", get(connections_stylesheet))
        .route("/admin/assets/connections.js", get(connections_script))
        .layer(middleware::from_fn(
            crate::admin_ui::add_browser_security_headers,
        ))
}

async fn connections_shell<B>(session: Option<Extension<ConnectionSession<B>>>) -> Response
where
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    if session.is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Html(CONNECTIONS_HTML).into_response()
}

async fn connections_stylesheet() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        CONNECTIONS_CSS,
    )
        .into_response()
}

async fn connections_script() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        CONNECTIONS_JS,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use axum::Extension;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use steward_types::CanonicalUserId;
    use tower::ServiceExt;

    use super::*;
    use crate::connections::{ConnectionSession, ConnectionSubject};

    #[derive(Clone, Eq, Hash, PartialEq)]
    struct TestBinding;

    fn session() -> Result<ConnectionSession<TestBinding>, String> {
        Ok(ConnectionSession {
            subject: ConnectionSubject {
                canonical_user_id: CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?,
                display_email: "alice@example.com".to_owned(),
            },
            binding: TestBinding,
        })
    }

    #[tokio::test]
    async fn connections_shell_requires_a_browser_session_and_serves_no_store_security_headers()
    -> Result<(), String> {
        let unauthenticated = router::<TestBinding, ()>()
            .oneshot(
                Request::builder()
                    .uri("/admin/connections")
                    .body(Body::empty())
                    .map_err(|error| format!("build unauthenticated Connections shell: {error}"))?,
            )
            .await
            .map_err(|error| format!("request unauthenticated Connections shell: {error}"))?;
        assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

        let authenticated = router::<TestBinding, ()>()
            .layer(Extension(session()?))
            .oneshot(
                Request::builder()
                    .uri("/admin/connections")
                    .body(Body::empty())
                    .map_err(|error| format!("build authenticated Connections shell: {error}"))?,
            )
            .await
            .map_err(|error| format!("request authenticated Connections shell: {error}"))?;
        assert_eq!(authenticated.status(), StatusCode::OK);
        assert_eq!(
            authenticated.headers().get(header::CACHE_CONTROL),
            Some(&header::HeaderValue::from_static("no-store"))
        );
        assert!(
            authenticated
                .headers()
                .contains_key(header::CONTENT_SECURITY_POLICY)
        );
        let body = to_bytes(authenticated.into_body(), 32 * 1024)
            .await
            .map_err(|error| format!("read Connections shell: {error}"))?;
        assert_eq!(body.as_ref(), CONNECTIONS_HTML.as_bytes());
        Ok(())
    }

    #[test]
    fn connections_assets_are_accessible_responsive_and_never_use_browser_storage_or_html_sinks() {
        for required in [
            "<main",
            "<h1",
            "aria-live=\"polite\"",
            "role=\"status\"",
            "id=\"connect-github\"",
            "id=\"disconnect-github\"",
            "<dialog",
            "id=\"scope-status\"",
            "id=\"connection-error\"",
            "aria-label=\"Steward primary navigation\"",
            "href=\"/envelopes\"",
            "href=\"/runs\"",
            "href=\"/admin/connections\" aria-current=\"page\"",
            "href=\"/settings\"",
            "id=\"signed-in-email\"",
        ] {
            assert!(
                CONNECTIONS_HTML.contains(required),
                "Connections shell is missing accessible contract {required:?}"
            );
        }
        assert!(CONNECTIONS_CSS.contains("@media (max-width:"));
        assert!(CONNECTIONS_CSS.contains("prefers-reduced-motion"));
        for required in [
            "/admin/api/v1/session",
            "/admin/api/v1/connections/github",
            "/admin/api/v1/connections/github/start",
            "/admin/api/v1/connections/github/disconnect",
            "X-Steward-CSRF",
            "textContent",
            "isAllowedAuthorizationUrl",
            "candidate.protocol === \"https:\"",
            "candidate.origin === window.location.origin",
            "candidate.pathname === \"/admin/connections/github/callback\"",
            "callbackStatus.textContent = \"GitHub connected.\"",
            "callbackStatus.hidden = true",
            "signedInEmail.textContent = value.principal.displayEmail",
        ] {
            assert!(
                CONNECTIONS_JS.contains(required),
                "Connections script is missing contract {required:?}"
            );
        }
        for forbidden in [
            "localStorage",
            "sessionStorage",
            "document.cookie",
            "innerHTML",
            "outerHTML",
            "insertAdjacentHTML",
        ] {
            assert!(
                !CONNECTIONS_JS.contains(forbidden),
                "Connections script uses forbidden browser sink/storage {forbidden}"
            );
        }
    }
}
