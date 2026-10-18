use std::env;
use std::error::Error;
use std::fs::{self, File};
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration as StdDuration, Instant};

use ring::hmac;
use serde_json::json;
use sha2::{Digest, Sha256};
use steward_admission::{Envelope, EnvelopeSpec};
use steward_store::{
    EnvelopeRequestReservationRequest, EnvelopeRequestStatus, EnvelopeRequestStatusUpdate,
    EnvelopeTemplatePublication, PgStore,
};
use steward_types::{
    Budget, Duration, GOOGLE_ORGANIZATION_ISSUER, ModelRef, OrganizationId,
    OrganizationIdentityPolicy, RunnerRequirements, ToolGrant,
};
use uuid::Uuid;

const SIGNING_SECRET: &[u8] = b"obviously-fake-jira-signing-secret";

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
    let (owner, digest) = seed_read_envelope(&store).await?;
    let config_file = run_dir.join("config").with_extension("json");
    let catalog_file = run_dir.join("capabilities").with_extension("json");
    let signing_file = run_dir.join("signing-secret");
    let kubeconfig_file = run_dir.join("kubeconfig");
    fs::write(&signing_file, SIGNING_SECRET)?;
    fs::write(&kubeconfig_file, synthetic_kubeconfig())?;
    fs::write(
        &config_file,
        serde_json::to_vec(&json!({
            "triggers": [{
                "id": "jira-read",
                "enabled": true,
                "source": "jira",
                "workflow": "triage@1",
                "service": "event-worker",
                "ownerUserId": owner,
                "envelopeDigest": format!("steward:{digest}"),
                "signingSecretFile": signing_file,
                "concurrencyLimit": 1,
                "cooldownSeconds": 0
            }]
        }))?,
    )?;
    write_catalog(&catalog_file, "read")?;

    let address = available_address()?;
    let running_log = run_dir.join("running").with_extension("log");
    let mut receiver = spawn_receiver(
        address,
        &database_url,
        &config_file,
        &catalog_file,
        &kubeconfig_file,
        &ca_file,
        &running_log,
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
        0,
        "the refused delivery created a Task"
    );
    receiver.stop()?;

    let startup_log = run_dir.join("startup-refusal").with_extension("log");
    let mut refused = spawn_receiver(
        available_address()?,
        &database_url,
        &config_file,
        &catalog_file,
        &kubeconfig_file,
        &ca_file,
        &startup_log,
    )?;
    let status = wait_for_exit(&mut refused.child, StdDuration::from_secs(10))?;
    assert!(!status.success(), "a receiver started with a write catalog");
    assert!(
        fs::read_to_string(&startup_log)?.contains("a Trigger Envelope is unavailable or unsafe"),
        "startup refusal did not report the Envelope policy failure"
    );
    assert_eq!(
        task_count(&store).await?,
        0,
        "startup refusal created a Task"
    );
    Ok(())
}

async fn seed_read_envelope(store: &PgStore) -> Result<(String, String), Box<dyn Error>> {
    let policy = OrganizationIdentityPolicy::new(
        GOOGLE_ORGANIZATION_ISSUER,
        "example.com",
        OrganizationId::parse("org_example")?,
    )?;
    let owner = store
        .register_canonical_identity(
            &policy.validate(
                GOOGLE_ORGANIZATION_ISSUER,
                "jira-receiver-e2e",
                "example.com",
                "alice@example.com",
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
    let roles = vec!["engineer".to_owned()];
    store
        .insert_envelope_template_revision(EnvelopeTemplatePublication {
            template_id: "jira-read",
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
            template_id: Some("jira-read"),
            template_revision: Some(envelope.revision),
            requested_envelope: &envelope,
            idempotency_key: "jira-read-envelope",
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
    Ok((owner.user_id.as_str().to_owned(), digest))
}

fn spawn_receiver(
    address: SocketAddr,
    database_url: &str,
    config_file: &Path,
    catalog_file: &Path,
    kubeconfig_file: &Path,
    ca_file: &Path,
    log_file: &Path,
) -> Result<ReceiverProcess, Box<dyn Error>> {
    let output = File::create(log_file)?;
    let child = Command::new(env!("CARGO_BIN_EXE_steward-trigger-receiver"))
        .env_clear()
        .env("KUBECONFIG", kubeconfig_file)
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
            "https://exchange.example.test:8443/v1/workload/exchange",
        )
        .env(
            "STEWARD_WORKLOAD_EXCHANGE_SERVER_NAME",
            "exchange.example.test",
        )
        .env("STEWARD_WORKLOAD_EXCHANGE_CA_CERTIFICATE_FILE", ca_file)
        .env("STEWARD_API_ORIGIN", "https://steward.example.test:8443")
        .env("STEWARD_API_SERVER_NAME", "steward.example.test")
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

fn synthetic_kubeconfig() -> &'static str {
    "apiVersion: v1\nkind: Config\nclusters:\n  - name: synthetic\n    cluster:\n      server: https://127.0.0.1:1\n      insecure-skip-tls-verify: true\ncontexts:\n  - name: synthetic\n    context:\n      cluster: synthetic\n      user: synthetic\ncurrent-context: synthetic\nusers:\n  - name: synthetic\n    user:\n      token: synthetic-token\n"
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
