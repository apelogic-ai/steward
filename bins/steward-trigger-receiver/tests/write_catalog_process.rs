use std::env;
use std::error::Error;
use std::fs::{self, File};
use std::future::Future;
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration as StdDuration, Instant};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use ring::hmac;
use serde_json::json;
use sha2::{Digest, Sha256};
use steward_admission::{Envelope, EnvelopeSpec};
use steward_apiserver::{
    TaskApiConfig, TaskAuthenticationError, TaskIdentity, TaskIdentityResolver, task_router,
};
use steward_ports::{PortError, TaskExecutionAdapter, TaskExecutionPlan, TaskExecutionPlanRequest};
use steward_store::{
    EnvelopeRequestReservationRequest, EnvelopeRequestStatus, EnvelopeRequestStatusUpdate,
    EnvelopeTemplatePublication, PgStore, TaskOrchestrationMode, WorkflowPublication,
};
use steward_types::{
    Budget, CanonicalUserId, Duration, Email, GOOGLE_ORGANIZATION_ISSUER, ModelRef, OrganizationId,
    OrganizationIdentityPolicy, RunnerRequirements, ToolGrant,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tower::ServiceExt;
use uuid::Uuid;

const SIGNING_SECRET: &[u8] = b"obviously-fake-jira-signing-secret";

struct SeededReceiverAuthority {
    owner_id: String,
    owner_email: String,
    digest: String,
    workflow: String,
}

struct ReceiverProcess {
    child: Child,
}

impl ReceiverProcess {
    fn stop(&mut self) -> Result<(), Box<dyn Error>> {
        if self.child.try_wait()?.is_none() {
            let status = Command::new("kill")
                .arg(self.child.id().to_string())
                .status()?;
            if !status.success() {
                return Err(io::Error::other("failed to stop the receiver process").into());
            }
            self.child.wait()?;
        }
        Ok(())
    }
}

impl Drop for ReceiverProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = Command::new("kill")
                .arg(self.child.id().to_string())
                .status();
            let _ = self.child.wait();
        }
    }
}

struct TestServer {
    address: SocketAddr,
    handle: tokio::task::JoinHandle<()>,
    requests: Arc<Mutex<Vec<(String, String)>>>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

#[derive(Clone)]
struct FixedTaskIdentity(TaskIdentity);

impl TaskIdentityResolver for FixedTaskIdentity {
    fn resolve<'a>(
        &'a self,
        _assertion: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<TaskIdentity, TaskAuthenticationError>> + Send + 'a>>
    {
        let identity = self.0.clone();
        Box::pin(async move { Ok(identity) })
    }
}

#[derive(Clone)]
struct ProcessTestExecutionAdapter;

impl TaskExecutionAdapter for ProcessTestExecutionAdapter {
    fn contract(&self) -> &'static str {
        "receiver-process-test-v1"
    }

    fn render(
        &self,
        request: TaskExecutionPlanRequest<'_>,
    ) -> Result<TaskExecutionPlan, PortError> {
        Ok(TaskExecutionPlan {
            command: vec![
                "example-agent".to_owned(),
                request.workflow_prompt.to_owned(),
            ],
        })
    }
}

struct ReceiverEndpoints<'a> {
    kubeconfig_file: &'a Path,
    exchange_endpoint: &'a str,
    exchange_server_name: &'a str,
    api_origin: &'a str,
    api_server_name: &'a str,
}

#[tokio::test]
async fn write_catalog_refuses_startup_and_the_next_signed_delivery() -> Result<(), Box<dyn Error>>
{
    install_rustls_crypto_provider()?;
    let database_url = env::var("STEWARD_TEST_TRIGGER_DATABASE_URL")?;
    let ca_file = PathBuf::from(env::var("STEWARD_TEST_TRIGGER_CA_CERTIFICATE_FILE")?);
    let run_dir = PathBuf::from(env::var("STEWARD_TEST_RUN_DIR")?).join("trigger-write-refusal");
    fs::create_dir_all(&run_dir)?;

    let store = PgStore::connect(&database_url).await?;
    store.migrate().await?;
    let initial_task_count = task_count(&store).await?;
    let suffix = Uuid::new_v4().simple().to_string();
    let seeded = seed_read_envelope(&store, &suffix).await?;
    let config_file = run_dir.join("config").with_extension("json");
    let catalog_file = run_dir.join("capabilities").with_extension("json");
    let signing_file = run_dir.join("signing-secret");
    let kubeconfig_file = run_dir.join("kubeconfig");
    fs::write(&signing_file, SIGNING_SECRET)?;
    fs::write(
        &kubeconfig_file,
        synthetic_kubeconfig("https://127.0.0.1:1"),
    )?;
    fs::write(
        &config_file,
        serde_json::to_vec(&json!({
            "triggers": [{
                "id": "jira-read",
                "enabled": true,
                "source": "jira",
                "workflow": seeded.workflow,
                "service": "event-worker",
                "ownerUserId": seeded.owner_id,
                "envelopeDigest": format!("steward:{}", seeded.digest),
                "signingSecretFile": signing_file,
                "concurrencyLimit": 1,
                "cooldownSeconds": 0
            }]
        }))?,
    )?;
    write_catalog(&catalog_file, "read")?;

    let address = available_address()?;
    let running_log = run_dir.join("running").with_extension("log");
    let endpoints = ReceiverEndpoints {
        kubeconfig_file: &kubeconfig_file,
        exchange_endpoint: "https://exchange.example.test:8443/v1/workload/exchange",
        exchange_server_name: "exchange.example.test",
        api_origin: "https://steward.example.test:8443",
        api_server_name: "steward.example.test",
    };
    let mut receiver = spawn_receiver(
        address,
        &database_url,
        &config_file,
        &catalog_file,
        &ca_file,
        &running_log,
        &endpoints,
    )?;
    wait_for_listener(&mut receiver.child, address, &running_log)?;

    write_catalog(&catalog_file, "write")?;
    let request_body = serde_json::to_vec(&json!({
        "webhookEvent": "jira:issue_updated",
        "issue": {
            "key": "PROJ-123",
            "fields": {
                "project": {"key": "PROJ"},
                "issuetype": {"id": "10001"}
            }
        },
        "user": {"accountId": "acct-1"}
    }))?;
    let response = reqwest::Client::builder()
        .timeout(StdDuration::from_secs(5))
        .build()?
        .post(format!("http://{address}/hooks/jira/jira-read"))
        .header("x-hub-signature", jira_signature(&request_body))
        .body(request_body)
        .send()
        .await?;
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(response.text().await?, "This Trigger is not read-only.");
    assert_eq!(
        task_count(&store).await?,
        initial_task_count,
        "the refused delivery created a Task"
    );
    receiver.stop()?;

    let startup_log = run_dir.join("startup-refusal").with_extension("log");
    let mut refused = spawn_receiver(
        available_address()?,
        &database_url,
        &config_file,
        &catalog_file,
        &ca_file,
        &startup_log,
        &endpoints,
    )?;
    let status = wait_for_exit(&mut refused.child, StdDuration::from_secs(10))?;
    assert!(!status.success(), "a receiver started with a write catalog");
    assert!(
        fs::read_to_string(&startup_log)?.contains("a Trigger Envelope is unavailable or unsafe"),
        "startup refusal did not report the Envelope policy failure"
    );
    assert_eq!(
        task_count(&store).await?,
        initial_task_count,
        "startup refusal created a Task"
    );
    Ok(())
}

#[tokio::test]
async fn exact_replay_on_a_second_receiver_uses_postgres_idempotency() -> Result<(), Box<dyn Error>>
{
    install_rustls_crypto_provider()?;
    let database_url = env::var("STEWARD_TEST_TRIGGER_DATABASE_URL")?;
    let ca_file = PathBuf::from(env::var("STEWARD_TEST_TRIGGER_CA_CERTIFICATE_FILE")?);
    let test_root = PathBuf::from(env::var("STEWARD_TEST_RUN_DIR")?);
    let run_dir = test_root.join("trigger-replay");
    fs::create_dir_all(&run_dir)?;

    let store = PgStore::connect(&database_url).await?;
    store.migrate().await?;
    let suffix = Uuid::new_v4().simple().to_string();
    let seeded = seed_read_envelope(&store, &suffix).await?;
    let config_file = run_dir.join("config").with_extension("json");
    let catalog_file = run_dir.join("capabilities").with_extension("json");
    let signing_file = run_dir.join("signing-secret");
    let kubeconfig_file = run_dir.join("kubeconfig");
    fs::write(&signing_file, SIGNING_SECRET)?;
    fs::write(
        &config_file,
        serde_json::to_vec(&json!({
            "triggers": [{
                "id": "jira-read",
                "enabled": true,
                "source": "jira",
                "workflow": seeded.workflow,
                "service": "event-worker",
                "ownerUserId": seeded.owner_id,
                "envelopeDigest": format!("steward:{}", seeded.digest),
                "signingSecretFile": signing_file,
                "concurrencyLimit": 1,
                "cooldownSeconds": 60
            }]
        }))?,
    )?;
    write_catalog(&catalog_file, "read")?;

    let kube = spawn_kubernetes_api().await?;
    fs::write(
        &kubeconfig_file,
        synthetic_kubeconfig(&format!("http://{}", kube.address)),
    )?;
    let identity = TaskIdentity {
        service: "event-worker".to_owned(),
        acting_user: None,
        owner: Email::parse(seeded.owner_email.clone())?,
        canonical_user_id: CanonicalUserId::parse(seeded.owner_id.clone())?,
        source_provenance: None,
    };
    let app = task_router(
        store.clone(),
        FixedTaskIdentity(identity),
        process_task_config()?,
    )
    .route("/v1/workload/exchange", post(exchange_token));
    let task_api = spawn_tls_app(
        app,
        &test_root.join("server").with_extension("crt"),
        &test_root.join("server").with_extension("key"),
    )
    .await?;
    let api_origin = format!("https://localhost:{}", task_api.address.port());
    let exchange_endpoint = format!("{api_origin}/v1/workload/exchange");
    let endpoints = ReceiverEndpoints {
        kubeconfig_file: &kubeconfig_file,
        exchange_endpoint: &exchange_endpoint,
        exchange_server_name: "localhost",
        api_origin: &api_origin,
        api_server_name: "localhost",
    };

    let first_address = available_address()?;
    let second_address = available_address()?;
    let first_log = run_dir.join("receiver-a").with_extension("log");
    let second_log = run_dir.join("receiver-b").with_extension("log");
    let mut first = spawn_receiver(
        first_address,
        &database_url,
        &config_file,
        &catalog_file,
        &ca_file,
        &first_log,
        &endpoints,
    )?;
    let mut second = spawn_receiver(
        second_address,
        &database_url,
        &config_file,
        &catalog_file,
        &ca_file,
        &second_log,
        &endpoints,
    )?;
    wait_for_listener(&mut first.child, first_address, &first_log)?;
    wait_for_listener(&mut second.child, second_address, &second_log)?;

    let request_body = serde_json::to_vec(&json!({
        "webhookEvent": "jira:issue_updated",
        "issue": {
            "key": "PROJ-123",
            "fields": {
                "project": {"key": "PROJ"},
                "issuetype": {"id": "10001"}
            }
        },
        "user": {"accountId": "acct-1"}
    }))?;
    let first_response = send_delivery(first_address, "delivery-123", &request_body).await?;
    let first_status = first_response.status();
    let first_body = first_response.text().await?;
    assert_eq!(
        first_status,
        reqwest::StatusCode::ACCEPTED,
        "first receiver body: {first_body}; log: {}",
        fs::read_to_string(&first_log).unwrap_or_default()
    );
    assert_eq!(first_body, "The event was accepted.");

    let replay = send_delivery(second_address, "delivery-123", &request_body).await?;
    assert_eq!(replay.status(), reqwest::StatusCode::OK);
    assert_eq!(replay.text().await?, "No run was needed.");
    assert_eq!(
        task_count_for_owner(&store, &seeded.owner_id).await?,
        1,
        "the cross-process replay created a second PostgreSQL Task reservation"
    );
    let requests = task_api
        .requests
        .lock()
        .map_err(|_| io::Error::other("task API request record is unavailable"))?;
    assert_eq!(
        requests
            .iter()
            .filter(|(method, path)| method == "POST" && path == "/v1/tasks")
            .count(),
        2,
        "each receiver must consult the durable Task idempotency fence"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|(method, path)| method == "PUT" && path.ends_with("/inputs"))
            .count(),
        1,
        "the duplicate receiver uploaded Task inputs"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|(method, path)| method == "POST" && path.ends_with("/execute"))
            .count(),
        1,
        "the duplicate receiver executed the existing Task"
    );
    drop(requests);

    let fresh_body = request_body
        .windows(b"acct-1".len())
        .position(|window| window == b"acct-1")
        .map(|index| {
            let mut body = request_body.clone();
            body[index..index + b"acct-1".len()].copy_from_slice(b"acct-2");
            body
        })
        .ok_or_else(|| io::Error::other("Jira account fixture is unavailable"))?;
    let fresh = send_delivery(second_address, "delivery-456", &fresh_body).await?;
    assert_eq!(
        fresh.status(),
        reqwest::StatusCode::ACCEPTED,
        "the duplicate consumed the second receiver cooldown or concurrency slot"
    );
    assert_eq!(fresh.text().await?, "The event was accepted.");
    assert_eq!(task_count_for_owner(&store, &seeded.owner_id).await?, 2);

    first.stop()?;
    second.stop()?;
    Ok(())
}

async fn seed_read_envelope(
    store: &PgStore,
    suffix: &str,
) -> Result<SeededReceiverAuthority, Box<dyn Error>> {
    let policy = OrganizationIdentityPolicy::new(
        GOOGLE_ORGANIZATION_ISSUER,
        "example.com",
        OrganizationId::parse("org_example")?,
    )?;
    let owner = store
        .register_canonical_identity(
            &policy.validate(
                GOOGLE_ORGANIZATION_ISSUER,
                &format!("jira-receiver-{suffix}"),
                "example.com",
                &format!("alice-{suffix}{}example.com", char::from(64_u8)),
                true,
            )?,
            "test-bootstrap",
        )
        .await?;
    let envelope = Envelope {
        revision: 1,
        spec: EnvelopeSpec {
            llms: vec![ModelRef {
                provider: "example".to_owned(),
                model: "model-a".to_owned(),
            }],
            tools: vec![ToolGrant {
                provider: "jira".to_owned(),
                resource: "issue".to_owned(),
                action: "get".to_owned(),
            }],
            budget: Budget {
                monthly_limit: "10.00".to_owned(),
                single_run_limit: Some("1.00".to_owned()),
                currency: "USD".to_owned(),
            },
            runtime_minutes_limit: Some("10".to_owned()),
            workspace: None,
            ttl: Duration("30m".to_owned()),
            runner: RunnerRequirements::default(),
        },
    };
    let template_id = format!("jira-read-{suffix}");
    let roles = vec!["engineer".to_owned()];
    store
        .insert_envelope_template_revision(EnvelopeTemplatePublication {
            template_id: &template_id,
            display_name: "Jira read",
            member_roles: &roles,
            ceiling: &envelope,
            auto_provision_threshold: None,
            allow_inline_browser_tasks: false,
            authored_by: "test-bootstrap",
        })
        .await?;
    let request = store
        .reserve_envelope_request(EnvelopeRequestReservationRequest {
            owner_user_id: &owner.user_id,
            template_id: Some(&template_id),
            template_revision: Some(envelope.revision),
            requested_envelope: &envelope,
            idempotency_key: &format!("jira-read-envelope-{suffix}"),
            justification: None,
            ceiling_exceedances: &[],
            actor: "test-bootstrap",
        })
        .await?
        .record;
    let digest = format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&envelope)?)
    );
    let instance = format!("env_{}", request.id.simple());
    store
        .append_envelope_request_status(
            request.id,
            EnvelopeRequestStatusUpdate {
                from: EnvelopeRequestStatus::Pending,
                to: EnvelopeRequestStatus::Provisioned,
                approval_id: Some(Uuid::new_v4()),
                envelope_instance_id: Some(&instance),
                envelope_digest: Some(&digest),
                reason: None,
                rationale: Some("receiver process test"),
                evidence_url: None,
                expires_at: None,
                approved_envelope: Some(&envelope),
                actor: "test-bootstrap",
            },
        )
        .await?;
    let workflow = format!("triage-{suffix}");
    store
        .publish_initial_workflow(WorkflowPublication {
            name: &workflow,
            display_name: "Jira triage",
            agent: "example-agent@1",
            prompt: "Review the Jira event.",
            content_digest: &format!("sha256:{:x}", Sha256::digest(workflow.as_bytes())),
            published_by: "test-bootstrap",
        })
        .await?;
    Ok(SeededReceiverAuthority {
        owner_id: owner.user_id.as_str().to_owned(),
        owner_email: owner.display_email.as_str().to_owned(),
        digest,
        workflow: format!("{workflow}{}1", char::from(64_u8)),
    })
}

fn process_task_config() -> Result<TaskApiConfig, Box<dyn Error>> {
    let bindings = json!({
        "apiVersion": "steward.execution-bindings/v2",
        "bindings": [{
            "agentRef": "example-agent@1",
            "displayName": "Example agent",
            "adapter": "receiver-process-test-v1",
            "image": format!(
                "registry.example.test/steward/agent{}sha256:{}",
                char::from(64_u8),
                "a".repeat(64)
            ),
            "executable": "/opt/steward/example-agent",
            "versionProbe": {
                "arguments": ["--version"],
                "expectedStdout": "example-agent 1"
            },
            "models": {"providers": ["example"]},
            "providerProfiles": {
                "tools": {
                    "id": "example-tools-profile-v1",
                    "digest": format!("sha256:{}", "c".repeat(64))
                },
                "inference": {
                    "id": "example-inference-profile-v1",
                    "digest": format!("sha256:{}", "b".repeat(64))
                }
            }
        }]
    })
    .to_string();
    Ok(
        TaskApiConfig::new(Some("https://mcp-gw.example.test/mcp".to_owned()))?
            .with_execution_adapter(Arc::new(ProcessTestExecutionAdapter))?
            .with_execution_bindings_json(Some(&bindings))?
            .with_execution_bindings_active(true)?
            .with_task_orchestration_mode(TaskOrchestrationMode::Active),
    )
}

async fn exchange_token() -> impl IntoResponse {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "access_token": "obviously-fake-task-token",
            "token_type": "Bearer",
            "expires_in": 120
        })),
    )
}

async fn spawn_kubernetes_api() -> Result<TestServer, Box<dyn Error>> {
    let app = Router::new().fallback(post(|| async {
        (
            StatusCode::CREATED,
            Json(json!({
                "apiVersion": ["authentication", "k8s", "io"].join(".") + "/v1",
                "kind": "TokenRequest",
                "metadata": {},
                "spec": {
                    "audiences": ["steward-workload-exchange"],
                    "expirationSeconds": 600
                },
                "status": {
                    "expirationTimestamp": "2030-01-01T00:00:00Z",
                    "token": "obviously-fake-source-token"
                }
            })),
        )
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(TestServer {
        address,
        handle,
        requests: Arc::new(Mutex::new(Vec::new())),
    })
}

async fn spawn_tls_app(
    app: Router,
    certificate_file: &Path,
    private_key_file: &Path,
) -> Result<TestServer, Box<dyn Error>> {
    let certificate_bytes = fs::read(certificate_file)?;
    let certificates =
        CertificateDer::pem_slice_iter(&certificate_bytes).collect::<Result<Vec<_>, _>>()?;
    let private_key_bytes = fs::read(private_key_file)?;
    let private_key = PrivateKeyDer::from_pem_slice(&private_key_bytes)?;
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, private_key)?;
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured_requests = requests.clone();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            let app = app.clone();
            let requests = captured_requests.clone();
            tokio::spawn(async move {
                let Ok(stream) = acceptor.accept(stream).await else {
                    return;
                };
                let _ = serve_one_http_request(stream, app, requests).await;
            });
        }
    });
    Ok(TestServer {
        address,
        handle,
        requests,
    })
}

async fn serve_one_http_request(
    mut stream: tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
    app: Router,
    requests: Arc<Mutex<Vec<(String, String)>>>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    const MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024;
    let mut request_bytes = Vec::new();
    let (header_end, content_length) = loop {
        let mut buffer = [0_u8; 8192];
        let read = stream.read(&mut buffer).await?;
        if read == 0 || request_bytes.len().saturating_add(read) > MAX_REQUEST_BYTES {
            return Err(io::Error::other("test HTTPS request is incomplete or too large").into());
        }
        request_bytes.extend_from_slice(&buffer[..read]);
        let Some(header_end) = request_bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| index + 4)
        else {
            continue;
        };
        let headers = std::str::from_utf8(&request_bytes[..header_end])?;
        let content_length = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .map(|(_, value)| value.trim().parse::<usize>())
            .transpose()?
            .unwrap_or(0);
        if content_length > MAX_REQUEST_BYTES.saturating_sub(header_end) {
            return Err(io::Error::other("test HTTPS request body is too large").into());
        }
        if request_bytes.len() >= header_end + content_length {
            break (header_end, content_length);
        }
    };
    let head = std::str::from_utf8(&request_bytes[..header_end])?;
    let mut lines = head.lines();
    let request_line = lines
        .next()
        .ok_or_else(|| io::Error::other("test HTTPS request line is missing"))?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts
        .next()
        .ok_or_else(|| io::Error::other("test HTTPS request method is missing"))?;
    let uri = request_parts
        .next()
        .ok_or_else(|| io::Error::other("test HTTPS request URI is missing"))?;
    requests
        .lock()
        .map_err(|_| io::Error::other("test HTTPS request record is unavailable"))?
        .push((method.to_owned(), uri.to_owned()));
    let mut request = Request::builder().method(method).uri(uri);
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| io::Error::other("test HTTPS request header is invalid"))?;
        request = request.header(name.trim(), value.trim());
    }
    let body = request_bytes[header_end..header_end + content_length].to_vec();
    let response = app.oneshot(request.body(Body::from(body))?).await?;
    let (parts, body) = response.into_parts();
    let body = to_bytes(body, MAX_REQUEST_BYTES).await?;
    let reason = parts.status.canonical_reason().unwrap_or("Test");
    let mut response_head = format!("HTTP/1.1 {} {reason}\r\n", parts.status.as_u16());
    for (name, value) in &parts.headers {
        if matches!(
            name.as_str(),
            "content-length" | "connection" | "transfer-encoding"
        ) {
            continue;
        }
        response_head.push_str(name.as_str());
        response_head.push_str(": ");
        response_head.push_str(value.to_str()?);
        response_head.push_str("\r\n");
    }
    response_head.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ));
    stream.write_all(response_head.as_bytes()).await?;
    stream.write_all(&body).await?;
    stream.shutdown().await?;
    Ok(())
}

fn spawn_receiver(
    address: SocketAddr,
    database_url: &str,
    config_file: &Path,
    catalog_file: &Path,
    ca_file: &Path,
    log_file: &Path,
    endpoints: &ReceiverEndpoints<'_>,
) -> Result<ReceiverProcess, Box<dyn Error>> {
    let output = File::create(log_file)?;
    let child = Command::new(env!("CARGO_BIN_EXE_steward-trigger-receiver"))
        .env_clear()
        .env("KUBECONFIG", endpoints.kubeconfig_file)
        .env("STEWARD_TRIGGER_CONFIG_FILE", config_file)
        .env("STEWARD_CAPABILITY_CATALOG_FILE", catalog_file)
        .env("STEWARD_DATABASE_URL", database_url)
        .env("STEWARD_TRIGGER_NAMESPACE", "steward-test")
        .env(
            "STEWARD_TRIGGER_SERVICE_ACCOUNT",
            "steward-trigger-receiver",
        )
        .env("STEWARD_TRIGGER_BIND", address.to_string())
        .env(
            "STEWARD_WORKLOAD_SOURCE_AUDIENCE",
            "steward-workload-exchange",
        )
        .env(
            "STEWARD_WORKLOAD_EXCHANGE_ENDPOINT",
            endpoints.exchange_endpoint,
        )
        .env(
            "STEWARD_WORKLOAD_EXCHANGE_SERVER_NAME",
            endpoints.exchange_server_name,
        )
        .env("STEWARD_WORKLOAD_EXCHANGE_CA_CERTIFICATE_FILE", ca_file)
        .env("STEWARD_API_ORIGIN", endpoints.api_origin)
        .env("STEWARD_API_SERVER_NAME", endpoints.api_server_name)
        .env("STEWARD_API_CA_CERTIFICATE_FILE", ca_file)
        .envs(receiver_limits())
        .stdout(Stdio::from(output.try_clone()?))
        .stderr(Stdio::from(output))
        .spawn()?;
    Ok(ReceiverProcess { child })
}

fn receiver_limits() -> [(&'static str, &'static str); 21] {
    [
        ("STEWARD_TRIGGER_MAX_CONFIG_BYTES", "1048576"),
        ("STEWARD_TRIGGER_MAX_BODY_BYTES", "1048576"),
        ("STEWARD_TRIGGER_MAX_IDENTIFIER_BYTES", "1024"),
        ("STEWARD_TRIGGER_MAX_TRIGGERS", "16"),
        ("STEWARD_TRIGGER_MAX_IGNORED_ACCOUNTS", "16"),
        ("STEWARD_TRIGGER_MAX_CONCURRENCY", "16"),
        ("STEWARD_TRIGGER_MAX_COOLDOWN_SECONDS", "3600"),
        ("STEWARD_TRIGGER_MAX_RECENT_DELIVERIES", "100"),
        ("STEWARD_TRIGGER_MAX_ISSUE_COOLDOWNS", "100"),
        ("STEWARD_TRIGGER_MAX_SIGNING_SECRET_BYTES", "4096"),
        (
            "STEWARD_TRIGGER_DATABASE_ACQUIRE_TIMEOUT_MILLISECONDS",
            "5000",
        ),
        ("STEWARD_WORKLOAD_SOURCE_TOKEN_SECONDS", "600"),
        (
            "STEWARD_WORKLOAD_EXCHANGE_CONNECT_TIMEOUT_MILLISECONDS",
            "1000",
        ),
        (
            "STEWARD_WORKLOAD_EXCHANGE_REQUEST_TIMEOUT_MILLISECONDS",
            "2000",
        ),
        ("STEWARD_WORKLOAD_EXCHANGE_MAX_REPLY_BYTES", "65536"),
        ("STEWARD_TRIGGER_TASK_DEADLINE_MILLISECONDS", "10000"),
        ("STEWARD_TRIGGER_POLL_INITIAL_MILLISECONDS", "10"),
        ("STEWARD_TRIGGER_POLL_MAX_MILLISECONDS", "100"),
        ("STEWARD_TRIGGER_API_CONNECT_TIMEOUT_MILLISECONDS", "1000"),
        ("STEWARD_TRIGGER_API_REQUEST_TIMEOUT_MILLISECONDS", "2000"),
        ("STEWARD_TRIGGER_MAX_TASK_REPLY_BYTES", "1048576"),
    ]
}

fn write_catalog(path: &Path, access_class: &str) -> Result<(), Box<dyn Error>> {
    fs::write(
        path,
        serde_json::to_vec(&json!({
            "schemaVersion": "steward.capability-catalog/v2",
            "models": [],
            "tools": [{
                "provider": "jira",
                "resource": "issue",
                "action": "get",
                "accessClass": access_class
            }],
            "catalogs": []
        }))?,
    )?;
    Ok(())
}

fn jira_signature(body: &[u8]) -> String {
    let tag = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, SIGNING_SECRET), body);
    let hex = tag
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("sha256={hex}")
}

async fn task_count(store: &PgStore) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM task_submissions")
        .fetch_one(store.pool())
        .await
}

async fn task_count_for_owner(store: &PgStore, owner_user_id: &str) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM task_submissions WHERE owner_user_id = $1")
        .bind(owner_user_id)
        .fetch_one(store.pool())
        .await
}

async fn send_delivery(
    address: SocketAddr,
    delivery_id: &str,
    body: &[u8],
) -> Result<reqwest::Response, reqwest::Error> {
    reqwest::Client::builder()
        .timeout(StdDuration::from_secs(10))
        .build()?
        .post(format!("http://{address}/hooks/jira/jira-read"))
        .header("x-atlassian-webhook-identifier", delivery_id)
        .header("x-hub-signature", jira_signature(body))
        .body(body.to_vec())
        .send()
        .await
}

fn available_address() -> io::Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.local_addr()
}

fn wait_for_listener(
    child: &mut Child,
    address: SocketAddr,
    log: &Path,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + StdDuration::from_secs(10);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            return Err(io::Error::other(format!(
                "receiver exited before listening ({status}): {}",
                fs::read_to_string(log).unwrap_or_default()
            ))
            .into());
        }
        if TcpStream::connect_timeout(&address, StdDuration::from_millis(100)).is_ok() {
            return Ok(());
        }
        thread::sleep(StdDuration::from_millis(50));
    }
    Err(io::Error::other("receiver did not start before the deadline").into())
}

fn wait_for_exit(child: &mut Child, timeout: StdDuration) -> Result<ExitStatus, Box<dyn Error>> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        thread::sleep(StdDuration::from_millis(50));
    }
    Err(io::Error::other("receiver did not refuse startup before the deadline").into())
}

fn synthetic_kubeconfig(server: &str) -> String {
    format!(
        "apiVersion: v1\nkind: Config\nclusters:\n  - name: synthetic\n    cluster:\n      server: {server}\n      insecure-skip-tls-verify: true\ncontexts:\n  - name: synthetic\n    context:\n      cluster: synthetic\n      user: synthetic\ncurrent-context: synthetic\nusers:\n  - name: synthetic\n    user:\n      token: synthetic-token\n"
    )
}

fn install_rustls_crypto_provider() -> Result<(), io::Error> {
    use tokio_rustls::rustls::crypto::{CryptoProvider, ring};

    if CryptoProvider::get_default().is_none() {
        let _ = ring::default_provider().install_default();
    }
    CryptoProvider::get_default()
        .map(|_| ())
        .ok_or_else(|| io::Error::other("Rustls crypto provider is unavailable"))
}
