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
use serde_json::{Value, json};
use steward_adapter_jira::{JiraAdapter, JiraConfig};
use steward_admission::{Envelope, EnvelopeSpec};
use steward_apiserver::{
    AuthenticatedCaller, AuthenticationError, BoxFuture, KubeRuntimeRepository,
    RequestAuthenticator, StaticTaskWorkflowCatalog, TaskAuthenticationError, TaskIdentity,
    TaskIdentityResolver, TaskWorkflow, router as api_router, task_router,
};
use steward_store::PgStore;
use steward_types::{Budget, CanonicalUserId, Duration, Email, ToolGrant};
use tokio::net::TcpListener;

#[derive(Clone)]
struct TestTaskIdentities;

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
            let (service, acting_user, owner) = match assertion {
                "github-assertion" => (
                    "steward-run",
                    Some("alice@example.com"),
                    "alice@example.com",
                ),
                "slack-assertion" => ("burble", Some("alice@example.com"), "alice@example.com"),
                "portal-assertion" => (
                    "request-portal",
                    Some("alice@example.com"),
                    "alice@example.com",
                ),
                "scheduled-assertion" => ("scheduled-scanner", None, "owner@example.org"),
                _ => return Err(TaskAuthenticationError::InvalidCredentials),
            };
            Ok(TaskIdentity {
                service: service.to_owned(),
                acting_user: acting_user.map(|email| Email(email.to_owned())),
                owner: Email(owner.to_owned()),
                canonical_user_id: CanonicalUserId::parse(match assertion {
                    "scheduled-assertion" => "usr_abcdef0123456789abcdef0123456789",
                    _ => "usr_0123456789abcdef0123456789abcdef",
                })
                .map_err(|_| TaskAuthenticationError::InvalidCredentials)?,
            })
        })
    }
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
                is_admin: true,
                can_bootstrap_steward_run_service_envelope: false,
            })
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL")
        .map_err(|_| io::Error::other("STEWARD_TEST_DATABASE_URL is required"))?;
    let store = PgStore::connect(&database_url).await?;
    store.migrate().await?;
    let envelope = Envelope {
        revision: 1,
        spec: EnvelopeSpec {
            llms: Vec::new(),
            tools: vec![task_tool()],
            budget: Budget {
                monthly_limit: "1.00".to_owned(),
                currency: "USD".to_owned(),
            },
            ttl: Duration("1h".to_owned()),
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
        TestTaskIdentities,
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
    );
    let bind = env::var("STEWARD_TEST_TASK_BIND").unwrap_or_else(|_| "0.0.0.0:8082".to_owned());
    let result = axum::serve(TcpListener::bind(bind).await?, app).await;
    jira_task.abort();
    result?;
    Ok(())
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
