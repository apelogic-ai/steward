//! Opt-in, loopback-only dashboard harness for human localhost acceptance.

use std::net::SocketAddr;
use std::str::FromStr;

use axum::Router;
use axum::extract::Request;
use axum::http::{HeaderValue, header};
use axum::middleware::{self, Next};
use axum::response::Response;

use crate::{
    AuthenticatedCaller, AuthenticationError, BoxFuture, RequestAuthenticator, admin_ui,
    browser_auth::{
        BrowserSessionBinding, LocalFakeIdentity, browser_auth_router,
        local_fake_browser_auth_service,
    },
    connections,
    connections_demo::LocalConnectionsBroker,
    protect_admin_routes,
};

const LOCAL_DEMO_BEARER: &str = "steward-local-demo";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdminDashboardDemoMode {
    Authenticated,
    Unauthenticated,
    OidcUser,
    OidcAdmin,
}

impl FromStr for AdminDashboardDemoMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "authenticated" => Ok(Self::Authenticated),
            "unauthenticated" => Ok(Self::Unauthenticated),
            "oidc-user" => Ok(Self::OidcUser),
            "oidc-admin" => Ok(Self::OidcAdmin),
            _ => Err(
                "demo mode must be authenticated, unauthenticated, oidc-user, or oidc-admin"
                    .to_owned(),
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdminDashboardDemoConfig {
    bind: SocketAddr,
    mode: AdminDashboardDemoMode,
}

impl AdminDashboardDemoConfig {
    pub fn new(mode: AdminDashboardDemoMode, bind: SocketAddr) -> Result<Self, String> {
        if !bind.ip().is_loopback() {
            return Err("localhost dashboard demo bind must be loopback".to_owned());
        }
        Ok(Self { bind, mode })
    }

    pub fn bind(self) -> SocketAddr {
        self.bind
    }

    pub fn mode(self) -> AdminDashboardDemoMode {
        self.mode
    }
}

#[derive(Clone, Copy)]
struct DemoAuthenticator;

impl RequestAuthenticator for DemoAuthenticator {
    fn authenticate<'a>(
        &'a self,
        bearer_token: &'a str,
    ) -> BoxFuture<'a, Result<AuthenticatedCaller, AuthenticationError>> {
        Box::pin(async move {
            if bearer_token != LOCAL_DEMO_BEARER {
                return Err(AuthenticationError::InvalidCredentials);
            }
            Ok(AuthenticatedCaller {
                actor: "admin@example.com".to_owned(),
                member_roles: Vec::new(),
                is_admin: true,
                can_bootstrap_steward_run_service_envelope: false,
            })
        })
    }
}

async fn inject_local_demo_bearer(mut request: Request, next: Next) -> Response {
    request.headers_mut().insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer steward-local-demo"),
    );
    next.run(request).await
}

pub fn router(mode: AdminDashboardDemoMode, origin: &str) -> Result<Router, String> {
    let protected = protect_admin_routes(admin_ui::router::<()>(), DemoAuthenticator);
    Ok(match mode {
        AdminDashboardDemoMode::Authenticated => {
            protected.layer(middleware::from_fn(inject_local_demo_bearer))
        }
        AdminDashboardDemoMode::Unauthenticated => protected,
        AdminDashboardDemoMode::OidcUser => {
            oidc_connections_router(origin, LocalFakeIdentity::User)?
        }
        AdminDashboardDemoMode::OidcAdmin => {
            oidc_connections_router(origin, LocalFakeIdentity::Admin)?
        }
    })
}

fn oidc_connections_router(origin: &str, identity: LocalFakeIdentity) -> Result<Router, String> {
    let bind = origin
        .strip_prefix("http://")
        .ok_or_else(|| "localhost OIDC demo origin must use explicit loopback HTTP".to_owned())?
        .parse::<SocketAddr>()
        .map_err(|_| "localhost OIDC demo origin must contain a socket address".to_owned())?;
    let browser_auth = local_fake_browser_auth_service(origin, identity)?;
    let broker = LocalConnectionsBroker::<BrowserSessionBinding>::new(bind)?;
    Ok(browser_auth_router(browser_auth.clone())
        .merge(connections::protected_router(broker, browser_auth)))
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    use super::{AdminDashboardDemoConfig, AdminDashboardDemoMode, router};

    const TEST_ORIGIN: &str = "http://127.0.0.1:33002";

    fn cookie_pair(response: &axum::response::Response, name: &str) -> Result<String, String> {
        response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find(|value| value.starts_with(&format!("{name}=")))
            .and_then(|value| value.split(';').next())
            .map(str::to_owned)
            .ok_or_else(|| format!("response omitted {name} cookie"))
    }

    #[test]
    fn demo_bind_is_loopback_only() {
        for address in [
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 0),
        ] {
            assert!(
                AdminDashboardDemoConfig::new(AdminDashboardDemoMode::Authenticated, address,)
                    .is_ok(),
                "a loopback address must be accepted"
            );
        }
        for address in [
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 3000),
            SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 3000),
        ] {
            assert!(
                AdminDashboardDemoConfig::new(AdminDashboardDemoMode::Authenticated, address,)
                    .is_err(),
                "a non-loopback bind must fail closed"
            );
        }
    }

    #[tokio::test]
    async fn authenticated_mode_exercises_real_assets_auth_and_security_headers()
    -> Result<(), String> {
        let shell = router(AdminDashboardDemoMode::Authenticated, TEST_ORIGIN)?
            .oneshot(
                Request::builder()
                    .uri("/admin")
                    .body(Body::empty())
                    .map_err(|error| format!("build shell request: {error}"))?,
            )
            .await
            .map_err(|error| format!("request authenticated shell: {error}"))?;
        assert_eq!(shell.status(), StatusCode::OK);
        assert_eq!(
            shell.headers().get(header::CACHE_CONTROL),
            Some(&header::HeaderValue::from_static("no-store"))
        );
        assert!(
            shell
                .headers()
                .contains_key(header::CONTENT_SECURITY_POLICY),
            "the real browser security middleware must wrap the demo shell"
        );

        let bootstrap = router(AdminDashboardDemoMode::Authenticated, TEST_ORIGIN)?
            .oneshot(
                Request::builder()
                    .uri("/admin/api/v1/bootstrap")
                    .body(Body::empty())
                    .map_err(|error| format!("build bootstrap request: {error}"))?,
            )
            .await
            .map_err(|error| format!("request authenticated bootstrap: {error}"))?;
        assert_eq!(bootstrap.status(), StatusCode::OK);
        let body = to_bytes(bootstrap.into_body(), 16 * 1024)
            .await
            .map_err(|error| format!("read bootstrap body: {error}"))?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("parse bootstrap body: {error}"))?;
        assert_eq!(value["apiVersion"], "steward.admin/v1");
        assert_eq!(value["actor"], "admin@example.com");
        assert_eq!(
            value["surfaces"],
            serde_json::json!(["approvals", "envelope", "fleet"])
        );

        let script = router(AdminDashboardDemoMode::Authenticated, TEST_ORIGIN)?
            .oneshot(
                Request::builder()
                    .uri("/admin/assets/admin.js")
                    .body(Body::empty())
                    .map_err(|error| format!("build asset request: {error}"))?,
            )
            .await
            .map_err(|error| format!("request authenticated asset: {error}"))?;
        assert_eq!(script.status(), StatusCode::OK);
        assert_eq!(
            script.headers().get(header::CONTENT_TYPE),
            Some(&header::HeaderValue::from_static(
                "text/javascript; charset=utf-8"
            ))
        );
        Ok(())
    }

    #[tokio::test]
    async fn unauthenticated_mode_fails_at_the_real_admin_boundary() -> Result<(), String> {
        let response = router(AdminDashboardDemoMode::Unauthenticated, TEST_ORIGIN)?
            .oneshot(
                Request::builder()
                    .uri("/admin")
                    .body(Body::empty())
                    .map_err(|error| format!("build unauthenticated request: {error}"))?,
            )
            .await
            .map_err(|error| format!("request unauthenticated shell: {error}"))?;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response.headers().get(header::WWW_AUTHENTICATE),
            Some(&header::HeaderValue::from_static("Bearer"))
        );
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL),
            Some(&header::HeaderValue::from_static("no-store"))
        );
        assert!(
            response
                .headers()
                .contains_key(header::CONTENT_SECURITY_POLICY),
            "authentication denials must retain the real browser security headers"
        );
        Ok(())
    }

    #[tokio::test]
    async fn oidc_demo_modes_serve_the_real_sign_in_boundary() -> Result<(), String> {
        for mode in [
            AdminDashboardDemoMode::OidcUser,
            AdminDashboardDemoMode::OidcAdmin,
        ] {
            let response = router(mode, TEST_ORIGIN)?
                .oneshot(
                    Request::builder()
                        .uri("/admin/sign-in")
                        .body(Body::empty())
                        .map_err(|error| format!("build OIDC sign-in request: {error}"))?,
                )
                .await
                .map_err(|error| format!("request OIDC sign-in: {error}"))?;
            assert_eq!(response.status(), StatusCode::OK);
            assert!(
                response
                    .headers()
                    .contains_key(header::CONTENT_SECURITY_POLICY)
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn oidc_connections_surface_is_wired_behind_the_browser_session() -> Result<(), String> {
        let response = router(AdminDashboardDemoMode::OidcUser, TEST_ORIGIN)?
            .oneshot(
                Request::builder()
                    .uri("/admin/connections")
                    .body(Body::empty())
                    .map_err(|error| {
                        format!("build unauthenticated Connections request: {error}")
                    })?,
            )
            .await
            .map_err(|error| format!("request unauthenticated Connections shell: {error}"))?;
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "the Connections route must exist but fail closed before a browser session"
        );
        Ok(())
    }

    #[tokio::test]
    async fn oidc_user_can_complete_the_credential_free_connections_loopback_flow()
    -> Result<(), String> {
        let app = router(AdminDashboardDemoMode::OidcUser, TEST_ORIGIN)?;
        let login = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/auth/login")
                    .body(Body::empty())
                    .map_err(|error| format!("build login request: {error}"))?,
            )
            .await
            .map_err(|error| format!("request login: {error}"))?;
        assert_eq!(login.status(), StatusCode::SEE_OTHER);
        let flow_cookie = cookie_pair(&login, "steward-local-oidc-flow")?;
        let authorization = login
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| "login omitted authorization redirect".to_owned())?
            .to_owned();

        let authorized = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(authorization)
                    .body(Body::empty())
                    .map_err(|error| format!("build local authorization request: {error}"))?,
            )
            .await
            .map_err(|error| format!("request local authorization: {error}"))?;
        let callback = authorized
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| "local authorization omitted callback redirect".to_owned())?
            .to_owned();
        let signed_in = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(callback)
                    .header(header::COOKIE, flow_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build OIDC callback request: {error}"))?,
            )
            .await
            .map_err(|error| format!("request OIDC callback: {error}"))?;
        assert_eq!(signed_in.status(), StatusCode::SEE_OTHER);
        let session_cookie = cookie_pair(&signed_in, "steward-local-session")?;

        let session = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/api/v1/session")
                    .header(header::COOKIE, &session_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build browser session request: {error}"))?,
            )
            .await
            .map_err(|error| format!("request browser session: {error}"))?;
        let body = to_bytes(session.into_body(), 16 * 1024)
            .await
            .map_err(|error| format!("read browser session: {error}"))?;
        let session: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("parse browser session: {error}"))?;
        let csrf = session["csrf"]
            .as_str()
            .ok_or_else(|| "browser session omitted CSRF proof".to_owned())?;

        let missing_csrf = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/api/v1/connections/github/start")
                    .header(header::COOKIE, &session_cookie)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .map_err(|error| format!("build unproved connection start: {error}"))?,
            )
            .await
            .map_err(|error| format!("request unproved connection start: {error}"))?;
        assert_eq!(missing_csrf.status(), StatusCode::FORBIDDEN);

        let started = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/api/v1/connections/github/start")
                    .header(header::COOKIE, &session_cookie)
                    .header(header::ORIGIN, TEST_ORIGIN)
                    .header("sec-fetch-site", "same-origin")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("x-steward-csrf", csrf)
                    .body(Body::from("{}"))
                    .map_err(|error| format!("build connection start: {error}"))?,
            )
            .await
            .map_err(|error| format!("request connection start: {error}"))?;
        assert_eq!(started.status(), StatusCode::OK);
        let body = to_bytes(started.into_body(), 16 * 1024)
            .await
            .map_err(|error| format!("read connection start: {error}"))?;
        let start: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("parse connection start: {error}"))?;
        let continuation = start["authorizationUrl"]
            .as_str()
            .ok_or_else(|| "connection start omitted one-time URL".to_owned())?;
        assert!(continuation.starts_with(TEST_ORIGIN));
        for forbidden in ["token", "secret", "alice@example.com", "usr_"] {
            assert!(
                !String::from_utf8_lossy(&body)
                    .to_lowercase()
                    .contains(forbidden)
            );
        }

        let connected = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(continuation)
                    .header(header::COOKIE, &session_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build provider callback: {error}"))?,
            )
            .await
            .map_err(|error| format!("request provider callback: {error}"))?;
        assert_eq!(connected.status(), StatusCode::SEE_OTHER);

        let status = app
            .oneshot(
                Request::builder()
                    .uri("/admin/api/v1/connections/github")
                    .header(header::COOKIE, session_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build connected status request: {error}"))?,
            )
            .await
            .map_err(|error| format!("request connected status: {error}"))?;
        let body = to_bytes(status.into_body(), 16 * 1024)
            .await
            .map_err(|error| format!("read connected status: {error}"))?;
        let status: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("parse connected status: {error}"))?;
        assert_eq!(status["status"]["phase"], "connected");
        assert_eq!(status["status"]["accountEmail"], "alice@example.com");
        Ok(())
    }
}
