use std::env;
use std::error::Error;
use std::fs;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use kube::Client;
use sqlx::postgres::PgPoolOptions;
use sqlx::types::Uuid;
use sqlx::{PgPool, Row};
use steward_adapter_openshell::{
    OpenShellConnectionConfig, OpenShellRuntime, OpenShellTaskLogMode,
};
use steward_admission::{AdmissionDecision, Envelope, EnvelopeSpec, evaluate, validate_envelope};
use steward_apiserver::connections::{
    ConnectionBrokerError, ConnectionPhase, ConnectionSession, ConnectionSubject,
    ProviderConnectionBroker,
};
use steward_apiserver::governed_connections::{
    ConnectionExecutionBindings, ConnectionOperationReconciler, GovernedConnectionsBroker,
    GovernedConnectionsConfig,
};
use steward_ports::{
    InferenceCapabilities, InferenceObservation, InferencePlane, InferenceRequest, PortError,
    ProvisionedInference,
};
use steward_store::{AgentRunQuery, PgStore};
use steward_types::{
    AgentRuntimeSpec, Budget, CanonicalUserId, Duration as StewardDuration,
    GOOGLE_ORGANIZATION_ISSUER, ModelRef, OrganizationId, OrganizationIdentityPolicy,
    RunnerRequirements, ToolGrant,
};
use tokio::task::JoinHandle;

const CONNECTIONS_NAMESPACE: &str = "steward-connections";
const ALICE_NAMESPACE: &str = "team-a";
const BOB_NAMESPACE: &str = "team-b";
const ALICE_RUNTIME: &str = "long-running-alice";
const ALICE_AFTER_DISCONNECT_RUNTIME: &str = "new-alice-after-disconnect";
const BOB_RUNTIME: &str = "long-running-bob";
const MCP_GW_ISSUER: &str = "http://steward-mint.steward-system.svc.cluster.local:8080";
const MCP_GW_ORIGIN: &str = "http://mcp-gw.steward-system.svc.cluster.local:8080";
const MCP_URL: &str = "http://mcp-gw.steward-system.svc.cluster.local:8080/mcp";

#[derive(Clone, Copy)]
struct NoInference;

impl InferencePlane for NoInference {
    fn capabilities(&self) -> InferenceCapabilities {
        InferenceCapabilities::default()
    }

    async fn validate_configuration(
        &self,
        models: &[ModelRef],
        _budget: &Budget,
    ) -> Result<(), PortError> {
        if models.is_empty() {
            Ok(())
        } else {
            Err(PortError::Unsupported {
                operation: "governed Connections test inference",
            })
        }
    }

    async fn provision(
        &self,
        _request: &InferenceRequest,
    ) -> Result<ProvisionedInference, PortError> {
        Err(PortError::Unsupported {
            operation: "governed Connections test inference",
        })
    }

    async fn reconcile_configuration(&self, _request: &InferenceRequest) -> Result<(), PortError> {
        Ok(())
    }

    async fn observe(
        &self,
        _request: &InferenceRequest,
    ) -> Result<InferenceObservation, PortError> {
        Ok(InferenceObservation::Absent)
    }

    async fn revoke(&self, _request: &InferenceRequest) -> Result<(), PortError> {
        Ok(())
    }
}

struct Harness {
    bridge_image: String,
    client: Client,
    context: String,
    controller: Option<JoinHandle<()>>,
    database: PgPool,
    kubeconfig: PathBuf,
    mcp_forward: String,
    openshell: PathBuf,
    reconciler: Option<JoinHandle<()>>,
    run_dir: PathBuf,
    runtime: OpenShellRuntime,
    store: PgStore,
}

impl Harness {
    async fn from_environment() -> Result<Self, Box<dyn Error>> {
        if required("STEWARD_OPEN_SHELL_RELEASE")? != "v0.0.98" {
            return Err(
                io::Error::other("governed Connections E2E requires OpenShell v0.0.98").into(),
            );
        }
        let context = required("STEWARD_TEST_KUBE_CONTEXT")?;
        if !context.starts_with("kind-steward-") {
            return Err(io::Error::other(format!(
                "refusing non-ephemeral kube context: {context}"
            ))
            .into());
        }
        let database_url = required("STEWARD_CONNECTIONS_TEST_DATABASE_URL")?;
        let database = PgPoolOptions::new()
            .max_connections(8)
            .connect(&database_url)
            .await?;
        let store = PgStore::new(database.clone());
        store.migrate().await?;
        let bridge_image = required("STEWARD_CONNECTIONS_TEST_BRIDGE_DIGEST_IMAGE")?;
        let runtime = OpenShellRuntime::connect(OpenShellConnectionConfig {
            endpoint: required("STEWARD_OPENSHELL_ENDPOINT")?,
            ca_certificate_pem: required_file("STEWARD_OPENSHELL_CA_CERTIFICATE_FILE")?,
            client_certificate_pem: required_file("STEWARD_OPENSHELL_CLIENT_CERTIFICATE_FILE")?,
            client_private_key_pem: required_file("STEWARD_OPENSHELL_CLIENT_PRIVATE_KEY_FILE")?,
            workload_exchange_endpoint: required("STEWARD_WORKLOAD_EXCHANGE_ENDPOINT")?,
            workload_exchange_server_name: required("STEWARD_WORKLOAD_EXCHANGE_SERVER_NAME")?,
            workload_exchange_ca_certificate_pem: required_file(
                "STEWARD_WORKLOAD_EXCHANGE_CA_CERTIFICATE_FILE",
            )?,
            workload_source_credential_file: PathBuf::from(required(
                "STEWARD_WORKLOAD_SOURCE_CREDENTIAL_FILE",
            )?),
            server_name: required("STEWARD_OPENSHELL_SERVER_NAME")?,
            runtime_class_name: required("STEWARD_OPENSHELL_RUNTIME_CLASS_NAME")?,
            task_log_mode: OpenShellTaskLogMode::Full,
            stable_bridge_image: None,
            stable_bridge_gateway_origin: None,
            bridge_image: Some(bridge_image.clone()),
            bridge_gateway_origin: Some(MCP_GW_ORIGIN.to_owned()),
            bridge_gateway_version: Some("0.3.2".to_owned()),
            bridge_runtime_namespace: Some(CONNECTIONS_NAMESPACE.to_owned()),
        })
        .await
        .map_err(|error| io::Error::other(format!("connect OpenShell: {error:?}")))?;
        let client = Client::try_default().await?;
        let mut harness = Self {
            bridge_image,
            client,
            context,
            controller: None,
            database,
            kubeconfig: PathBuf::from(required("STEWARD_TEST_KUBECONFIG")?),
            mcp_forward: required("STEWARD_CONNECTIONS_TEST_MCP_FORWARD")?,
            openshell: PathBuf::from(required("STEWARD_OPENSHELL_CLI")?),
            reconciler: None,
            run_dir: PathBuf::from(required("STEWARD_RUN_DIR")?),
            runtime,
            store,
        };
        harness.start_controller();
        harness.start_reconciler();
        Ok(harness)
    }

    fn start_controller(&mut self) {
        let client = self.client.clone();
        let runtime = self.runtime.clone();
        let store = self.store.clone();
        self.controller = Some(tokio::spawn(async move {
            steward_controller::run_controller_with_planes(client, runtime, NoInference, store)
                .await;
        }));
    }

    fn restart_controller(&mut self) {
        if let Some(controller) = self.controller.take() {
            controller.abort();
        }
        self.start_controller();
    }

    fn start_reconciler(&mut self) {
        let reconciler = ConnectionOperationReconciler::new(self.store.clone());
        self.reconciler = Some(tokio::spawn(reconciler.run()));
    }

    fn restart_reconciler(&mut self) {
        if let Some(reconciler) = self.reconciler.take() {
            reconciler.abort();
        }
        self.start_reconciler();
    }

    fn broker(&self) -> Result<GovernedConnectionsBroker<()>, Box<dyn Error>> {
        let config = GovernedConnectionsConfig::new(
            ConnectionExecutionBindings {
                bridge_image_digest: self.bridge_image.clone(),
                mcp_gw_origin: MCP_GW_ORIGIN.to_owned(),
                mcp_gw_version: "0.3.2".to_owned(),
                namespace: CONNECTIONS_NAMESPACE.to_owned(),
                runtime_class: required("STEWARD_OPENSHELL_RUNTIME_CLASS_NAME")?,
            },
            "https://steward.example.test",
        )
        .map_err(|error| io::Error::other(format!("build governed broker: {error:?}")))?;
        Ok(GovernedConnectionsBroker::new(self.store.clone(), config))
    }

    async fn register_user(
        &self,
        subject: &str,
        hosted_domain: &str,
        email: &str,
    ) -> Result<CanonicalUserId, Box<dyn Error>> {
        let identity = OrganizationIdentityPolicy::new(
            GOOGLE_ORGANIZATION_ISSUER,
            hosted_domain,
            OrganizationId::parse("org_example")?,
        )?
        .validate(
            GOOGLE_ORGANIZATION_ISSUER,
            subject,
            hosted_domain,
            email,
            true,
        )?;
        Ok(self
            .store
            .register_canonical_identity(&identity, "identity-admin")
            .await?
            .user_id)
    }

    fn session(user_id: CanonicalUserId, email: &str) -> ConnectionSession<()> {
        ConnectionSession {
            subject: ConnectionSubject {
                canonical_user_id: user_id,
                display_email: email.to_owned(),
            },
            binding: (),
        }
    }

    fn kubectl(&self, arguments: &[&str]) -> Result<Output, Box<dyn Error>> {
        Ok(Command::new("kubectl")
            .args(["--kubeconfig"])
            .arg(&self.kubeconfig)
            .args(["--context", &self.context])
            .args(arguments)
            .output()?)
    }

    fn kubectl_ok(&self, arguments: &[&str]) -> Result<String, Box<dyn Error>> {
        let output = self.kubectl(arguments)?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "kubectl {} failed: {}",
                arguments.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ))
            .into());
        }
        Ok(String::from_utf8(output.stdout)?)
    }

    fn write_runtime(
        &self,
        namespace: &str,
        name: &str,
        user_id: &CanonicalUserId,
        email: &str,
        member_role: &str,
        envelope: &Envelope,
    ) -> Result<(), Box<dyn Error>> {
        let manifest_path = self
            .run_dir
            .join(format!("governed-connections-{name}.json"));
        let spec: AgentRuntimeSpec = serde_json::from_value(serde_json::json!({
                "principal": {"kind": "user", "actingUser": email},
                "owner": email,
                "canonicalAuthority": {
                    "schemaVersion": "steward/canonical-authority-binding/v1",
                    "ownerUserId": user_id.as_str(),
                    "actingUserId": user_id.as_str()
                },
                "agentType": {"name": "base"},
                "llms": [],
                "tools": [{
                    "provider": "github",
                    "resource": "get_file_contents",
                    "action": "read"
                }],
                "budget": {"monthlyLimit": "1.00", "currency": "USD"},
                "ttl": "1h"
        }))?;
        let decision = evaluate(&spec, envelope).map_err(|error| {
            io::Error::other(format!("evaluate long-running runtime: {error:?}"))
        })?;
        if decision != AdmissionDecision::Admit {
            return Err(io::Error::other(format!(
                "long-running runtime did not pass Steward admission: {decision:?}"
            ))
            .into());
        }
        let manifest = serde_json::json!({
            "apiVersion": required("STEWARD_AGENTRUNTIME_API_VERSION")?,
            "kind": "AgentRuntime",
            "metadata": {
                "name": name,
                "namespace": namespace,
                "annotations": {"agents.apelogic.ai/member-role": member_role}
            },
            "spec": spec
        });
        fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;
        self.kubectl_ok(&["apply", "-f", path_text(&manifest_path)?])?;
        Ok(())
    }

    async fn seed_long_running_envelope(
        &self,
        member_role: &str,
    ) -> Result<Envelope, Box<dyn Error>> {
        let envelope = Envelope {
            revision: 1,
            spec: EnvelopeSpec {
                llms: Vec::new(),
                tools: vec![ToolGrant {
                    provider: "github".to_owned(),
                    resource: "get_file_contents".to_owned(),
                    action: "read".to_owned(),
                }],
                budget: Budget {
                    monthly_limit: "1.00".to_owned(),
                    single_run_limit: None,
                    currency: "USD".to_owned(),
                },
                ttl: StewardDuration("1h".to_owned()),
                runner: RunnerRequirements::default(),
            },
        };
        validate_envelope(&envelope).map_err(|error| {
            io::Error::other(format!("validate long-running envelope: {error:?}"))
        })?;
        self.store
            .insert_envelope(member_role, &envelope, "e2e-admin")
            .await?;
        Ok(envelope)
    }

    fn wait_runtime_phase(
        &self,
        namespace: &str,
        name: &str,
        expected: &str,
        timeout: Duration,
    ) -> Result<(), Box<dyn Error>> {
        let deadline = Instant::now() + timeout;
        let mut last = String::new();
        while Instant::now() < deadline {
            let output = self.kubectl(&[
                "-n",
                namespace,
                "get",
                "agentruntime",
                name,
                "-o",
                "jsonpath={.status.phase}",
            ])?;
            if output.status.success() {
                last = String::from_utf8(output.stdout)?;
                if last == expected {
                    return Ok(());
                }
                if last == "Failed" {
                    return Err(io::Error::other(format!(
                        "AgentRuntime {namespace}/{name} failed"
                    ))
                    .into());
                }
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        Err(io::Error::other(format!(
            "AgentRuntime {namespace}/{name} did not reach {expected}; last={last:?}"
        ))
        .into())
    }

    fn runtime_ref(
        &self,
        namespace: &str,
        name: &str,
        field: &str,
    ) -> Result<String, Box<dyn Error>> {
        Ok(self
            .kubectl_ok(&[
                "-n",
                namespace,
                "get",
                "agentruntime",
                name,
                "-o",
                &format!("jsonpath={{.status.refs.{field}}}"),
            ])?
            .trim()
            .to_owned())
    }

    fn call_tool(&self, namespace: &str, name: &str) -> Result<String, Box<dyn Error>> {
        let workspace = self.runtime_ref(namespace, name, "workspace")?;
        let sandbox = self.runtime_ref(namespace, name, "sandbox")?;
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "get_file_contents",
                "arguments": {"owner": "example-org", "repo": "fixture", "path": "README.md"}
            }
        })
        .to_string();
        let output = Command::new(&self.openshell)
            .args(["--gateway-endpoint"])
            .arg(required("STEWARD_OPENSHELL_ENDPOINT")?)
            .args([
                "--workspace",
                &workspace,
                "sandbox",
                "exec",
                "--name",
                &sandbox,
            ])
            .args([
                "--no-tty",
                "--",
                "curl",
                "-sS",
                "--max-time",
                "20",
                "-H",
                "Content-Type: application/json",
                "-H",
                "Accept: application/json, text/event-stream",
                "-H",
                "MCP-Protocol-Version: 2025-06-18",
                "-d",
                &request,
                MCP_URL,
            ])
            .output()?;
        Ok(format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ))
    }

    fn wait_tool_contains(
        &self,
        namespace: &str,
        name: &str,
        expected: &str,
        timeout: Duration,
    ) -> Result<Duration, Box<dyn Error>> {
        let started = Instant::now();
        let mut last = String::new();
        while started.elapsed() < timeout {
            last = self.call_tool(namespace, name)?;
            if last.contains(expected) {
                return Ok(started.elapsed());
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        Err(io::Error::other(format!(
            "tool result for {namespace}/{name} did not contain {expected:?}; last={last}"
        ))
        .into())
    }

    async fn latest_operation(
        &self,
        user_id: &CanonicalUserId,
        kind: &str,
        after_count: i64,
    ) -> Result<Uuid, Box<dyn Error>> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let rows = sqlx::query(
                "SELECT operation_id FROM connection_operations \
                 WHERE canonical_user_id = $1 AND operation_kind = $2 \
                 ORDER BY created_at DESC",
            )
            .bind(user_id.as_str())
            .bind(kind)
            .fetch_all(&self.database)
            .await?;
            if i64::try_from(rows.len())? > after_count {
                return Ok(rows[0].try_get("operation_id")?);
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other(format!(
                    "no new {kind} connection operation was durably reserved"
                ))
                .into());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn operation_count(
        &self,
        user_id: &CanonicalUserId,
        kind: &str,
    ) -> Result<i64, Box<dyn Error>> {
        Ok(sqlx::query_scalar(
            "SELECT count(*) FROM connection_operations \
             WHERE canonical_user_id = $1 AND operation_kind = $2",
        )
        .bind(user_id.as_str())
        .bind(kind)
        .fetch_one(&self.database)
        .await?)
    }

    async fn wait_operation_finalized(
        &self,
        operation_id: Uuid,
        timeout: Duration,
    ) -> Result<(), Box<dyn Error>> {
        let deadline = Instant::now() + timeout;
        loop {
            let state: Option<(String, bool)> = sqlx::query_as(
                "SELECT operations.finalization_state, tasks.finalized \
                 FROM connection_operations operations \
                 JOIN task_submissions tasks ON tasks.task_uid = operations.task_uid \
                 WHERE operations.operation_id = $1",
            )
            .bind(operation_id)
            .fetch_optional(&self.database)
            .await?;
            if state == Some(("finalized".to_owned(), true)) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other(format!(
                    "connection operation {operation_id} did not finalize"
                ))
                .into());
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn capture_bridge_refs(
        &self,
        operation_id: Uuid,
    ) -> Result<(String, String, String), Box<dyn Error>> {
        let runtime_name: String = sqlx::query_scalar(
            "SELECT tasks.runtime_name FROM task_submissions tasks \
             JOIN connection_operations operations ON operations.task_uid = tasks.task_uid \
             WHERE operations.operation_id = $1",
        )
        .bind(operation_id)
        .fetch_one(&self.database)
        .await?;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let output = self.kubectl(&[
                "-n",
                CONNECTIONS_NAMESPACE,
                "get",
                "agentruntime",
                &runtime_name,
                "-o",
                "jsonpath={.status.refs.workspace}{'\\n'}{.status.refs.sandbox}{'\\n'}{.metadata.uid}",
            ])?;
            if output.status.success() {
                let values = String::from_utf8(output.stdout)?;
                let mut lines = values.lines();
                if let (Some(workspace), Some(sandbox), Some(uid)) =
                    (lines.next(), lines.next(), lines.next())
                    && !workspace.is_empty()
                    && !sandbox.is_empty()
                    && !uid.is_empty()
                {
                    return Ok((workspace.to_owned(), sandbox.to_owned(), uid.to_owned()));
                }
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other("bridge runtime refs were not observable").into());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    fn assert_bridge_runtime_absent(
        &self,
        workspace: &str,
        sandbox: &str,
        runtime_uid: &str,
    ) -> Result<(), Box<dyn Error>> {
        let runtime = self.kubectl_ok(&[
            "-n",
            CONNECTIONS_NAMESPACE,
            "get",
            "agentruntime",
            "-o",
            "name",
        ])?;
        if !runtime.trim().is_empty() {
            return Err(io::Error::other(format!(
                "governed bridge AgentRuntime survived finalization: {}",
                runtime.trim()
            ))
            .into());
        }
        let sandboxes = self.kubectl_ok(&[
            "-n",
            "openshell",
            "get",
            "sandboxes.agents.x-k8s.io",
            "--selector",
            &format!(
                "openshell.ai/sandbox-workspace={workspace},openshell.ai/sandbox-name={sandbox}"
            ),
            "-o",
            "name",
        ])?;
        if !sandboxes.trim().is_empty() {
            return Err(io::Error::other(format!(
                "bridge OpenShell sandbox survived finalization: {}",
                sandboxes.trim()
            ))
            .into());
        }
        let secret = self.kubectl(&["-n", CONNECTIONS_NAMESPACE, "get", "secret", runtime_uid])?;
        if secret.status.success() {
            return Err(
                io::Error::other("bridge runtime-scoped Secret survived finalization").into(),
            );
        }
        Ok(())
    }

    fn callback(&self, state: &str) -> Result<(), Box<dyn Error>> {
        let mut stream = TcpStream::connect(&self.mcp_forward)?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        let request = format!(
            "GET /oauth/github/callback?code=fixture-code&state={state} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(request.as_bytes())?;
        let mut response = Vec::new();
        let mut chunk = [0_u8; 4096];
        while !response.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut chunk)?;
            if read == 0 {
                break;
            }
            response.extend_from_slice(&chunk[..read]);
            if response.len() > 32 * 1024 {
                return Err(
                    io::Error::other("pinned MCP-GW callback response is oversized").into(),
                );
            }
        }
        let response = String::from_utf8_lossy(&response);
        if !response.starts_with("HTTP/1.1 302")
            || !response
                .to_ascii_lowercase()
                .contains("location: https://steward.example.test/connections#github-connected")
        {
            return Err(io::Error::other("pinned MCP-GW callback did not complete safely").into());
        }
        Ok(())
    }

    fn delete_runtime(&self, namespace: &str, name: &str) {
        let _ = self.kubectl(&[
            "-n",
            namespace,
            "delete",
            "agentruntime",
            name,
            "--ignore-not-found=true",
            "--wait=true",
            "--timeout=120s",
        ]);
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.delete_runtime(ALICE_NAMESPACE, ALICE_AFTER_DISCONNECT_RUNTIME);
        self.delete_runtime(BOB_NAMESPACE, BOB_RUNTIME);
        self.delete_runtime(ALICE_NAMESPACE, ALICE_RUNTIME);
        if let Some(reconciler) = self.reconciler.take() {
            reconciler.abort();
        }
        if let Some(controller) = self.controller.take() {
            controller.abort();
        }
    }
}

fn required(name: &str) -> Result<String, io::Error> {
    env::var(name).map_err(|_| io::Error::other(format!("{name} is required")))
}

fn required_file(name: &str) -> Result<Vec<u8>, io::Error> {
    fs::read(required(name)?).map_err(|error| io::Error::other(format!("read {name}: {error}")))
}

fn path_text(path: &Path) -> Result<&str, io::Error> {
    path.to_str()
        .ok_or_else(|| io::Error::other("test path is not UTF-8"))
}

fn oauth_state(authorization_url: &str) -> Result<&str, io::Error> {
    authorization_url
        .split_once("state=")
        .map(|(_, value)| value.split('&').next().unwrap_or_default())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| io::Error::other("authorization URL omitted its opaque state"))
}

fn connection_result<T>(result: Result<T, ConnectionBrokerError>) -> Result<T, io::Error> {
    result.map_err(|error| io::Error::other(format!("connection operation failed: {error:?}")))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn governed_connections_share_the_runtime_credential_owner_and_cleanup_exactly()
-> Result<(), Box<dyn Error>> {
    tokio_rustls::rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| {
            io::Error::other("the governed Connections E2E crypto provider was already selected")
        })?;
    let mut harness = Harness::from_environment().await?;
    let alice_id = harness
        .register_user("alice-subject", "example.com", "alice@example.com")
        .await?;
    let bob_id = harness
        .register_user("bob-subject", "example.org", "bob@example.org")
        .await?;
    let alice = Harness::session(alice_id.clone(), "alice@example.com");
    let bob = Harness::session(bob_id.clone(), "bob@example.org");
    let broker = harness.broker()?;
    let member_role = "engineer";
    let agent_envelope = harness.seed_long_running_envelope(member_role).await?;

    harness.write_runtime(
        ALICE_NAMESPACE,
        ALICE_RUNTIME,
        &alice_id,
        "alice@example.com",
        member_role,
        &agent_envelope,
    )?;
    harness.write_runtime(
        BOB_NAMESPACE,
        BOB_RUNTIME,
        &bob_id,
        "bob@example.org",
        member_role,
        &agent_envelope,
    )?;
    harness.wait_runtime_phase(
        ALICE_NAMESPACE,
        ALICE_RUNTIME,
        "Running",
        Duration::from_secs(300),
    )?;
    harness.wait_runtime_phase(
        BOB_NAMESPACE,
        BOB_RUNTIME,
        "Running",
        Duration::from_secs(300),
    )?;
    harness.wait_tool_contains(
        ALICE_NAMESPACE,
        ALICE_RUNTIME,
        "not connected",
        Duration::from_secs(30),
    )?;

    let status_count = harness.operation_count(&alice_id, "status").await?;
    let status_broker = broker.clone();
    let status_session = alice.clone();
    let status_task = tokio::spawn(async move { status_broker.status(&status_session).await });
    let restarted_status = harness
        .latest_operation(&alice_id, "status", status_count)
        .await?;
    harness.restart_controller();
    let status = connection_result(status_task.await?)?;
    assert_eq!(status.phase, ConnectionPhase::Disconnected);
    harness
        .wait_operation_finalized(restarted_status, Duration::from_secs(90))
        .await?;

    tokio::time::sleep(Duration::from_secs(6)).await;
    let status_count = harness.operation_count(&alice_id, "status").await?;
    let status_broker = broker.clone();
    let status_session = alice.clone();
    let status_task = tokio::spawn(async move { status_broker.status(&status_session).await });
    let reconciler_restart_status = harness
        .latest_operation(&alice_id, "status", status_count)
        .await?;
    harness.restart_reconciler();
    assert_eq!(
        connection_result(status_task.await?)?.phase,
        ConnectionPhase::Disconnected
    );
    harness
        .wait_operation_finalized(reconciler_restart_status, Duration::from_secs(90))
        .await?;

    tokio::time::sleep(Duration::from_secs(6)).await;
    let status_count = harness.operation_count(&alice_id, "status").await?;
    let cancelled_broker = broker.clone();
    let cancelled_session = alice.clone();
    let cancelled = tokio::spawn(async move { cancelled_broker.status(&cancelled_session).await });
    let cancelled_status = harness
        .latest_operation(&alice_id, "status", status_count)
        .await?;
    cancelled.abort();
    harness
        .wait_operation_finalized(cancelled_status, Duration::from_secs(90))
        .await?;

    let start_count = harness.operation_count(&alice_id, "start").await?;
    let start_broker = broker.clone();
    let start_session = alice.clone();
    let start_task = tokio::spawn(async move { start_broker.start(&start_session).await });
    let start_operation = harness
        .latest_operation(&alice_id, "start", start_count)
        .await?;
    let (bridge_workspace, bridge_sandbox, bridge_uid) =
        harness.capture_bridge_refs(start_operation).await?;
    let started = connection_result(start_task.await?)?;
    harness
        .wait_operation_finalized(start_operation, Duration::from_secs(90))
        .await?;
    harness.assert_bridge_runtime_absent(&bridge_workspace, &bridge_sandbox, &bridge_uid)?;

    let reused = connection_result(broker.start(&alice).await)?;
    assert_eq!(
        reused.authorization_url.as_str(),
        started.authorization_url.as_str(),
        "duplicate starts must reuse one unexpired real MCP-GW flow"
    );
    assert_eq!(
        harness.operation_count(&alice_id, "start").await?,
        start_count + 1
    );

    let state = oauth_state(started.authorization_url.as_str())?;
    let oauth_row = sqlx::query(
        "SELECT hop1_issuer, hop1_subject, email, \
                EXTRACT(EPOCH FROM (expires_at - now()))::float8 AS remaining \
         FROM oauth_states WHERE hop1_subject = $1 ORDER BY expires_at DESC LIMIT 1",
    )
    .bind(alice_id.as_str())
    .fetch_one(&harness.database)
    .await?;
    assert_eq!(
        oauth_row.try_get::<String, _>("hop1_issuer")?,
        MCP_GW_ISSUER
    );
    assert_eq!(
        oauth_row.try_get::<String, _>("hop1_subject")?,
        alice_id.as_str()
    );
    assert_eq!(
        oauth_row.try_get::<String, _>("email")?,
        "alice@example.com"
    );
    let remaining: f64 = oauth_row.try_get("remaining")?;
    assert!(
        (585.0..=600.0).contains(&remaining),
        "real MCP-GW 0.3.2 OAuth state must have its pinned 600-second lifetime"
    );
    let steward_lifetime: f64 = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM (flow_expires_at - flow_created_at))::float8 \
         FROM connection_operations WHERE operation_id = $1",
    )
    .bind(start_operation)
    .fetch_one(&harness.database)
    .await?;
    assert_eq!(steward_lifetime, 630.0);

    assert_eq!(
        broker.disconnect(&alice).await,
        Err(ConnectionBrokerError::OAuthFlowPending),
        "disconnect must perform an uncached governed status and retain a genuinely pending flow"
    );
    let pending_url_present: bool = sqlx::query_scalar(
        "SELECT authorization_url IS NOT NULL FROM connection_operations WHERE operation_id = $1",
    )
    .bind(start_operation)
    .fetch_one(&harness.database)
    .await?;
    assert!(pending_url_present);

    harness.callback(state)?;
    assert_eq!(
        connection_result(broker.status(&alice).await)?.phase,
        ConnectionPhase::Connected
    );
    let owner = sqlx::query(
        "SELECT hop1_issuer, hop1_subject, email FROM oauth_accounts \
         WHERE provider = 'github' AND revoked_at IS NULL",
    )
    .fetch_one(&harness.database)
    .await?;
    assert_eq!(owner.try_get::<String, _>("hop1_issuer")?, MCP_GW_ISSUER);
    assert_eq!(
        owner.try_get::<String, _>("hop1_subject")?,
        alice_id.as_str()
    );
    assert_eq!(owner.try_get::<String, _>("email")?, "alice@example.com");
    let continuation_redacted: bool = sqlx::query_scalar(
        "SELECT authorization_url IS NULL AND oauth_phase = 'completed' \
         FROM connection_operations WHERE operation_id = $1",
    )
    .bind(start_operation)
    .fetch_one(&harness.database)
    .await?;
    assert!(continuation_redacted);

    harness.wait_tool_contains(
        ALICE_NAMESPACE,
        ALICE_RUNTIME,
        "governed fixture file contents",
        Duration::from_secs(30),
    )?;
    harness.wait_tool_contains(
        BOB_NAMESPACE,
        BOB_RUNTIME,
        "not connected",
        Duration::from_secs(30),
    )?;
    harness.wait_runtime_phase(
        ALICE_NAMESPACE,
        ALICE_RUNTIME,
        "Running",
        Duration::from_secs(5),
    )?;
    harness.wait_runtime_phase(
        BOB_NAMESPACE,
        BOB_RUNTIME,
        "Running",
        Duration::from_secs(5),
    )?;

    connection_result(broker.disconnect(&alice).await)?;
    let enforcement = harness.wait_tool_contains(
        ALICE_NAMESPACE,
        ALICE_RUNTIME,
        "not connected",
        Duration::from_secs(10),
    )?;
    assert!(enforcement <= Duration::from_secs(10));
    println!(
        "governed Connections disconnect enforcement bound observed: {} ms",
        enforcement.as_millis()
    );
    harness.write_runtime(
        ALICE_NAMESPACE,
        ALICE_AFTER_DISCONNECT_RUNTIME,
        &alice_id,
        "alice@example.com",
        member_role,
        &agent_envelope,
    )?;
    harness.wait_runtime_phase(
        ALICE_NAMESPACE,
        ALICE_AFTER_DISCONNECT_RUNTIME,
        "Running",
        Duration::from_secs(300),
    )?;
    harness.wait_tool_contains(
        ALICE_NAMESPACE,
        ALICE_AFTER_DISCONNECT_RUNTIME,
        "not connected",
        Duration::from_secs(30),
    )?;

    let bob_status_count = harness.operation_count(&bob_id, "status").await?;
    assert_eq!(
        connection_result(broker.status(&bob).await)?.phase,
        ConnectionPhase::Disconnected
    );
    let bob_status = harness
        .latest_operation(&bob_id, "status", bob_status_count)
        .await?;
    harness
        .wait_operation_finalized(bob_status, Duration::from_secs(90))
        .await?;
    let generic_runs = harness
        .store
        .agent_runs(&AgentRunQuery {
            limit: 100,
            cursor: None,
            phase: None,
            workflow: None,
            owner_user_id: None,
            runtime_uid: None,
            user_envelope_instance_id: None,
            task_uid: None,
        })
        .await?;
    assert!(
        generic_runs
            .records
            .iter()
            .all(|run| run.submitter_service != "steward-connections"),
        "connection operations must remain structurally absent from generic run history"
    );
    let nonfinal_bridge_tasks: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM connection_operations operations \
         JOIN task_submissions tasks ON tasks.task_uid = operations.task_uid \
         WHERE NOT tasks.finalized OR operations.finalization_state <> 'finalized'",
    )
    .fetch_one(&harness.database)
    .await?;
    assert_eq!(nonfinal_bridge_tasks, 0);
    let durable_audit_count: i64 = sqlx::query_scalar("SELECT count(*) FROM connection_operations")
        .fetch_one(&harness.database)
        .await?;
    assert!(durable_audit_count >= 8);

    harness.delete_runtime(ALICE_NAMESPACE, ALICE_AFTER_DISCONNECT_RUNTIME);
    harness.delete_runtime(BOB_NAMESPACE, BOB_RUNTIME);
    harness.delete_runtime(ALICE_NAMESPACE, ALICE_RUNTIME);
    Ok(())
}
