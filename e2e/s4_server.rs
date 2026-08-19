use std::collections::BTreeMap;
use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::serve::Listener;
use axum::{Json, Router};
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use steward_adapter_jira::{JiraAdapter, JiraConfig};
use steward_apiserver::{
    AuthenticatedCaller, AuthenticationError, BoxFuture, KubeRuntimeRepository,
    RequestAuthenticator, router,
};
use steward_controller::{
    PortError, SandboxObservation, SandboxRequest, SandboxRuntime, run_controller_with_store,
    webhook_router_for_controller,
};
use steward_store::PgStore;
use steward_types::{CanonicalUserId, RuntimeRefs};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::sleep;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::server::TlsStream;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    install_rustls_crypto_provider()?;
    let database_url = required("STEWARD_TEST_DATABASE_URL")?;
    let certificate_path = required("STEWARD_TEST_TLS_CERT_DER")?;
    let private_key_path = required("STEWARD_TEST_TLS_KEY_DER")?;
    let steward_bind =
        env::var("STEWARD_TEST_HTTP_BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_owned());
    let jira_bind =
        env::var("STEWARD_TEST_JIRA_BIND").unwrap_or_else(|_| "0.0.0.0:8081".to_owned());
    let jira_base_url =
        env::var("STEWARD_TEST_JIRA_URL").unwrap_or_else(|_| "http://127.0.0.1:8081".to_owned());

    let jira_state = JiraState::default();
    let jira_listener = TcpListener::bind(&jira_bind).await?;
    let jira_task = tokio::spawn(async move {
        axum::serve(jira_listener, jira_router(jira_state))
            .await
            .map_err(|error| error.to_string())
    });

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let client = kube::Client::try_default().await?;
    let controller_task = tokio::spawn(run_controller_with_store(
        client.clone(),
        S4SandboxRuntime,
        store.clone(),
    ));
    let decisions = JiraAdapter::new(
        JiraConfig {
            base_url: jira_base_url,
            project_key: "PROJ".to_owned(),
            account_email: "jira-bot@example.com".to_owned(),
        },
        "obviously-fake-test-token".to_owned(),
    )
    .map_err(|error| io::Error::other(format!("failed to configure Jira adapter: {error:?}")))?;
    let app = router(
        KubeRuntimeRepository::new(client),
        store.clone(),
        S4Authenticator,
        decisions,
    )
    .merge(webhook_router_for_controller(
        store,
        "system:serviceaccount:steward-system:steward-s3".to_owned(),
    ));
    let listener = TcpListener::bind(&steward_bind).await?;
    let certificate = CertificateDer::from(fs::read(certificate_path)?);
    let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(fs::read(private_key_path)?));
    let tls = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![certificate], private_key)?;
    let steward_result = axum::serve(
        TlsListener {
            acceptor: TlsAcceptor::from(Arc::new(tls)),
            listener,
        },
        app,
    )
    .await;
    jira_task.abort();
    controller_task.abort();
    steward_result?;
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

#[derive(Clone, Copy)]
struct S4SandboxRuntime;

impl SandboxRuntime for S4SandboxRuntime {
    async fn ensure(&self, request: &SandboxRequest) -> Result<SandboxObservation, PortError> {
        Ok(SandboxObservation::Running {
            refs: RuntimeRefs {
                workspace: Some(format!("workspace-{}", request.runtime.0)),
                sandbox: Some(format!("sandbox-{}", request.runtime.0)),
                litellm_key: None,
            },
        })
    }

    async fn delete(&self, _request: &SandboxRequest) -> Result<SandboxObservation, PortError> {
        Ok(SandboxObservation::Absent)
    }
}

#[derive(Clone)]
struct S4Authenticator;

impl RequestAuthenticator for S4Authenticator {
    fn authenticate<'a>(
        &'a self,
        bearer_token: &'a str,
    ) -> BoxFuture<'a, Result<AuthenticatedCaller, AuthenticationError>> {
        Box::pin(async move {
            match bearer_token {
                "test-alice-session" => {
                    user("alice@example.com", "usr_0123456789abcdef0123456789abcdef")
                }
                "test-bob-session" => {
                    user("bob@example.org", "usr_abcdef0123456789abcdef0123456789")
                }
                "test-admin-session" => Ok(AuthenticatedCaller {
                    actor: "admin@example.com".to_owned(),
                    member_roles: Vec::new(),
                    canonical_user_id: None,
                    is_admin: true,
                    can_bootstrap_steward_run_service_envelope: false,
                }),
                _ => Err(AuthenticationError::InvalidCredentials),
            }
        })
    }
}

fn user(actor: &str, canonical_user_id: &str) -> Result<AuthenticatedCaller, AuthenticationError> {
    Ok(AuthenticatedCaller {
        actor: actor.to_owned(),
        member_roles: vec!["engineer".to_owned()],
        canonical_user_id: Some(
            CanonicalUserId::parse(canonical_user_id)
                .map_err(|_| AuthenticationError::InvalidCredentials)?,
        ),
        is_admin: false,
        can_bootstrap_steward_run_service_envelope: false,
    })
}

#[derive(Clone, Default)]
struct JiraState {
    inner: Arc<Mutex<JiraData>>,
}

#[derive(Default)]
struct JiraData {
    issues: BTreeMap<String, JiraIssue>,
    next_issue: u32,
}

struct JiraIssue {
    key: String,
    request_marker: String,
    create_body: Value,
    comments: Vec<Value>,
    transitioned: bool,
}

fn jira_router(state: JiraState) -> Router {
    Router::new()
        .route("/rest/api/3/search/jql", get(search_issues))
        .route("/rest/api/3/issue", post(create_issue))
        .route("/rest/api/3/issue/{key}/comment", post(comment_issue))
        .route("/test/issues/{key}/transition", post(transition_issue))
        .route("/test/state", get(jira_state))
        .with_state(state)
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
        .issues
        .values()
        .filter(|issue| jql.contains(&issue.request_marker))
        .map(|issue| json!({"key": issue.key}))
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
    if let Some(issue) = data
        .issues
        .values()
        .find(|issue| issue.request_marker == marker)
    {
        return (StatusCode::OK, Json(json!({"key": issue.key})));
    }
    data.next_issue += 1;
    let key = format!("PROJ-{}", 122 + data.next_issue);
    data.issues.insert(
        key.clone(),
        JiraIssue {
            key: key.clone(),
            request_marker: marker,
            create_body: body,
            comments: Vec::new(),
            transitioned: false,
        },
    );
    (StatusCode::CREATED, Json(json!({"key": key})))
}

async fn comment_issue(
    State(state): State<JiraState>,
    Path(key): Path<String>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let mut data = match state.inner.lock() {
        Ok(data) => data,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR,
    };
    let Some(issue) = data.issues.get_mut(&key) else {
        return StatusCode::NOT_FOUND;
    };
    issue.comments.push(body);
    StatusCode::CREATED
}

async fn transition_issue(
    State(state): State<JiraState>,
    Path(key): Path<String>,
) -> impl IntoResponse {
    let mut data = match state.inner.lock() {
        Ok(data) => data,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR,
    };
    let Some(issue) = data.issues.get_mut(&key) else {
        return StatusCode::NOT_FOUND;
    };
    issue.transitioned = true;
    StatusCode::NO_CONTENT
}

async fn jira_state(State(state): State<JiraState>) -> impl IntoResponse {
    let data = match state.inner.lock() {
        Ok(data) => data,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Jira state unavailable"})),
            );
        }
    };
    let issues = data
        .issues
        .values()
        .map(|issue| {
            json!({
                "key": issue.key,
                "requestMarker": issue.request_marker,
                "createBody": issue.create_body,
                "comments": issue.comments,
                "transitioned": issue.transitioned,
            })
        })
        .collect::<Vec<_>>();
    (StatusCode::OK, Json(json!({"issues": issues})))
}

fn required(name: &str) -> Result<String, io::Error> {
    env::var(name).map_err(|_| io::Error::other(format!("{name} is required")))
}

struct TlsListener {
    acceptor: TlsAcceptor,
    listener: TcpListener,
}

impl Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.listener.accept().await {
                Ok((stream, address)) => match self.acceptor.accept(stream).await {
                    Ok(stream) => return (stream, address),
                    Err(error) => eprintln!("test TLS handshake failed: {error}"),
                },
                Err(error) => {
                    eprintln!("test TLS listener accept failed: {error}");
                    sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

#[cfg(test)]
mod tests {
    use super::{ServerConfig, install_rustls_crypto_provider};

    #[test]
    fn tls_server_configuration_selects_a_crypto_provider() -> Result<(), String> {
        install_rustls_crypto_provider().map_err(|error| error.to_string())?;
        let _ = ServerConfig::builder();
        Ok(())
    }
}
