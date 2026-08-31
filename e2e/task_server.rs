use std::collections::BTreeMap;
use std::env;
use std::error::Error;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use kube::ResourceExt;
use kube::api::{Api, DeleteParams, ListParams, PostParams};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use steward_adapter_jira::{JiraAdapter, JiraConfig};
use steward_admission::{Envelope, EnvelopeSpec};
use steward_apiserver::{
    AuthenticatedCaller, AuthenticationError, BoxFuture, KubeRuntimeRepository,
    RequestAuthenticator, StaticTaskWorkflowCatalog, TaskAuthenticationError, TaskIdentity,
    TaskIdentityResolver, TaskWorkflow, router as api_router, task_router,
};
use steward_store::{
    EnvelopeRequestReservationRequest, EnvelopeRequestStatus, EnvelopeRequestStatusUpdate, PgStore,
    WorkflowPublication,
};
use steward_types::{
    AgentRuntime, Budget, CanonicalPrincipal, Duration, KubernetesQuantity, ModelRef,
    OrganizationId, OrganizationIdentity, OrganizationIdentityPolicy, RunnerPlatform,
    RunnerRequirements, ToolGrant,
};
use tokio::net::TcpListener;

const SCHEDULED_OWNER_EMAIL: &str = "owner@example.com";
const VERSIONED_WORKFLOW_NAME: &str = "repository-review";
const VERSIONED_WORKFLOW_AGENT: &str = "codex@0.117.0";
const VERSIONED_WORKFLOW_PROMPT: &str =
    "Review the repository state that triggered this GitHub Actions run.";

#[derive(Clone)]
struct TestTaskIdentities {
    assertions: Arc<BTreeMap<String, TaskIdentity>>,
}

impl TestTaskIdentities {
    fn from_trusted_principals(alice: &CanonicalPrincipal, scheduled: &CanonicalPrincipal) -> Self {
        let mut assertions = BTreeMap::new();
        for (assertion, service) in [
            ("github-assertion", "steward-run"),
            ("slack-assertion", "burble"),
            ("portal-assertion", "request-portal"),
        ] {
            assertions.insert(
                assertion.to_owned(),
                TaskIdentity {
                    service: service.to_owned(),
                    acting_user: Some(alice.display_email.clone()),
                    owner: alice.display_email.clone(),
                    canonical_user_id: alice.user_id.clone(),
                },
            );
        }
        assertions.insert(
            "scheduled-assertion".to_owned(),
            TaskIdentity {
                service: "scheduled-scanner".to_owned(),
                acting_user: None,
                owner: scheduled.display_email.clone(),
                canonical_user_id: scheduled.user_id.clone(),
            },
        );
        Self {
            assertions: Arc::new(assertions),
        }
    }
}

impl TaskIdentityResolver for TestTaskIdentities {
    fn resolve<'a>(
        &'a self,
        assertion: &'a str,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<TaskIdentity, TaskAuthenticationError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            self.assertions
                .get(assertion)
                .cloned()
                .ok_or(TaskAuthenticationError::InvalidCredentials)
        })
    }
}

async fn seed_test_task_identities(store: &PgStore) -> Result<TestTaskIdentities, Box<dyn Error>> {
    let alice = store
        .register_canonical_identity(
            &test_google_identity("task-server-alice", "alice@example.com")?,
            "task-server-fixture-bootstrap",
        )
        .await?;
    let scheduled = store
        .register_canonical_identity(
            &test_google_identity("task-server-scheduled", SCHEDULED_OWNER_EMAIL)?,
            "task-server-fixture-bootstrap",
        )
        .await?;
    Ok(TestTaskIdentities::from_trusted_principals(
        &alice, &scheduled,
    ))
}

async fn seed_versioned_workflow_authority(
    store: &PgStore,
    identity: &TaskIdentity,
) -> Result<(), Box<dyn Error>> {
    if store
        .workflow_revision(VERSIONED_WORKFLOW_NAME, 1)
        .await?
        .is_none()
    {
        let digest = workflow_content_digest(VERSIONED_WORKFLOW_AGENT, VERSIONED_WORKFLOW_PROMPT);
        store
            .publish_initial_workflow(WorkflowPublication {
                name: VERSIONED_WORKFLOW_NAME,
                display_name: "Repository review",
                agent: VERSIONED_WORKFLOW_AGENT,
                prompt: VERSIONED_WORKFLOW_PROMPT,
                content_digest: &digest,
                published_by: "admin@example.com",
            })
            .await?;
    }

    if store
        .envelope_requests(&identity.canonical_user_id)
        .await?
        .is_empty()
    {
        let envelope = versioned_user_envelope();
        let reservation = store
            .reserve_envelope_request(EnvelopeRequestReservationRequest {
                owner_user_id: &identity.canonical_user_id,
                template_id: "engineer",
                template_revision: 1,
                requested_envelope: &envelope,
                idempotency_key: "repository-review-envelope",
                actor: identity.canonical_user_id.as_str(),
            })
            .await?;
        let digest = envelope_content_digest(&envelope)?;
        store
            .append_envelope_request_status(
                reservation.record.id,
                EnvelopeRequestStatusUpdate {
                    from: EnvelopeRequestStatus::Pending,
                    to: EnvelopeRequestStatus::Provisioned,
                    approval_id: None,
                    envelope_instance_id: Some("env_repository_review_1"),
                    envelope_digest: Some(&digest),
                    reason: None,
                    approved_envelope: Some(&envelope),
                    actor: identity.canonical_user_id.as_str(),
                },
            )
            .await?;
    }
    Ok(())
}

fn workflow_content_digest(agent: &str, prompt: &str) -> String {
    let mut hasher = Sha256::new();
    for value in [agent, prompt] {
        hasher.update(value.len().to_be_bytes());
        hasher.update(value.as_bytes());
    }
    format!("sha256:{:x}", hasher.finalize())
}

fn envelope_content_digest(envelope: &Envelope) -> Result<String, serde_json::Error> {
    let bytes = serde_json::to_vec(envelope)?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn versioned_user_envelope() -> Envelope {
    Envelope {
        revision: 3,
        spec: EnvelopeSpec {
            llms: vec![ModelRef {
                provider: "openai".to_owned(),
                model: "priced-model".to_owned(),
            }],
            tools: vec![task_tool()],
            budget: Budget {
                monthly_limit: "0.75".to_owned(),
                single_run_limit: Some("0.25".to_owned()),
                currency: "USD".to_owned(),
            },
            ttl: Duration("30m".to_owned()),
            runner: RunnerRequirements {
                platforms: vec![RunnerPlatform::Linux],
                memory: Some(KubernetesQuantity("256Mi".to_owned())),
                compute: Some(KubernetesQuantity("250m".to_owned())),
                storage: Some(KubernetesQuantity("1Gi".to_owned())),
            },
        },
    }
}

fn test_google_identity(
    subject: &str,
    email: &str,
) -> Result<OrganizationIdentity, Box<dyn Error>> {
    let policy = OrganizationIdentityPolicy::new(
        "https://accounts.google.com",
        "example.com",
        OrganizationId::parse("org_example")?,
    )?;
    Ok(policy.validate(
        "https://accounts.google.com",
        subject,
        "example.com",
        email,
        true,
    )?)
}

#[test]
fn task_fixture_identities_match_the_reviewed_organization_domain() {
    assert!(
        test_google_identity("task-server-scheduled", SCHEDULED_OWNER_EMAIL).is_ok(),
        "every fixture Task identity must satisfy the reviewed organization policy before the server starts"
    );
}

#[test]
fn task_server_selects_a_rustls_crypto_provider() -> Result<(), String> {
    install_rustls_crypto_provider().map_err(|error| error.to_string())?;
    assert!(
        tokio_rustls::rustls::crypto::CryptoProvider::get_default().is_some(),
        "the Task server must select Rustls cryptography before it constructs Kubernetes clients"
    );
    Ok(())
}

#[test]
fn task_mcp_seed_binds_the_registered_fixture_identity() {
    let seed = include_str!("../config/s1/seed-mcp-gw.ts");
    let tools_stack = include_str!("../config/s5/tools-stack.yaml");
    assert!(
        seed.contains("TASK_FIXTURE_IDENTITY_SUBJECT")
            && seed.contains("canonical_identity_subjects")
            && seed.contains("await taskFixtureHop1Subject()")
            && seed.contains("task fixture canonical identity was not registered"),
        "the Task GitHub fixture must resolve the persisted canonical subject instead of reusing S1's fixed account ID"
    );
    assert!(
        tools_stack.contains("TASK_FIXTURE_IDENTITY_SUBJECT"),
        "the task tools stack must opt the shared seed into canonical-identity lookup"
    );
}

#[tokio::test]
async fn task_server_assertions_reference_persisted_canonical_principals()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL")
        .map_err(|_| io::Error::other("STEWARD_TEST_DATABASE_URL is required"))?;
    let store = PgStore::connect(&database_url).await?;
    store.migrate().await?;
    let identity = seed_test_task_identities(&store)
        .await?
        .resolve("github-assertion")
        .await
        .map_err(|error| {
            io::Error::other(format!("fixture identity failed to resolve: {error:?}"))
        })?;

    assert_eq!(
        store
            .resolve_canonical_principal(&identity.canonical_user_id, &identity.owner)
            .await?
            .user_id,
        identity.canonical_user_id,
        "a task-server fixture identity must reference an explicit trusted canonical-user row before Task insertion"
    );
    Ok(())
}

#[derive(Clone)]
struct TestAdminAuthenticator;

impl RequestAuthenticator for TestAdminAuthenticator {
    fn authenticate<'a>(
        &'a self,
        bearer_token: &'a str,
    ) -> BoxFuture<'a, Result<AuthenticatedCaller, AuthenticationError>> {
        Box::pin(async move {
            if bearer_token != "admin-assertion" {
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    install_rustls_crypto_provider()?;
    let database_url = env::var("STEWARD_TEST_DATABASE_URL")
        .map_err(|_| io::Error::other("STEWARD_TEST_DATABASE_URL is required"))?;
    let store = PgStore::connect(&database_url).await?;
    store.migrate().await?;
    let task_identities = seed_test_task_identities(&store).await?;
    let versioned_identity = task_identities
        .resolve("github-assertion")
        .await
        .map_err(|error| io::Error::other(format!("versioned fixture identity: {error:?}")))?;
    seed_versioned_workflow_authority(&store, &versioned_identity).await?;
    let envelope = Envelope {
        revision: 1,
        spec: EnvelopeSpec {
            llms: vec![ModelRef {
                provider: "openai".to_owned(),
                model: "priced-model".to_owned(),
            }],
            tools: vec![task_tool()],
            budget: Budget {
                monthly_limit: "1.00".to_owned(),
                single_run_limit: None,
                currency: "USD".to_owned(),
            },
            ttl: Duration("1h".to_owned()),
            runner: RunnerRequirements {
                platforms: vec![RunnerPlatform::Linux],
                memory: Some(KubernetesQuantity("256Mi".to_owned())),
                compute: Some(KubernetesQuantity("250m".to_owned())),
                storage: Some(KubernetesQuantity("1Gi".to_owned())),
            },
        },
    };
    for service in [
        "steward-run",
        "burble",
        "request-portal",
        "scheduled-scanner",
    ] {
        if store.latest_service_envelope(service).await?.is_none() {
            store
                .insert_service_envelope(service, &envelope, "admin@example.com")
                .await?;
        }
    }
    let task_command = concat!(
        "set -eu; mkdir -p \"$STEWARD_OUTPUT_DIR/out\"; ",
        "cp in/payload.bin \"$STEWARD_OUTPUT_DIR/out/payload.bin\"; ",
        "attempt=0; while :; do ",
        "curl -sS --max-time 20 -H 'Content-Type: application/json' ",
        "-H 'MCP-Protocol-Version: 2025-06-18' ",
        "-d '{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",",
        "\"params\":{\"name\":\"search_repositories\",\"arguments\":{}}}' ",
        "http://hop1-capture-tools.steward-system.svc.cluster.local:8085/mcp ",
        "> \"$STEWARD_OUTPUT_DIR/out/tool.json\"; ",
        "if grep -q 'example-org/fixture-repository' \"$STEWARD_OUTPUT_DIR/out/tool.json\"; ",
        "then break; fi; attempt=$((attempt + 1)); ",
        "if [ \"$attempt\" -ge 60 ]; then exit 1; fi; sleep 1; done",
    );
    let workflows = StaticTaskWorkflowCatalog::new([
        copy_workflow(),
        workflow("code-review", "1.00", task_command),
        workflow("approval-review", "2.00", task_command),
        workflow("failing-review", "1.00", "exit 23"),
    ]);
    let client = kube::Client::try_default().await?;
    let runtime_controls = TestRuntimeControls {
        runtimes: Api::all(client.clone()),
        run_id: env::var("STEWARD_TEST_RUN_ID")
            .map_err(|_| io::Error::other("STEWARD_TEST_RUN_ID is required"))?,
    };
    let jira_listener = TcpListener::bind("127.0.0.1:8083").await?;
    let jira_state = JiraState::default();
    let jira_server_state = jira_state.clone();
    let jira_task = tokio::spawn(async move {
        axum::serve(jira_listener, jira_router(jira_server_state))
            .await
            .map_err(|error| error.to_string())
    });
    let decisions = JiraAdapter::new(
        JiraConfig {
            base_url: "http://127.0.0.1:8083".to_owned(),
            project_key: "PROJ".to_owned(),
            account_email: "jira-bot@example.com".to_owned(),
        },
        "obviously-fake-test-token".to_owned(),
    )
    .map_err(|error| io::Error::other(format!("failed to configure Jira adapter: {error:?}")))?;
    let runtimes = KubeRuntimeRepository::new(client);
    let app: Router = task_router(
        runtimes.clone(),
        store.clone(),
        decisions.clone(),
        task_identities,
        workflows,
    )
    .merge(api_router(
        runtimes,
        store,
        TestAdminAuthenticator,
        decisions,
    ))
    .merge(
        Router::new()
            .route("/test/resolutions", get(resolutions))
            .with_state(jira_state),
    )
    .merge(
        Router::new()
            .route(
                "/test/runtimes/{runtime_uid}/replace",
                post(replace_runtime_for_stale_uid_test),
            )
            .with_state(runtime_controls),
    );
    let bind = env::var("STEWARD_TEST_TASK_BIND").unwrap_or_else(|_| "0.0.0.0:8082".to_owned());
    let result = axum::serve(TcpListener::bind(bind).await?, app).await;
    jira_task.abort();
    result?;
    Ok(())
}

fn install_rustls_crypto_provider() -> Result<(), io::Error> {
    use tokio_rustls::rustls::crypto::{CryptoProvider, ring};

    if CryptoProvider::get_default().is_none() {
        let _ = ring::default_provider().install_default();
    }
    if CryptoProvider::get_default().is_some() {
        Ok(())
    } else {
        Err(io::Error::other("Rustls crypto provider is unavailable"))
    }
}

#[derive(Clone, Default)]
struct JiraState {
    inner: Arc<Mutex<JiraData>>,
}

#[derive(Default)]
struct JiraData {
    markers: BTreeMap<String, String>,
    resolutions: Vec<Value>,
    next_issue: u32,
}

#[derive(Clone)]
struct TestRuntimeControls {
    runtimes: Api<AgentRuntime>,
    run_id: String,
}

async fn replace_runtime_for_stale_uid_test(
    State(controls): State<TestRuntimeControls>,
    Path(runtime_uid): Path<String>,
) -> impl IntoResponse {
    let current = match controls.runtimes.list(&ListParams::default()).await {
        Ok(runtimes) => runtimes
            .items
            .into_iter()
            .find(|runtime| runtime.metadata.uid.as_deref() == Some(runtime_uid.as_str())),
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("could not list test runtimes: {error}")})),
            );
        }
    };
    let Some(current) = current else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "test runtime was not found"})),
        );
    };
    if current.namespace().as_deref() != Some("team-a") || !current.name_any().starts_with("task-")
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "test replacement is limited to task runtimes in team-a"})),
        );
    }
    let namespace = current.namespace().unwrap_or_default();
    let name = current.name_any();
    let mut replacement = AgentRuntime::new(&name, current.spec.clone());
    replacement.metadata.namespace = Some(namespace.clone());
    replacement.metadata.annotations = current.metadata.annotations.clone();
    replacement.metadata.labels = Some(BTreeMap::from([(
        "steward.test/run-id".to_owned(),
        controls.run_id.clone(),
    )]));
    let namespaced =
        Api::<AgentRuntime>::namespaced(controls.runtimes.clone().into_client(), &namespace);
    if let Err(error) = namespaced.delete(&name, &DeleteParams::default()).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("could not delete test runtime: {error}")})),
        );
    }
    let mut deleted = false;
    for _ in 0..60 {
        match namespaced.get_opt(&name).await {
            Ok(None) => {
                deleted = true;
                break;
            }
            Ok(Some(_)) => tokio::time::sleep(std::time::Duration::from_millis(250)).await,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(
                        json!({"error": format!("could not observe test runtime deletion: {error}")}),
                    ),
                );
            }
        }
    }
    if !deleted {
        return (
            StatusCode::GATEWAY_TIMEOUT,
            Json(json!({"error": "test runtime deletion did not complete within 15 seconds"})),
        );
    }
    match namespaced
        .create(&PostParams::default(), &replacement)
        .await
    {
        Ok(replacement) => (
            StatusCode::CREATED,
            Json(json!({
                "runtimeUid": replacement.metadata.uid,
                "runId": controls.run_id,
            })),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("could not create replacement runtime: {error}")})),
        ),
    }
}

fn jira_router(state: JiraState) -> Router {
    Router::new()
        .route("/rest/api/3/search/jql", get(search_issues))
        .route("/rest/api/3/issue", post(create_issue))
        .route("/rest/api/3/issue/{key}/comment", post(comment_issue))
        .with_state(state)
}

async fn comment_issue(
    State(state): State<JiraState>,
    Path(key): Path<String>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let mut data = match state.inner.lock() {
        Ok(data) => data,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Jira state unavailable"})),
            );
        }
    };
    data.resolutions.push(json!({"key": key, "comment": body}));
    (StatusCode::CREATED, Json(json!({})))
}

async fn resolutions(State(state): State<JiraState>) -> impl IntoResponse {
    match state.inner.lock() {
        Ok(data) => (StatusCode::OK, Json(json!(data.resolutions))),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "Jira state unavailable"})),
        ),
    }
}

async fn search_issues(
    State(state): State<JiraState>,
    Query(query): Query<BTreeMap<String, String>>,
) -> impl IntoResponse {
    let jql = query.get("jql").map_or("", String::as_str);
    let data = match state.inner.lock() {
        Ok(data) => data,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"issues": []})),
            );
        }
    };
    let issues = data
        .markers
        .iter()
        .filter(|(marker, _)| jql.contains(marker.as_str()))
        .map(|(_, key)| json!({"key": key}))
        .collect::<Vec<_>>();
    (StatusCode::OK, Json(json!({"issues": issues})))
}

async fn create_issue(
    State(state): State<JiraState>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let marker = body
        .pointer("/fields/labels")
        .and_then(Value::as_array)
        .and_then(|labels| {
            labels.iter().filter_map(Value::as_str).find(|label| {
                label
                    .strip_prefix("steward-approval-")
                    .is_some_and(|suffix| !suffix.is_empty())
            })
        })
        .map(str::to_owned);
    let Some(marker) = marker else {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": "Steward approval marker is required"})),
        );
    };
    let mut data = match state.inner.lock() {
        Ok(data) => data,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Jira state unavailable"})),
            );
        }
    };
    if let Some(key) = data.markers.get(&marker) {
        return (StatusCode::OK, Json(json!({"key": key})));
    }
    data.next_issue += 1;
    let key = format!("PROJ-{}", 122 + data.next_issue);
    data.markers.insert(marker, key.clone());
    (StatusCode::CREATED, Json(json!({"key": key})))
}

fn copy_workflow() -> TaskWorkflow {
    TaskWorkflow {
        name: "copy-smoke".to_owned(),
        namespace: "team-a".to_owned(),
        coding_agent_runtime: "base".to_owned(),
        llms: Vec::new(),
        tools: Vec::new(),
        budget: Budget {
            monthly_limit: "0.00".to_owned(),
            single_run_limit: None,
            currency: "USD".to_owned(),
        },
        ttl: Duration("1h".to_owned()),
        command: vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            concat!(
                "set -eu; mkdir -p \"$STEWARD_OUTPUT_DIR/out\"; ",
                "cp in/payload.bin \"$STEWARD_OUTPUT_DIR/out/payload.bin\"",
            )
            .to_owned(),
        ],
    }
}

fn workflow(name: &str, budget: &str, shell: &str) -> TaskWorkflow {
    TaskWorkflow {
        name: name.to_owned(),
        namespace: "team-a".to_owned(),
        coding_agent_runtime: "base".to_owned(),
        llms: Vec::new(),
        tools: vec![task_tool()],
        budget: Budget {
            monthly_limit: budget.to_owned(),
            single_run_limit: None,
            currency: "USD".to_owned(),
        },
        ttl: Duration("1h".to_owned()),
        command: vec!["/bin/sh".to_owned(), "-c".to_owned(), shell.to_owned()],
    }
}

fn task_tool() -> ToolGrant {
    ToolGrant {
        provider: "github".to_owned(),
        resource: "search_repositories".to_owned(),
        action: "read".to_owned(),
    }
}
