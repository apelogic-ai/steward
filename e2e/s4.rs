use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use sqlx::Row;
use sqlx::postgres::PgPoolOptions;
use steward_store::{ApproveAdmission, PgStore};

const NAMESPACE: &str = "team-a";
const MEMBER_ROLE: &str = "engineer";
const CEILING: &str = "200.00";
const GRANTED_BUDGET: &str = "220.00";

#[derive(Clone, Copy)]
enum Caller {
    Alice,
    Bob,
    Admin,
}

impl Caller {
    fn bearer_token(self) -> &'static str {
        match self {
            Self::Alice => "test-alice-session",
            Self::Bob => "test-bob-session",
            Self::Admin => "test-admin-session",
        }
    }
}

struct Harness {
    api_url: String,
    ca_certificate: PathBuf,
    context: String,
    database_url: String,
    jira_url: String,
    kubeconfig: PathBuf,
    resolve: String,
    run_directory: PathBuf,
}

impl Harness {
    fn from_environment() -> Result<Self, Box<dyn Error>> {
        let context = env::var("STEWARD_TEST_KUBE_CONTEXT")?;
        if !context.starts_with("kind-steward-s4-") {
            return Err(io::Error::other(format!(
                "refusing non-S4 ephemeral kube context: {context}"
            ))
            .into());
        }
        Ok(Self {
            api_url: env::var("STEWARD_S4_URL")?,
            ca_certificate: PathBuf::from(env::var("STEWARD_TEST_TLS_CA")?),
            context,
            database_url: env::var("STEWARD_TEST_DATABASE_URL")?,
            jira_url: env::var("STEWARD_TEST_JIRA_URL")?,
            kubeconfig: PathBuf::from(env::var("STEWARD_TEST_KUBECONFIG")?),
            resolve: env::var("STEWARD_S4_RESOLVE")?,
            run_directory: PathBuf::from(env::var("STEWARD_RUN_DIR")?),
        })
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

    fn steward(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
        output_name: &str,
        caller: Caller,
    ) -> Result<(u16, String), Box<dyn Error>> {
        let output_path = self.run_directory.join(output_name);
        let mut command = Command::new("curl");
        command
            .args(["--silent", "--show-error", "--cacert"])
            .arg(&self.ca_certificate)
            .args(["--resolve", &self.resolve, "--request", method, "--header"])
            .arg(format!("authorization: Bearer {}", caller.bearer_token()))
            .arg("--output")
            .arg(&output_path)
            .args(["--write-out", "%{http_code}"]);
        if let Some(body) = body {
            command
                .args(["--header", "content-type: application/json", "--data"])
                .arg(body);
        }
        let output = command.arg(format!("{}{}", self.api_url, path)).output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "curl {method} {path} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))
            .into());
        }
        let status = String::from_utf8(output.stdout)?
            .parse::<u16>()
            .map_err(|error| io::Error::other(format!("invalid HTTP status: {error}")))?;
        Ok((status, fs::read_to_string(output_path)?))
    }

    fn jira(
        &self,
        method: &str,
        path: &str,
        output_name: &str,
    ) -> Result<(u16, String), Box<dyn Error>> {
        let output_path = self.run_directory.join(output_name);
        let output = Command::new("curl")
            .args(["--silent", "--show-error", "--request", method, "--output"])
            .arg(&output_path)
            .args(["--write-out", "%{http_code}"])
            .arg(format!("{}{}", self.jira_url, path))
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "mock Jira {method} {path} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))
            .into());
        }
        let status = String::from_utf8(output.stdout)?
            .parse::<u16>()
            .map_err(|error| io::Error::other(format!("invalid Jira HTTP status: {error}")))?;
        Ok((status, fs::read_to_string(output_path)?))
    }

    fn write_runtime(
        &self,
        name: &str,
        acting_user: &str,
        budget: &str,
    ) -> Result<PathBuf, Box<dyn Error>> {
        let path = self.run_directory.join(format!("{name}.json"));
        let manifest = serde_json::json!({
            "apiVersion": "agents.apelogic.ai/v1alpha1",
            "kind": "AgentRuntime",
            "metadata": {
                "name": name,
                "namespace": NAMESPACE,
                "annotations": {
                    "agents.apelogic.ai/member-role": "engineer",
                },
            },
            "spec": {
                "principal": {
                    "kind": "user",
                    "actingUser": acting_user,
                },
                "owner": acting_user,
                "agentType": {"name": "base"},
                "llms": [{"provider": "provider-a", "model": "model-a"}],
                "tools": [],
                "budget": {
                    "monthlyLimit": budget,
                    "currency": "USD",
                },
                "ttl": "24h",
            },
        });
        fs::write(&path, serde_json::to_vec_pretty(&manifest)?)?;
        Ok(path)
    }

    fn apply_as(&self, manifest: &Path, acting_user: &str) -> Result<Output, Box<dyn Error>> {
        self.kubectl(&[
            "--as",
            acting_user,
            "--as-group",
            "agents.apelogic.ai/member-role:engineer",
            "apply",
            "-f",
            path_text(manifest)?,
        ])
    }

    fn runtime_field(&self, name: &str, jsonpath: &str) -> Result<String, Box<dyn Error>> {
        Ok(self
            .kubectl_ok(&["-n", NAMESPACE, "get", "agentruntime", name, "-o", jsonpath])?
            .trim()
            .to_owned())
    }

    fn wait_for_runtime_field(
        &self,
        name: &str,
        jsonpath: &str,
        expected: &str,
    ) -> Result<(), Box<dyn Error>> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if self.runtime_field(name, jsonpath)? == expected {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other(format!(
                    "AgentRuntime {name} field {jsonpath} did not converge to {expected:?}"
                ))
                .into());
            }
            thread::sleep(Duration::from_millis(250));
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _result = self.kubectl(&[
            "delete",
            "namespace",
            NAMESPACE,
            "--ignore-not-found=true",
            "--wait=true",
            "--timeout=60s",
        ]);
    }
}

#[test]
fn e2e_s4_grant_binds_to_instance() -> Result<(), Box<dyn Error>> {
    let harness = Harness::from_environment()?;
    harness.kubectl_ok(&["create", "namespace", NAMESPACE])?;

    let envelope = serde_json::json!({
        "revision": 1,
        "spec": {
            "llms": [{"provider": "provider-a", "model": "model-a"}],
            "tools": [],
            "budget": {"monthlyLimit": CEILING, "currency": "USD"},
            "ttl": "24h",
        },
    });
    let (status, _) = harness.steward(
        "POST",
        &format!("/admin/envelopes/{MEMBER_ROLE}"),
        Some(&envelope.to_string()),
        "s4-envelope.json",
        Caller::Admin,
    )?;
    assert_eq!(status, 201, "member-role envelope must be authored");

    let alice_manifest = harness.write_runtime("runtime-a", "alice@example.com", "100.00")?;
    let alice_apply = harness.apply_as(&alice_manifest, "alice@example.com")?;
    assert!(
        alice_apply.status.success(),
        "Alice's in-envelope runtime must be admitted: {}",
        String::from_utf8_lossy(&alice_apply.stderr)
    );
    let alice_uid = harness.runtime_field("runtime-a", "jsonpath={.metadata.uid}")?;
    assert!(
        !alice_uid.is_empty(),
        "runtime A must have an immutable UID"
    );

    let increase = r#"{"amount":"120.00"}"#;
    let (parked_status, parked_body) = harness.steward(
        "PATCH",
        "/v1/namespaces/team-a/runtimes/runtime-a/budget",
        Some(increase),
        "alice-parked.json",
        Caller::Alice,
    )?;
    assert_eq!(parked_status, 202, "Alice's over-limit request must park");
    let parked = serde_json::from_str::<serde_json::Value>(&parked_body)?;
    let approval_id = required_json_string(&parked, "/approvalId")?;
    let decision_key = required_json_string(&parked, "/decisionKey")?;
    let evidence_url = required_json_string(&parked, "/evidenceUrl")?;
    assert_eq!(
        parked.pointer("/proposedSpec/budget/monthlyLimit"),
        Some(&serde_json::json!(GRANTED_BUDGET))
    );
    assert_eq!(
        harness.runtime_field("runtime-a", "jsonpath={.spec.budget.monthlyLimit}")?,
        "100.00",
        "parking must not mutate desired state"
    );

    let (jira_status, jira_body) = harness.jira("GET", "/test/state", "jira-created.json")?;
    assert_eq!(jira_status, 200);
    let jira = serde_json::from_str::<serde_json::Value>(&jira_body)?;
    assert_eq!(
        jira.pointer("/issues/0/key"),
        Some(&serde_json::json!(decision_key))
    );
    let create_body = jira
        .pointer("/issues/0/createBody")
        .map(serde_json::Value::to_string)
        .ok_or_else(|| io::Error::other("Jira issue must retain its structured create body"))?;
    for expected in [
        approval_id.as_str(),
        alice_uid.as_str(),
        "alice@example.com",
        "engineer",
        "requested 220.00 USD",
        "ceiling 200.00 USD",
    ] {
        assert!(
            create_body.contains(expected),
            "Jira issue must be prepopulated with {expected:?}"
        );
    }

    let (transition_status, _) = harness.jira(
        "POST",
        &format!("/test/issues/{decision_key}/transition"),
        "jira-transition.json",
    )?;
    assert_eq!(transition_status, 204);
    assert_eq!(
        harness.runtime_field("runtime-a", "jsonpath={.spec.budget.monthlyLimit}")?,
        "100.00",
        "a Jira transition must never grant Steward authority"
    );

    let approval = serde_json::json!({
        "rationale": "approved for this runtime instance",
        "evidenceUrl": evidence_url,
        "expiresAt": "2999-01-01T00:00:00Z",
    });
    let (approval_status, approval_body) = harness.steward(
        "POST",
        &format!("/admin/approvals/{approval_id}/approve"),
        Some(&approval.to_string()),
        "alice-approved.json",
        Caller::Admin,
    )?;
    assert_eq!(
        approval_status, 200,
        "only Steward's authenticated admin endpoint may approve: {approval_body}"
    );
    assert_eq!(
        harness.runtime_field("runtime-a", "jsonpath={.spec.budget.monthlyLimit}")?,
        GRANTED_BUDGET,
        "the grant must admit the approved manifest for runtime A"
    );

    let bob_manifest = harness.write_runtime("runtime-b", "bob@example.org", "100.00")?;
    let bob_apply = harness.apply_as(&bob_manifest, "bob@example.org")?;
    assert!(
        bob_apply.status.success(),
        "Bob's in-envelope runtime must be admitted: {}",
        String::from_utf8_lossy(&bob_apply.stderr)
    );
    let bob_uid = harness.runtime_field("runtime-b", "jsonpath={.metadata.uid}")?;
    assert_ne!(
        alice_uid, bob_uid,
        "the test requires distinct runtime UIDs"
    );

    let (bob_status, bob_body) = harness.steward(
        "PATCH",
        "/v1/namespaces/team-a/runtimes/runtime-b/budget",
        Some(increase),
        "bob-parked.json",
        Caller::Bob,
    )?;
    assert_eq!(
        bob_status, 202,
        "the same role and request on runtime B must escalate again"
    );
    let bob_parked = serde_json::from_str::<serde_json::Value>(&bob_body)?;
    assert_ne!(
        bob_parked.pointer("/decisionKey"),
        parked.pointer("/decisionKey"),
        "runtime B must receive a distinct approval request"
    );
    assert_eq!(
        harness.runtime_field("runtime-b", "jsonpath={.spec.budget.monthlyLimit}")?,
        "100.00",
        "runtime A's grant must not apply to runtime B"
    );
    let bob_over_limit = harness.write_runtime("runtime-b", "bob@example.org", GRANTED_BUDGET)?;
    let bob_direct_apply = harness.apply_as(&bob_over_limit, "bob@example.org")?;
    assert!(
        !bob_direct_apply.status.success(),
        "runtime B must be denied when it tries to reuse runtime A's grant"
    );

    let database_runtime = tokio::runtime::Runtime::new()?;
    let (alice_grants, bob_grants, ceiling) = database_runtime.block_on(async {
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&harness.database_url)
            .await?;
        let alice_grants =
            sqlx::query("SELECT count(*)::bigint AS count FROM grants WHERE runtime_uid = $1")
                .bind(&alice_uid)
                .fetch_one(&pool)
                .await?
                .try_get::<i64, _>("count")?;
        let bob_grants =
            sqlx::query("SELECT count(*)::bigint AS count FROM grants WHERE runtime_uid = $1")
                .bind(&bob_uid)
                .fetch_one(&pool)
                .await?
                .try_get::<i64, _>("count")?;
        let ceiling = sqlx::query(
            "SELECT spec->'budget'->>'monthlyLimit' AS ceiling \
             FROM envelopes WHERE scope_kind = 'member_role' AND scope_ref = $1",
        )
        .bind(MEMBER_ROLE)
        .fetch_one(&pool)
        .await?
        .try_get::<String, _>("ceiling")?;
        Ok::<_, sqlx::Error>((alice_grants, bob_grants, ceiling))
    })?;
    assert_eq!(
        alice_grants, 1,
        "runtime A must own exactly one budget grant"
    );
    assert_eq!(bob_grants, 0, "runtime B must own no grant");
    assert_eq!(ceiling, CEILING, "approval must never edit the envelope");

    let (revocation_status, revocation_body) = harness.steward(
        "POST",
        &format!("/admin/runtimes/{alice_uid}/grants/revoke"),
        Some(r#"{"reason":"approved exception window ended"}"#),
        "alice-revoked.json",
        Caller::Admin,
    )?;
    assert_eq!(
        revocation_status, 204,
        "revoking runtime A's grant must succeed: {revocation_body}"
    );
    assert_eq!(
        harness.runtime_field("runtime-a", "jsonpath={.spec.budget.monthlyLimit}")?,
        "100.00",
        "revocation must remove authority already applied to the live runtime"
    );
    let revocations = database_runtime.block_on(async {
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&harness.database_url)
            .await?;
        sqlx::query(
            "SELECT count(*)::bigint AS count \
             FROM grant_revocations \
             JOIN grants ON grants.id = grant_revocations.grant_id \
             WHERE grants.runtime_uid = $1",
        )
        .bind(&alice_uid)
        .fetch_one(&pool)
        .await?
        .try_get::<i64, _>("count")
    })?;
    assert_eq!(
        revocations, 1,
        "revocation must retain one immutable authority-removal event"
    );

    let (_, final_jira_body) = harness.jira("GET", "/test/state", "jira-final.json")?;
    let final_jira = serde_json::from_str::<serde_json::Value>(&final_jira_body)?;
    let first_comments = final_jira
        .pointer("/issues/0/comments")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| io::Error::other("first Jira issue must expose comments"))?;
    assert_eq!(
        first_comments.len(),
        1,
        "Steward approval must write one resolution comment"
    );
    let comment = first_comments[0].to_string();
    for expected in [
        approval_id.as_str(),
        "admin@example.com",
        "approved for this runtime instance",
        evidence_url.as_str(),
    ] {
        assert!(
            comment.contains(expected),
            "resolution comment must contain {expected:?}"
        );
    }

    let initial_create = serde_json::json!({
        "name": "runtime-c",
        "spec": {
            "principal": {
                "kind": "user",
                "actingUser": "alice@example.com",
            },
            "owner": "alice@example.com",
            "agentType": {"name": "base"},
            "llms": [{"provider": "provider-a", "model": "model-a"}],
            "tools": [],
            "budget": {
                "monthlyLimit": GRANTED_BUDGET,
                "currency": "USD",
            },
            "ttl": "24h",
        },
    });
    let (create_status, create_body) = harness.steward(
        "POST",
        "/v1/namespaces/team-a/runtimes",
        Some(&initial_create.to_string()),
        "initial-create-parked.json",
        Caller::Alice,
    )?;
    assert_eq!(
        create_status, 202,
        "an over-envelope initial create must park: {create_body}"
    );
    let create_parked = serde_json::from_str::<serde_json::Value>(&create_body)?;
    let create_approval_id = required_json_string(&create_parked, "/approvalId")?;
    let create_evidence_url = required_json_string(&create_parked, "/evidenceUrl")?;
    let runtime_c_uid = harness.runtime_field("runtime-c", "jsonpath={.metadata.uid}")?;
    let marker_path = "jsonpath={.metadata.annotations.agents\\.apelogic\\.ai/pending-approval}";
    let pending_marker = harness.runtime_field("runtime-c", marker_path)?;
    assert!(
        !pending_marker.is_empty(),
        "parked create must retain its marker"
    );
    harness.wait_for_runtime_field("runtime-c", "jsonpath={.status.phase}", "Pending")?;
    assert_eq!(
        harness.runtime_field("runtime-c", "jsonpath={.status.refs.workspace}")?,
        "",
        "held placeholder must not receive a sandbox reference"
    );
    assert_eq!(
        harness.runtime_field("runtime-c", "jsonpath={.status.refs.litellmKey}")?,
        "",
        "held placeholder must not receive inference authority"
    );
    let held_secret = harness.kubectl(&["-n", NAMESPACE, "get", "secret", &runtime_c_uid])?;
    assert!(
        !held_secret.status.success(),
        "held placeholder must not receive an inference credential Secret"
    );

    let held_patch = harness.kubectl(&[
        "--as",
        "alice@example.com",
        "--as-group",
        "agents.apelogic.ai/member-role:engineer",
        "-n",
        NAMESPACE,
        "patch",
        "agentruntime",
        "runtime-c",
        "--type=merge",
        "-p",
        r#"{"spec":{"budget":{"monthlyLimit":"1.00"}}}"#,
    ])?;
    assert!(
        !held_patch.status.success(),
        "ordinary users must not mutate a held placeholder spec"
    );
    assert!(
        String::from_utf8_lossy(&held_patch.stderr)
            .contains("pending AgentRuntime spec may be changed only by a trusted Steward writer"),
        "held spec mutation must be rejected by Steward admission: {}",
        String::from_utf8_lossy(&held_patch.stderr)
    );
    let (held_retry_status, held_retry_body) = harness.steward(
        "POST",
        "/v1/namespaces/team-a/runtimes",
        Some(&initial_create.to_string()),
        "initial-create-held-retry.json",
        Caller::Alice,
    )?;
    assert_eq!(
        held_retry_status, 202,
        "held retry must remain recognizable"
    );
    let held_retry = serde_json::from_str::<serde_json::Value>(&held_retry_body)?;
    assert_eq!(
        required_json_string(&held_retry, "/approvalId")?,
        create_approval_id,
        "a rejected held-spec PATCH must preserve approval idempotency"
    );

    let user_delete = harness.kubectl(&[
        "--as",
        "alice@example.com",
        "--as-group",
        "agents.apelogic.ai/member-role:engineer",
        "-n",
        NAMESPACE,
        "delete",
        "agentruntime",
        "runtime-c",
        "--wait=false",
    ])?;
    assert!(
        !user_delete.status.success(),
        "ordinary user deletion must not remove the governance anchor"
    );
    assert!(
        String::from_utf8_lossy(&user_delete.stderr)
            .contains("pending AgentRuntime deletion requires a trusted Steward writer"),
        "pending deletion must be rejected by Steward admission: {}",
        String::from_utf8_lossy(&user_delete.stderr)
    );

    database_runtime.block_on(async {
        let store = PgStore::connect(&harness.database_url).await?;
        store.migrate().await?;
        store
            .approve_admission(ApproveAdmission {
                approval_id: create_approval_id.parse()?,
                decided_by: "admin@example.com",
                rationale: "approved initial runtime in the authority ledger",
                evidence_url: &create_evidence_url,
                expires_at: "2999-01-01T00:00:00Z",
            })
            .await?;
        Ok::<(), Box<dyn Error>>(())
    })?;
    harness.wait_for_runtime_field(
        "runtime-c",
        "jsonpath={.spec.budget.monthlyLimit}",
        GRANTED_BUDGET,
    )?;
    harness.wait_for_runtime_field("runtime-c", marker_path, "")?;

    let (create_revocation_status, create_revocation_body) = harness.steward(
        "POST",
        &format!("/admin/runtimes/{runtime_c_uid}/grants/revoke"),
        Some(r#"{"reason":"initial-create authority ended"}"#),
        "initial-create-revoked.json",
        Caller::Admin,
    )?;
    assert_eq!(
        create_revocation_status, 204,
        "initial-create revocation must restore its hold: {create_revocation_body}"
    );
    harness.wait_for_runtime_field("runtime-c", marker_path, &pending_marker)?;
    harness.wait_for_runtime_field("runtime-c", "jsonpath={.spec.budget.monthlyLimit}", "0")?;
    harness.wait_for_runtime_field("runtime-c", "jsonpath={.status.phase}", "Pending")?;
    assert_eq!(
        harness.runtime_field("runtime-c", "jsonpath={.status.refs.workspace}")?,
        "",
        "restored hold must remain free of sandbox authority"
    );
    let reverted_secret = harness.kubectl(&["-n", NAMESPACE, "get", "secret", &runtime_c_uid])?;
    assert!(
        !reverted_secret.status.success(),
        "restored hold must remain free of inference credentials"
    );

    let trusted_cleanup = harness.kubectl(&[
        "--as",
        "system:serviceaccount:steward-system:steward-s3",
        "-n",
        NAMESPACE,
        "delete",
        "agentruntime",
        "runtime-c",
        "--wait=true",
        "--timeout=30s",
    ])?;
    assert!(
        trusted_cleanup.status.success(),
        "configured Steward writer must retain controlled cleanup authority: {}",
        String::from_utf8_lossy(&trusted_cleanup.stderr)
    );

    let delayed_create = serde_json::json!({
        "name": "runtime-e",
        "spec": {
            "principal": {
                "kind": "user",
                "actingUser": "alice@example.com",
            },
            "owner": "alice@example.com",
            "agentType": {"name": "base"},
            "llms": [],
            "tools": [],
            "budget": {
                "monthlyLimit": GRANTED_BUDGET,
                "currency": "USD",
            },
            "ttl": "10s",
        },
    });
    let (delayed_status, delayed_body) = harness.steward(
        "POST",
        "/v1/namespaces/team-a/runtimes",
        Some(&delayed_create.to_string()),
        "delayed-create-parked.json",
        Caller::Alice,
    )?;
    assert_eq!(
        delayed_status, 202,
        "short-TTL initial request must park: {delayed_body}"
    );
    let delayed_parked = serde_json::from_str::<serde_json::Value>(&delayed_body)?;
    let delayed_approval_id = required_json_string(&delayed_parked, "/approvalId")?;
    let delayed_evidence_url = required_json_string(&delayed_parked, "/evidenceUrl")?;
    let runtime_e_uid = harness.runtime_field("runtime-e", "jsonpath={.metadata.uid}")?;
    let delayed_marker = harness.runtime_field("runtime-e", marker_path)?;
    harness.wait_for_runtime_field("runtime-e", "jsonpath={.status.phase}", "Pending")?;

    thread::sleep(Duration::from_secs(11));
    database_runtime.block_on(async {
        let store = PgStore::connect(&harness.database_url).await?;
        store.migrate().await?;
        store
            .approve_admission(ApproveAdmission {
                approval_id: delayed_approval_id.parse()?,
                decided_by: "admin@example.com",
                rationale: "approved after the placeholder deadline",
                evidence_url: &delayed_evidence_url,
                expires_at: "2999-01-01T00:00:00Z",
            })
            .await?;
        Ok::<(), Box<dyn Error>>(())
    })?;
    harness.wait_for_runtime_field("runtime-e", marker_path, "")?;
    harness.wait_for_runtime_field("runtime-e", "jsonpath={.status.phase}", "Running")?;
    assert!(
        !harness
            .runtime_field("runtime-e", "jsonpath={.status.refs.workspace}")?
            .is_empty(),
        "approval after placeholder-age TTL must provision from release time"
    );
    let (delayed_revocation_status, delayed_revocation_body) = harness.steward(
        "POST",
        &format!("/admin/runtimes/{runtime_e_uid}/grants/revoke"),
        Some(r#"{"reason":"short-TTL authority ended"}"#),
        "delayed-create-revoked.json",
        Caller::Admin,
    )?;
    assert_eq!(
        delayed_revocation_status, 204,
        "short-TTL grant revocation must restore its hold: {delayed_revocation_body}"
    );
    harness.wait_for_runtime_field("runtime-e", marker_path, &delayed_marker)?;
    harness.wait_for_runtime_field("runtime-e", "jsonpath={.status.phase}", "Pending")?;
    let delayed_cleanup = harness.kubectl(&[
        "--as",
        "system:serviceaccount:steward-system:steward-s3",
        "-n",
        NAMESPACE,
        "delete",
        "agentruntime",
        "runtime-e",
        "--wait=true",
        "--timeout=30s",
    ])?;
    assert!(
        delayed_cleanup.status.success(),
        "trusted writer must clean up the restored short-TTL hold: {}",
        String::from_utf8_lossy(&delayed_cleanup.stderr)
    );

    let expanding_create = serde_json::json!({
        "name": "runtime-d",
        "spec": {
            "principal": {
                "kind": "user",
                "actingUser": "alice@example.com",
            },
            "owner": "alice@example.com",
            "agentType": {"name": "base"},
            "llms": [],
            "tools": [],
            "budget": {
                "monthlyLimit": GRANTED_BUDGET,
                "currency": "USD",
            },
            "ttl": "24h",
        },
    });
    let (expanding_park_status, expanding_park_body) = harness.steward(
        "POST",
        "/v1/namespaces/team-a/runtimes",
        Some(&expanding_create.to_string()),
        "envelope-expansion-parked.json",
        Caller::Alice,
    )?;
    assert_eq!(
        expanding_park_status, 202,
        "initial request must park before envelope expansion: {expanding_park_body}"
    );
    let expanded_envelope = serde_json::json!({
        "revision": 2,
        "spec": {
            "llms": [{"provider": "provider-a", "model": "model-a"}],
            "tools": [],
            "budget": {"monthlyLimit": "300.00", "currency": "USD"},
            "ttl": "24h",
        },
    });
    let (expanded_envelope_status, expanded_envelope_body) = harness.steward(
        "POST",
        &format!("/admin/envelopes/{MEMBER_ROLE}"),
        Some(&expanded_envelope.to_string()),
        "s4-envelope-expanded.json",
        Caller::Admin,
    )?;
    assert_eq!(
        expanded_envelope_status, 201,
        "expanded envelope must be authored: {expanded_envelope_body}"
    );
    let (expanded_retry_status, expanded_retry_body) = harness.steward(
        "POST",
        "/v1/namespaces/team-a/runtimes",
        Some(&expanding_create.to_string()),
        "envelope-expansion-retry.json",
        Caller::Alice,
    )?;
    assert_eq!(
        expanded_retry_status, 201,
        "matching hold must release after envelope expansion: {expanded_retry_body}"
    );
    assert_eq!(
        harness.runtime_field("runtime-d", "jsonpath={.spec.budget.monthlyLimit}")?,
        GRANTED_BUDGET
    );
    assert_eq!(
        harness.runtime_field("runtime-d", marker_path)?,
        "",
        "envelope release must remove the hold"
    );
    Ok(())
}

fn required_json_string(
    value: &serde_json::Value,
    pointer: &str,
) -> Result<String, Box<dyn Error>> {
    value
        .pointer(pointer)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| io::Error::other(format!("response is missing {pointer}")).into())
}

fn path_text(path: &Path) -> Result<&str, Box<dyn Error>> {
    path.to_str()
        .ok_or_else(|| io::Error::other("run path is not valid UTF-8").into())
}
