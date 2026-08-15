//! Credential-free browser surface for user-bound provider connections.

use std::borrow::Cow;
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
    Html(connections_document()).into_response()
}

fn connections_document() -> Cow<'static, str> {
    #[cfg(feature = "admin-demo")]
    {
        Cow::Owned(CONNECTIONS_HTML.replacen(
            "data-fast-track-runtime=\"false\"",
            "data-fast-track-runtime=\"true\"",
            1,
        ))
    }
    #[cfg(not(feature = "admin-demo"))]
    {
        Cow::Borrowed(CONNECTIONS_HTML)
    }
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
        assert_eq!(body.as_ref(), connections_document().as_bytes());
        Ok(())
    }

    #[test]
    fn connections_assets_are_accessible_responsive_and_never_use_browser_storage_or_html_sinks() {
        assert!(CONNECTIONS_HTML.contains("data-fast-track-runtime=\"false\""));
        #[cfg(feature = "admin-demo")]
        assert!(connections_document().contains("data-fast-track-runtime=\"true\""));
        #[cfg(not(feature = "admin-demo"))]
        assert!(connections_document().contains("data-fast-track-runtime=\"false\""));
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
            "id=\"runtime-status\"",
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
            "/admin/api/v1/fast-track/connections/runtime",
            "async function bootstrapRuntime()",
            "await loadSession();\n    if (fastTrackRuntimeBootstrap) {\n      const runtimePhase = await bootstrapRuntime();\n      await loadConnectionWithPreviewPolling(runtimePhase);\n    } else {\n      await loadConnection();\n    }",
            "runtimeStatus.textContent = \"Preview runtime unavailable.\"",
            "const FAST_TRACK_STATUS_POLL_INTERVAL_MS = 1000;",
            "const FAST_TRACK_STATUS_POLL_DEADLINE_MS = 90000;",
            "x-steward-fast-track-bff-stage",
            "async function loadConnectionWithPreviewPolling",
            "runtimeStatus.dataset.bffStage",
            "await loadConnectionWithPreviewPolling(runtimePhase);",
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
            "actingUser",
            "canonicalAuthority",
            "service-principal",
        ] {
            assert!(
                !CONNECTIONS_JS.contains(forbidden),
                "Connections script uses forbidden browser sink/storage {forbidden}"
            );
        }
        assert!(
            !CONNECTIONS_JS.contains("await startConnection()"),
            "Connections initialization must not start GitHub OAuth automatically"
        );
    }

    #[test]
    fn preview_readiness_requires_two_running_successes_and_resets_on_oscillation() {
        for required in [
            "let consecutiveReadyChecks = 0;",
            "runtimePhase === \"running\"",
            "consecutiveReadyChecks += 1;",
            "consecutiveReadyChecks = 0;",
            "consecutiveReadyChecks >= 2",
            "function renderPreviewChecking(runtimePhase, stage)",
            "connectGithub.disabled = true;",
            "renderPreviewChecking(runtimePhase, stage);",
        ] {
            assert!(
                CONNECTIONS_JS.contains(required),
                "preview readiness is missing hysteresis contract {required:?}"
            );
        }

        assert!(
            CONNECTIONS_JS
                .contains("if (consecutiveReadyChecks >= 2) {\n        renderConnection(status);"),
            "only the stable-ready branch may render the actionable connection state"
        );
        assert!(
            CONNECTIONS_JS.contains(
                "renderPreviewChecking(runtimePhase, null);\n    } catch (error) {\n      consecutiveReadyChecks = 0;"
            ),
            "an oscillating failure must reset readiness after keeping the UI non-actionable"
        );
    }
}
