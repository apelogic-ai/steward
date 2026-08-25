//! Opt-in, loopback-only dashboard harness for human localhost acceptance.

use std::net::SocketAddr;
use std::str::FromStr;

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Request};
use axum::http::{HeaderValue, Method, StatusCode, header, uri::Authority};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use steward_admission::{AdmissionDecision, Envelope, evaluate, validate_envelope};
use steward_types::{
    AgentRuntimeSpec, AgentType, Budget, Duration, Email, KubernetesQuantity, ModelRef, Principal,
    RunnerPlatform, RunnerRequirements, ToolGrant,
};

use crate::{
    AuthenticatedCaller, AuthenticationError, BoxFuture, RequestAuthenticator, admin_ui,
    agent_runs_ui::protected_router as protected_agent_runs_router,
    browser_auth::{
        BrowserSessionBinding, LocalFakeIdentity, browser_auth_router,
        local_fake_browser_auth_service,
    },
    connections,
    connections_demo::LocalConnectionsBroker,
    protect_admin_routes,
    user_envelopes::protected_router as protected_envelope_router,
    user_envelopes_demo::{LocalEmptyAgentRunLedger, LocalEnvelopeRequestBroker},
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
                canonical_user_id: None,
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DemoEnvelopeThresholds {
    budget_monthly_limit: String,
    ttl: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DemoEnvelopeProofRequest {
    api_version: String,
    base_revision: i64,
    candidate: Envelope,
    thresholds: DemoEnvelopeThresholds,
}

fn engineer_template() -> Envelope {
    Envelope {
        revision: 3,
        spec: steward_admission::EnvelopeSpec {
            llms: vec![ModelRef {
                provider: "openai".to_owned(),
                model: "gpt-5.4".to_owned(),
            }],
            tools: vec![ToolGrant {
                provider: "github".to_owned(),
                resource: "repository".to_owned(),
                action: "get_file_contents".to_owned(),
            }],
            budget: Budget {
                monthly_limit: "250.00".to_owned(),
                single_run_limit: None,
                currency: "USD".to_owned(),
            },
            ttl: Duration("720h".to_owned()),
            runner: RunnerRequirements {
                platforms: vec![RunnerPlatform::Linux],
                memory: Some(KubernetesQuantity("2Gi".to_owned())),
                compute: Some(KubernetesQuantity("1".to_owned())),
                storage: Some(KubernetesQuantity("10Gi".to_owned())),
            },
        },
    }
}

async fn envelope_template(Path(template_id): Path<String>) -> Response {
    if template_id != "engineer" {
        return StatusCode::NOT_FOUND.into_response();
    }
    Json(serde_json::json!({
        "apiVersion": "steward.admin/v1",
        "source": "localhost-review-fixture",
        "template": {
            "id": "engineer", "displayName": "Engineer", "status": "active", "revision": 3,
            "envelope": engineer_template(),
            "thresholds": {"budgetMonthlyLimit": "100.00", "ttl": "168h"},
            "actionClasses": [
                {"name": "read", "state": "authoritative", "grantCount": 1},
                {"name": "write", "state": "unavailable", "grantCount": 0},
                {"name": "destructive", "state": "unavailable", "grantCount": 0}
            ],
            "capabilities": {
                "memory": {"authoritative": false, "reason": "No end-to-end resource contract"},
                "storage": {"authoritative": false, "reason": "No end-to-end resource contract"},
                "compute": {"authoritative": false, "reason": "No end-to-end resource contract"},
                "accelerator": {"authoritative": false, "reason": "No end-to-end resource contract"},
                "maxRuntime": {"authoritative": false, "reason": "Only standing-delegation TTL is enforced"},
                "tokenBudget": {"authoritative": false, "reason": "Spend is authoritative; token ceilings are not"}
            }
        }
    }))
    .into_response()
}

async fn prove_envelope_template(
    Path(template_id): Path<String>,
    Json(request): Json<DemoEnvelopeProofRequest>,
) -> Response {
    let current = engineer_template();
    if template_id != "engineer"
        || request.api_version != "steward.admin/v1"
        || request.base_revision != current.revision
        || request.candidate.revision != current.revision + 1
    {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "stale or unsupported template revision"})),
        )
            .into_response();
    }
    if validate_envelope(&request.candidate).is_err() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({"error": "candidate envelope is invalid"})),
        )
            .into_response();
    }
    let identity = Email("alice@example.com".to_owned());
    let threshold_request = AgentRuntimeSpec {
        principal: Principal::User {
            acting_user: identity.clone(),
        },
        owner: identity,
        canonical_authority: None,
        agent_type: AgentType {
            name: "template-proof".to_owned(),
        },
        llms: request.candidate.spec.llms.clone(),
        tools: request.candidate.spec.tools.clone(),
        budget: Budget {
            monthly_limit: request.thresholds.budget_monthly_limit,
            single_run_limit: None,
            currency: request.candidate.spec.budget.currency.clone(),
        },
        ttl: Duration(request.thresholds.ttl),
        runner: request.candidate.spec.runner.clone(),
        bindings: None,
    };
    match evaluate(&threshold_request, &request.candidate) {
        Ok(AdmissionDecision::Admit) => Json(serde_json::json!({
            "apiVersion": "steward.admin/v1", "verdict": "unknown",
            "baseRevision": request.base_revision, "candidateRevision": request.candidate.revision,
            "supportedAuthorityValid": true, "thresholdsWithinCeilings": true,
            "blastRadius": {"state": "unavailable", "affectedAgents": null, "reason": "The authoritative runtime impact read model is not available"},
            "propagation": [
                {"target": "OpenShell", "state": "not-applied"}, {"target": "MCP-GW", "state": "not-applied"},
                {"target": "LiteLLM", "state": "not-applied"}, {"target": "Kubernetes", "state": "not-applied"}
            ], "applyAllowed": false, "reason": "Unknown blast radius fails closed"
        })).into_response(),
        Ok(AdmissionDecision::Reject { deltas }) => (StatusCode::UNPROCESSABLE_ENTITY, Json(serde_json::json!({"error": "auto-provision threshold exceeds its hard ceiling", "deltas": deltas}))).into_response(),
        Err(_) => (StatusCode::UNPROCESSABLE_ENTITY, Json(serde_json::json!({"error": "threshold contract is invalid"}))).into_response(),
    }
}

async fn normalize_loopback_demo_origin(mut request: Request<Body>, next: Next) -> Response {
    if matches!(
        *request.method(),
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    ) {
        let host = request
            .headers()
            .get(header::HOST)
            .and_then(|value| value.to_str().ok());
        let origin = request
            .headers()
            .get(header::ORIGIN)
            .and_then(|value| value.to_str().ok());
        let loopback = host
            .and_then(|value| Authority::from_str(value).ok())
            .is_some_and(|authority| {
                authority.host() == "localhost"
                    || authority
                        .host()
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|address| address.is_loopback())
            });
        if loopback
            && host.is_some_and(|value| origin == Some(format!("http://{value}").as_str()))
            && let Some(replacement) = host
                .and_then(|value| HeaderValue::from_str(format!("https://{value}").as_str()).ok())
        {
            request.headers_mut().insert(header::ORIGIN, replacement);
        }
    }
    next.run(request).await
}

pub fn router(mode: AdminDashboardDemoMode, origin: &str) -> Result<Router, String> {
    let template_routes = Router::<()>::new()
        .route(
            "/admin/api/v1/envelope-templates/{template_id}",
            get(envelope_template),
        )
        .route(
            "/admin/api/v1/envelope-templates/{template_id}/prove",
            post(prove_envelope_template),
        )
        .route_layer(middleware::from_fn(
            admin_ui::enforce_browser_mutation_boundary,
        ))
        .layer(middleware::from_fn(normalize_loopback_demo_origin));
    let protected = protect_admin_routes(
        admin_ui::router::<()>().merge(template_routes),
        DemoAuthenticator,
    );
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
        .merge(connections::protected_router(broker, browser_auth.clone()))
        .merge(protected_envelope_router(
            LocalEnvelopeRequestBroker::new(),
            browser_auth.clone(),
        ))
        .merge(protected_agent_runs_router(
            LocalEmptyAgentRunLedger,
            browser_auth,
        )))
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
    async fn template_prover_is_admin_scoped_revision_bound_and_fails_closed() -> Result<(), String>
    {
        let app = router(AdminDashboardDemoMode::Authenticated, TEST_ORIGIN)?;
        let template = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/api/v1/envelope-templates/engineer")
                    .body(Body::empty())
                    .map_err(|error| format!("build template request: {error}"))?,
            )
            .await
            .map_err(|error| format!("request template: {error}"))?;
        assert_eq!(template.status(), StatusCode::OK);
        let template = to_bytes(template.into_body(), 64 * 1024)
            .await
            .map_err(|error| format!("read template response: {error}"))?;
        let template: serde_json::Value = serde_json::from_slice(&template)
            .map_err(|error| format!("parse template response: {error}"))?;
        assert_eq!(template["template"]["revision"], 3);
        assert_eq!(
            template["template"]["capabilities"]["memory"]["authoritative"],
            false
        );
        assert!(
            !template.to_string().to_lowercase().contains("secret"),
            "templates must never contain credential-shaped data"
        );

        let proof = serde_json::json!({
            "apiVersion": "steward.admin/v1", "baseRevision": 3,
            "candidate": {
                "revision": 4,
                "spec": {
                    "llms": [{"provider": "openai", "model": "gpt-5.4"}],
                    "tools": [{"provider": "github", "resource": "repository", "action": "get_file_contents"}],
                    "budget": {"monthlyLimit": "250.00", "currency": "USD"}, "ttl": "720h"
                }
            },
            "thresholds": {"budgetMonthlyLimit": "100.00", "ttl": "168h"}
        });
        let prove = |body: serde_json::Value| {
            Request::builder()
                .method("POST")
                .uri("/admin/api/v1/envelope-templates/engineer/prove")
                .header("host", "127.0.0.1:33002")
                .header("origin", TEST_ORIGIN)
                .header("sec-fetch-site", "same-origin")
                .header("x-steward-csrf", "1")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .map_err(|error| format!("build template proof: {error}"))
        };
        let response = app
            .clone()
            .oneshot(prove(proof.clone())?)
            .await
            .map_err(|error| format!("prove template: {error}"))?;
        assert_eq!(response.status(), StatusCode::OK);
        let response = to_bytes(response.into_body(), 64 * 1024)
            .await
            .map_err(|error| format!("read proof response: {error}"))?;
        let response: serde_json::Value = serde_json::from_slice(&response)
            .map_err(|error| format!("parse proof response: {error}"))?;
        assert_eq!(response["verdict"], "unknown");
        assert_eq!(response["applyAllowed"], false);

        let mut over_ceiling = proof.clone();
        over_ceiling["thresholds"]["budgetMonthlyLimit"] = serde_json::json!("251.00");
        assert_eq!(
            app.clone()
                .oneshot(prove(over_ceiling)?)
                .await
                .map_err(|error| format!("prove over-ceiling template: {error}"))?
                .status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let mut stale = proof;
        stale["baseRevision"] = serde_json::json!(2);
        assert_eq!(
            app.oneshot(prove(stale)?)
                .await
                .map_err(|error| format!("prove stale template: {error}"))?
                .status(),
            StatusCode::CONFLICT
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

        for path in [
            "/envelopes",
            "/runs",
            "/settings",
            "/app/api/v1/envelope-templates",
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .header(header::COOKIE, &session_cookie)
                        .body(Body::empty())
                        .map_err(|error| {
                            format!("build signed-in workspace request {path}: {error}")
                        })?,
                )
                .await
                .map_err(|error| format!("request signed-in workspace route {path}: {error}"))?;
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "the signed-in localhost demo must compose the workspace route {path}"
            );
        }

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
