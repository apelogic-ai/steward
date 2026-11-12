//! Envelope egress grants through the real request endpoint, the Postgres-backed request broker
//! and a real deployment capability catalog.

use std::env;
use std::error::Error;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use sqlx::postgres::PgPoolOptions;
use sqlx::types::Uuid;
use steward_admission::{Envelope, EnvelopeSpec};
use steward_apiserver::browser_admin::CapabilityCatalog;
use steward_apiserver::browser_auth::{
    BrowserAuthFailure, BrowserAuthService, BrowserAuthorizationRequest, BrowserIdentityResolver,
    BrowserOidcProvider, BrowserPrincipal, BrowserRole, GoogleOidcConfig,
    VerifiedOrganizationClaims, browser_auth_router,
};
use steward_apiserver::user_envelopes::{PgEnvelopeRequestBroker, protected_router};
use steward_apiserver::{StewardRunRelease, StewardRunWorkflowInstallationMode, TaskApiConfig};
use steward_store::{EnvelopeTemplatePublication, PgStore};
use steward_types::{
    Budget, CanonicalUserId, Duration, EgressAuthority, EgressHost, Email,
    GOOGLE_ORGANIZATION_ISSUER, ModelRef, OrganizationId, OrganizationIdentity,
    OrganizationIdentityPolicy, RunnerRequirements,
};
use tower::ServiceExt;

const ORIGIN: &str = "https://steward.example.test";

#[derive(Clone)]
struct EgressOidcProvider {
    nonce: Arc<Mutex<Option<String>>>,
    subject: String,
    email: String,
}

impl BrowserOidcProvider for EgressOidcProvider {
    fn authorization_url(
        &self,
        flow: &BrowserAuthorizationRequest,
    ) -> Result<String, BrowserAuthFailure> {
        *self
            .nonce
            .lock()
            .map_err(|_| BrowserAuthFailure::ProviderUnavailable)? = Some(flow.nonce().to_owned());
        Ok(format!(
            "{ORIGIN}/admin/auth/callback?code=egress-code&state={}",
            flow.state()
        ))
    }

    fn exchange_code<'a>(
        &'a self,
        code: &'a str,
        _pkce_verifier: &'a str,
        _callback_uri: &'a str,
        expected_nonce: &'a str,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<VerifiedOrganizationClaims, BrowserAuthFailure>> + Send + 'a,
        >,
    > {
        Box::pin(async move {
            let nonce = self
                .nonce
                .lock()
                .map_err(|_| BrowserAuthFailure::ProviderUnavailable)?
                .take()
                .ok_or(BrowserAuthFailure::InvalidFlow)?;
            if code != "egress-code" || nonce != expected_nonce {
                return Err(BrowserAuthFailure::InvalidIdentity);
            }
            Ok(VerifiedOrganizationClaims::new(
                GOOGLE_ORGANIZATION_ISSUER.to_owned(),
                self.subject.clone(),
                "example.com".to_owned(),
                self.email.clone(),
                true,
                Some("Egress Tester".to_owned()),
                nonce,
            ))
        })
    }
}

#[derive(Clone)]
struct EgressIdentityResolver {
    user_id: CanonicalUserId,
    email: String,
    role: String,
}

impl BrowserIdentityResolver for EgressIdentityResolver {
    fn resolve_or_register<'a>(
        &'a self,
        _identity: &'a OrganizationIdentity,
    ) -> Pin<Box<dyn Future<Output = Result<BrowserPrincipal, BrowserAuthFailure>> + Send + 'a>>
    {
        Box::pin(async move {
            Ok(BrowserPrincipal {
                canonical_user_id: self.user_id.clone(),
                display_name: "Egress Tester".to_owned(),
                display_email: Email::parse(&self.email)
                    .map_err(|_| BrowserAuthFailure::IdentityUnavailable)?,
                role: BrowserRole::User,
                member_roles: vec![self.role.clone()],
            })
        })
    }
}

fn hosts(names: &[&str]) -> EgressAuthority {
    EgressAuthority {
        hosts: names
            .iter()
            .map(|name| EgressHost {
                host: (*name).to_owned(),
                port: 443,
            })
            .collect(),
    }
}

#[tokio::test]
async fn envelope_egress_requests_use_the_deployment_catalog_through_the_real_broker()
-> Result<(), Box<dyn Error>> {
    super::install_rustls_crypto_provider()?;
    let store = PgStore::new(
        PgPoolOptions::new()
            .max_connections(4)
            .connect(&env::var("STEWARD_TEST_DATABASE_URL")?)
            .await?,
    );
    store.migrate().await?;
    let suffix = Uuid::new_v4().simple().to_string();
    let role = format!("egress-{suffix}");
    let subject = format!("egress-subject-{suffix}");
    let email = format!("egress-{suffix}@example.com");
    let identity = store
        .register_canonical_identity(
            &OrganizationIdentityPolicy::new(
                GOOGLE_ORGANIZATION_ISSUER,
                "example.com",
                OrganizationId::parse("org_example")?,
            )?
            .validate(
                GOOGLE_ORGANIZATION_ISSUER,
                &subject,
                "example.com",
                &email,
                true,
            )?,
            "test-bootstrap",
        )
        .await?;
    let model = ModelRef {
        provider: "example".to_owned(),
        model: "model-a".to_owned(),
    };
    // The template lists one host the deployment still offers and one it no longer does.
    let template = Envelope {
        revision: 1,
        spec: EnvelopeSpec {
            llms: vec![model.clone()],
            tools: Vec::new(),
            budget: Budget {
                monthly_limit: "25.00".to_owned(),
                single_run_limit: Some("5.00".to_owned()),
                currency: "USD".to_owned(),
            },
            runtime_minutes_limit: None,
            workspace: None,
            ttl: Duration("1h".to_owned()),
            runner: RunnerRequirements::default(),
            egress: Some(hosts(&["advisories.example.com", "retired.example.com"])),
        },
    };
    let template_id = format!("egress-{suffix}");
    store
        .insert_envelope_template_revision(EnvelopeTemplatePublication {
            template_id: &template_id,
            display_name: "Egress",
            member_roles: std::slice::from_ref(&role),
            ceiling: &template,
            auto_provision_threshold: None,
            allow_inline_browser_tasks: true,
            authored_by: "test-bootstrap",
        })
        .await?;
    let catalog = CapabilityCatalog {
        schema_version: "steward.capability-catalog/v2".to_owned(),
        models: vec![model],
        egress_hosts: hosts(&["advisories.example.com", "feeds.example.org"]).hosts,
        ..CapabilityCatalog::default()
    };
    let auth = BrowserAuthService::google(
        GoogleOidcConfig::new(
            "envelope-egress-e2e",
            ORIGIN,
            format!("{ORIGIN}/admin/auth/callback"),
            "example.com",
            OrganizationId::parse("org_example")?,
        )?,
        Arc::new(EgressOidcProvider {
            nonce: Arc::new(Mutex::new(None)),
            subject,
            email: email.clone(),
        }),
        Arc::new(EgressIdentityResolver {
            user_id: identity.user_id.clone(),
            email,
            role,
        }),
    )?;
    let routes = || {
        browser_auth_router(auth.clone()).merge(protected_router(
            PgEnvelopeRequestBroker::new(
                store.clone(),
                catalog.clone(),
                StewardRunRelease {
                    manifest_schema_version: 3,
                    version: "0.8.8".to_owned(),
                    workflow_repository: "example-org/steward-run".to_owned(),
                    workflow_ref: None,
                    workflow_commit: "3".repeat(40),
                    action_commit: "4".repeat(40),
                    governed_job_container_image: None,
                },
                StewardRunWorkflowInstallationMode::Remote,
                false,
            ),
            Vec::new(),
            TaskApiConfig::default().workspace_deployment_capabilities(),
            auth.clone(),
        ))
    };

    let login = routes()
        .oneshot(
            Request::builder()
                .uri("/admin/auth/login")
                .body(Body::empty())?,
        )
        .await?;
    let flow_cookie = cookie(&login, "__Secure-steward-oidc-flow")?;
    let callback = login
        .headers()
        .get(header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix(ORIGIN))
        .ok_or_else(|| io::Error::other("login omitted the callback redirect"))?
        .to_owned();
    let callback = routes()
        .oneshot(
            Request::builder()
                .uri(callback)
                .header(header::COOKIE, flow_cookie)
                .body(Body::empty())?,
        )
        .await?;
    let session_cookie = cookie(&callback, "__Host-steward-session")?;
    let session = routes()
        .oneshot(
            Request::builder()
                .uri("/admin/api/v1/session")
                .header(header::COOKIE, &session_cookie)
                .body(Body::empty())?,
        )
        .await?;
    let csrf = serde_json::from_slice::<serde_json::Value>(
        &to_bytes(session.into_body(), 1 << 20).await?,
    )?["csrf"]
        .as_str()
        .ok_or_else(|| io::Error::other("session omitted the CSRF value"))?
        .to_owned();

    let request = |names: &[&str], key: &str| {
        let mut requested = template.clone();
        requested.spec.egress = Some(hosts(names));
        let body = serde_json::json!({
            "templateId": template_id,
            "templateRevision": 1,
            "requestedEnvelope": requested,
            "idempotencyKey": format!("{key}-{suffix}"),
            "justification": "Read public advisories",
        })
        .to_string();
        let routes = routes();
        let session_cookie = session_cookie.clone();
        let csrf = csrf.clone();
        async move {
            let response = routes
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/app/api/v1/envelope-requests")
                        .header(header::COOKIE, session_cookie)
                        .header(header::ORIGIN, ORIGIN)
                        .header("sec-fetch-site", "same-origin")
                        .header(header::CONTENT_TYPE, "application/json")
                        .header("x-steward-csrf", csrf)
                        .body(Body::from(body))?,
                )
                .await?;
            let status = response.status();
            let value = serde_json::from_slice::<serde_json::Value>(
                &to_bytes(response.into_body(), 1 << 20).await?,
            )?;
            Ok::<_, Box<dyn Error>>((status, value))
        }
    };

    let (status, within) = request(&["advisories.example.com"], "within").await?;
    assert_eq!(status, StatusCode::CREATED, "{within}");
    assert_eq!(within["request"]["status"], "provisioned");

    let (status, retired) = request(&["retired.example.com"], "retired").await?;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a template host the catalog no longer lists is never provisioned: {retired}"
    );
    assert_eq!(retired["error"], "capability_not_available");
    assert_eq!(retired["fields"], serde_json::json!(["egressHosts"]));

    let (status, above) =
        request(&["advisories.example.com", "feeds.example.org"], "above").await?;
    assert_eq!(status, StatusCode::CREATED, "{above}");
    assert_eq!(above["request"]["status"], "pending");
    assert_eq!(
        above["request"]["ceilingExceedances"],
        serde_json::json!([{
            "dimension": "egressHosts",
            "requested": [{"host": "feeds.example.org", "port": 443}],
            "ceiling": [
                {"host": "advisories.example.com", "port": 443},
                {"host": "retired.example.com", "port": 443}
            ],
        }])
    );

    let (status, outside) = request(&["other.example.net"], "outside").await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{outside}");
    assert_eq!(outside["error"], "capability_not_available");
    assert_eq!(outside["fields"], serde_json::json!(["egressHosts"]));
    Ok(())
}

fn cookie(response: &axum::response::Response, name: &str) -> Result<String, io::Error> {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(|value| {
            value
                .split(';')
                .next()
                .filter(|pair| pair.starts_with(&format!("{name}=")))
                .map(str::to_owned)
        })
        .ok_or_else(|| io::Error::other(format!("response omitted the {name} cookie")))
}
