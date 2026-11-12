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
use std::borrow::Cow;
use std::str::FromStr;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::types::Uuid;
use steward_admission::AdmissionDelta;
use steward_admission::{Envelope, EnvelopeSpec};
use steward_apiserver::browser_admin::CapabilityCatalog;
use steward_apiserver::browser_auth::{
    BrowserAuthFailure, BrowserAuthService, BrowserAuthorizationRequest, BrowserIdentityResolver,
    BrowserOidcProvider, BrowserPrincipal, BrowserRole, GoogleOidcConfig,
    VerifiedOrganizationClaims, browser_auth_router,
};
use steward_apiserver::user_envelopes::{PgEnvelopeRequestBroker, protected_router};
use steward_apiserver::{StewardRunRelease, StewardRunWorkflowInstallationMode, TaskApiConfig};
use steward_store::{EnvelopeRequestReservationRequest, EnvelopeTemplatePublication, PgStore};
use steward_types::direct_package::{WorkspaceAuthority, WorkspaceEntryType};
use steward_types::{
    Budget, CanonicalUserId, Duration, EgressAuthority, EgressHost, Email,
    GOOGLE_ORGANIZATION_ISSUER, ModelRef, OrganizationId, OrganizationIdentity,
    OrganizationIdentityPolicy, RunnerRequirements,
};
use steward_types::{KubernetesQuantity, RunnerPlatform};
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

/// The eleven dimensions migration 0069 bounded, one delta each.
fn eleven_historical_deltas() -> Vec<AdmissionDelta> {
    let quantity = || KubernetesQuantity("1".to_owned());
    vec![
        AdmissionDelta::Budget {
            requested: "30.00".to_owned(),
            ceiling: "25.00".to_owned(),
            currency: "USD".to_owned(),
        },
        AdmissionDelta::SingleRunBudget {
            requested: None,
            ceiling: "5.00".to_owned(),
            currency: "USD".to_owned(),
        },
        AdmissionDelta::RuntimeMinutes {
            requested: None,
            ceiling: "60".to_owned(),
        },
        AdmissionDelta::Ttl {
            requested: "2h".to_owned(),
            ceiling: "1h".to_owned(),
        },
        AdmissionDelta::Models {
            requested: Vec::new(),
            ceiling: Vec::new(),
        },
        AdmissionDelta::Tools {
            requested: Vec::new(),
            ceiling: Vec::new(),
        },
        AdmissionDelta::RunnerPlatforms {
            requested: vec![RunnerPlatform::Linux],
            ceiling: Vec::new(),
        },
        AdmissionDelta::RunnerMemory {
            requested: quantity(),
            ceiling: None,
        },
        AdmissionDelta::RunnerCompute {
            requested: quantity(),
            ceiling: None,
        },
        AdmissionDelta::RunnerStorage {
            requested: quantity(),
            ceiling: None,
        },
        AdmissionDelta::Workspace {
            requested: WorkspaceAuthority {
                allowed_types: vec![WorkspaceEntryType::Git],
                max_total_size: None,
                max_files: None,
                max_history_depth: None,
                max_submodule_depth: None,
            },
            ceiling: None,
        },
    ]
}

#[tokio::test]
async fn migration_0077_upgrades_historical_envelope_requests_to_twelve_exceedances()
-> Result<(), Box<dyn Error>> {
    super::install_rustls_crypto_provider()?;
    let admin_options = PgConnectOptions::from_str(&env::var("STEWARD_TEST_DATABASE_URL")?)?;
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(admin_options.clone())
        .await?;
    let database = format!("steward_upgrade_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE {database}"))
        .execute(&admin)
        .await?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(admin_options.database(&database))
        .await?;
    let outcome = upgrade_with_historical_rows(pool.clone()).await;
    pool.close().await;
    sqlx::query(&format!("DROP DATABASE {database} WITH (FORCE)"))
        .execute(&admin)
        .await?;
    outcome
}

async fn upgrade_with_historical_rows(pool: sqlx::PgPool) -> Result<(), Box<dyn Error>> {
    let mut before = sqlx::migrate!("../migrations");
    before.migrations = Cow::Owned(
        before
            .migrations
            .iter()
            .filter(|migration| migration.version < 77)
            .cloned()
            .collect(),
    );
    before.run(&pool).await?;
    let store = PgStore::new(pool.clone());
    let identity = store
        .register_canonical_identity(
            &OrganizationIdentityPolicy::new(
                GOOGLE_ORGANIZATION_ISSUER,
                "example.com",
                OrganizationId::parse("org_example")?,
            )?
            .validate(
                GOOGLE_ORGANIZATION_ISSUER,
                "upgrade-subject",
                "example.com",
                "upgrade@example.com",
                true,
            )?,
            "test-bootstrap",
        )
        .await?;
    let template = Envelope {
        revision: 1,
        spec: EnvelopeSpec {
            llms: vec![ModelRef {
                provider: "example".to_owned(),
                model: "model-a".to_owned(),
            }],
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
            egress: None,
        },
    };
    store
        .insert_envelope_template_revision(EnvelopeTemplatePublication {
            template_id: "upgrade",
            display_name: "Upgrade",
            member_roles: &["engineer".to_owned()],
            ceiling: &template,
            auto_provision_threshold: None,
            allow_inline_browser_tasks: true,
            authored_by: "test-bootstrap",
        })
        .await?;
    let historical = eleven_historical_deltas();
    let reserve = |key: &'static str, deltas: Vec<AdmissionDelta>| {
        let store = store.clone();
        let owner = identity.user_id.clone();
        let template = template.clone();
        async move {
            store
                .reserve_envelope_request(EnvelopeRequestReservationRequest {
                    owner_user_id: &owner,
                    template_id: Some("upgrade"),
                    template_revision: Some(1),
                    requested_envelope: &template,
                    idempotency_key: key,
                    justification: Some("Upgrade rehearsal"),
                    ceiling_exceedances: &deltas,
                    actor: "test-bootstrap",
                })
                .await
        }
    };
    reserve("historical-none", Vec::new()).await?;
    reserve("historical-one", historical[3..4].to_vec()).await?;
    reserve("historical-eleven", historical.clone()).await?;
    let before_rows: Vec<(String, serde_json::Value)> = sqlx::query_as(
        "SELECT idempotency_key, ceiling_exceedances FROM envelope_requests ORDER BY idempotency_key",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(before_rows.len(), 3);

    sqlx::migrate!("../migrations").run(&pool).await?;

    let after_rows: Vec<(String, serde_json::Value)> = sqlx::query_as(
        "SELECT idempotency_key, ceiling_exceedances FROM envelope_requests ORDER BY idempotency_key",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        after_rows, before_rows,
        "historical requests survive the upgrade unchanged"
    );

    let mut twelve = historical;
    twelve.push(AdmissionDelta::EgressHosts {
        requested: hosts(&["feeds.example.org"]).hosts,
        ceiling: Vec::new(),
    });
    reserve("upgraded-twelve", twelve.clone()).await?;

    let mut thirteen = serde_json::to_value(&twelve)?;
    if let serde_json::Value::Array(deltas) = &mut thirteen {
        deltas.push(deltas[0].clone());
    }
    let over_bound = sqlx::query(
        "INSERT INTO envelope_requests \
         SELECT (jsonb_populate_record(NULL::envelope_requests, to_jsonb(existing) \
           || jsonb_build_object('id', gen_random_uuid(), 'idempotency_key', 'over-bound', \
                                 'ceiling_exceedances', $1::jsonb))).* \
         FROM envelope_requests existing WHERE idempotency_key = 'upgraded-twelve'",
    )
    .bind(thirteen)
    .execute(&pool)
    .await;
    let message = over_bound
        .err()
        .map(|error| error.to_string())
        .ok_or_else(|| io::Error::other("thirteen exceedances must be refused by the database"))?;
    assert!(
        message.contains("envelope_requests_ceiling_exceedances_check"),
        "the new bound refuses thirteen exceedances: {message}"
    );
    Ok(())
}
