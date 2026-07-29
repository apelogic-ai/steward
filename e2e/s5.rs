use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use steward_admission::{Envelope, EnvelopeSpec};
use steward_store::PgStore;
use steward_types::{Budget, Duration as RuntimeDuration, ModelRef, ToolGrant};

const NAMESPACE: &str = "team-a";
const RUNTIME_NAME: &str = "runtime-revocation";

struct Harness {
    capture_url: String,
    context: String,
    inference_url: String,
    kubeconfig: PathBuf,
    litellm_url: String,
    master_key: String,
    openshell: PathBuf,
    run_dir: PathBuf,
    tool_url: String,
}

impl Harness {
    fn from_environment() -> Result<Self, Box<dyn Error>> {
        let context = env::var("STEWARD_TEST_KUBE_CONTEXT")?;
        if !context.starts_with("kind-steward-") {
            return Err(io::Error::other(format!(
                "refusing non-ephemeral kube context: {context}"
            ))
            .into());
        }
        Ok(Self {
            capture_url: env::var("STEWARD_TEST_CAPTURE_URL")?,
            context,
            inference_url: env::var("STEWARD_TEST_INFERENCE_URL")?,
            kubeconfig: PathBuf::from(env::var("STEWARD_TEST_KUBECONFIG")?),
            litellm_url: env::var("STEWARD_TEST_LITELLM_URL")?,
            master_key: fs::read_to_string(env::var("STEWARD_TEST_LITELLM_MASTER_KEY_FILE")?)?,
            openshell: PathBuf::from(env::var("STEWARD_OPENSHELL_CLI")?),
            run_dir: PathBuf::from(env::var("STEWARD_RUN_DIR")?),
            tool_url: env::var("STEWARD_TEST_TOOL_URL")?,
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

    fn kubectl_as_actor(&self, arguments: &[&str]) -> Result<Output, Box<dyn Error>> {
        Ok(Command::new("kubectl")
            .args(["--kubeconfig"])
            .arg(&self.kubeconfig)
            .args(["--context", &self.context])
            .args([
                "--as",
                "alice@example.com",
                "--as-group",
                "agents.apelogic.ai/member-role:engineer",
            ])
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

    fn write_runtime(&self) -> Result<PathBuf, Box<dyn Error>> {
        let path = self.run_dir.join("e2e-s5-runtime.json");
        let manifest = serde_json::json!({
            "apiVersion": "agents.apelogic.ai/v1alpha1",
            "kind": "AgentRuntime",
            "metadata": {
                "name": RUNTIME_NAME,
                "namespace": NAMESPACE,
                "annotations": {
                    "agents.apelogic.ai/member-role": "engineer"
                }
            },
            "spec": {
                "principal": {
                    "kind": "user",
                    "actingUser": "alice@example.com"
                },
                "owner": "alice@example.com",
                "agentType": {"name": "base"},
                "llms": [{
                    "provider": "openai",
                    "model": "priced-model"
                }],
                "tools": [{
                    "provider": "github",
                    "resource": "search_repositories",
                    "action": "read"
                }],
                "budget": {
                    "monthlyLimit": "10.00",
                    "currency": "USD"
                },
                "ttl": "1h"
            }
        });
        fs::write(&path, serde_json::to_vec_pretty(&manifest)?)?;
        Ok(path)
    }

    fn wait_phase(&self, expected: &str, timeout: Duration) -> Result<(), Box<dyn Error>> {
        let deadline = Instant::now() + timeout;
        let mut last = String::new();
        while Instant::now() < deadline {
            let output = self.kubectl(&[
                "-n",
                NAMESPACE,
                "get",
                "agentruntime",
                RUNTIME_NAME,
                "-o",
                "jsonpath={.status.phase}",
            ])?;
            if output.status.success() {
                last = String::from_utf8(output.stdout)?;
                if last == expected {
                    return Ok(());
                }
            }
            thread::sleep(Duration::from_secs(1));
        }
        Err(io::Error::other(format!(
            "{RUNTIME_NAME} did not reach {expected}; last phase was {last:?}"
        ))
        .into())
    }

    fn runtime_value(&self, expression: &str) -> Result<String, Box<dyn Error>> {
        Ok(self
            .kubectl_ok(&[
                "-n",
                NAMESPACE,
                "get",
                "agentruntime",
                RUNTIME_NAME,
                "-o",
                expression,
            ])?
            .trim()
            .to_owned())
    }

    fn sandbox_request(
        &self,
        workspace: &str,
        sandbox: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Result<String, Box<dyn Error>> {
        let mut command = Command::new(&self.openshell);
        command
            .args(["--gateway-endpoint"])
            .arg(env::var("STEWARD_OPENSHELL_ENDPOINT")?)
            .args([
                "--workspace",
                workspace,
                "sandbox",
                "exec",
                "--name",
                sandbox,
            ])
            .args(["--no-tty", "--", "curl", "-sS", "--max-time", "20"]);
        for (name, value) in headers {
            command.args(["-H", &format!("{name}: {value}")]);
        }
        let output = command.args(["-d", body, url]).output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "sandbox request failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))
            .into());
        }
        Ok(String::from_utf8(output.stdout)?)
    }

    fn replay_status(&self, path: &str) -> Result<u16, Box<dyn Error>> {
        let output = Command::new("curl")
            .args(["-fsS", "-X", "POST"])
            .arg(format!("{}{path}", self.capture_url))
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!("capture replay {path} failed")).into());
        }
        let body: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        body.get("upstreamStatus")
            .and_then(serde_json::Value::as_u64)
            .and_then(|status| u16::try_from(status).ok())
            .ok_or_else(|| io::Error::other(format!("capture replay {path} had no status")).into())
    }

    fn expire_runtime(&self) -> Result<(), Box<dyn Error>> {
        let output = self.kubectl_as_actor(&[
            "-n",
            NAMESPACE,
            "patch",
            "agentruntime",
            RUNTIME_NAME,
            "--type=merge",
            "-p",
            r#"{"spec":{"ttl":"1s"}}"#,
        ])?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "TTL reduction failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))
            .into());
        }
        Ok(())
    }

    fn wait_runtime_absent(&self, timeout: Duration) -> Result<(), Box<dyn Error>> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let output = self.kubectl(&["-n", NAMESPACE, "get", "agentruntime", RUNTIME_NAME])?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                if stderr.contains("NotFound") || stderr.contains("not found") {
                    return Ok(());
                }
            }
            thread::sleep(Duration::from_secs(1));
        }
        Err(io::Error::other("TTL termination did not delete the runtime").into())
    }

    fn key_count(&self, alias: &str) -> Result<u64, Box<dyn Error>> {
        let output = Command::new("curl")
            .arg("-fsS")
            .arg("-H")
            .arg(format!("Authorization: Bearer {}", self.master_key.trim()))
            .arg("--get")
            .arg("--data-urlencode")
            .arg(format!("key_alias={alias}"))
            .arg("--data-urlencode")
            .arg("return_full_object=true")
            .arg(format!("{}/key/list", self.litellm_url))
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other("LiteLLM key lookup failed").into());
        }
        let body: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        body.get("total_count")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| io::Error::other("LiteLLM key count was missing").into())
    }

    fn assert_holds_nothing(
        &self,
        runtime_uid: &str,
        workspace: &str,
        sandbox: &str,
        key_alias: &str,
    ) -> Result<(), Box<dyn Error>> {
        assert_eq!(
            self.replay_status("/replay-token")?,
            403,
            "the mint must refuse the captured sandbox SVID after its runtime is gone"
        );
        assert_eq!(
            self.replay_status("/replay-tool")?,
            401,
            "mcp-gw must reject the previously issued HOP-1 after termination"
        );
        assert_eq!(
            self.replay_status("/replay-inference")?,
            401,
            "LiteLLM must reject the previously issued runtime key after termination"
        );
        assert_eq!(
            self.key_count(key_alias)?,
            0,
            "the runtime key must be absent from LiteLLM's active key set"
        );

        let secret = self.kubectl(&["-n", NAMESPACE, "get", "secret", runtime_uid])?;
        assert!(
            !secret.status.success(),
            "the runtime-scoped credential Secret survived termination"
        );
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
        assert!(
            sandboxes.trim().is_empty(),
            "the terminated runtime retained Agent Sandbox resources: {}",
            sandboxes.trim()
        );
        Ok(())
    }

    fn delete_runtime(&self) {
        let _ = self.kubectl_as_actor(&[
            "-n",
            NAMESPACE,
            "delete",
            "agentruntime",
            RUNTIME_NAME,
            "--ignore-not-found=true",
            "--wait=true",
            "--timeout=120s",
        ]);
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.delete_runtime();
    }
}

#[tokio::test]
async fn e2e_s5_terminated_runtime_holds_nothing() -> Result<(), Box<dyn Error>> {
    let harness = Harness::from_environment()?;
    let store = PgStore::connect(&env::var("STEWARD_TEST_DATABASE_URL")?).await?;
    store.migrate().await?;
    store
        .insert_envelope(
            "engineer",
            &Envelope {
                revision: 1,
                spec: EnvelopeSpec {
                    llms: vec![ModelRef {
                        provider: "openai".to_owned(),
                        model: "priced-model".to_owned(),
                    }],
                    tools: vec![ToolGrant {
                        provider: "github".to_owned(),
                        resource: "search_repositories".to_owned(),
                        action: "read".to_owned(),
                    }],
                    budget: Budget {
                        monthly_limit: "100.00".to_owned(),
                        currency: "USD".to_owned(),
                    },
                    ttl: RuntimeDuration("1h".to_owned()),
                },
            },
            "admin@example.com",
        )
        .await?;

    let manifest = harness.write_runtime()?;
    let applied = harness.kubectl_as_actor(&["apply", "-f", path_text(&manifest)?])?;
    if !applied.status.success() {
        return Err(io::Error::other(format!(
            "S5 runtime admission failed: {}",
            String::from_utf8_lossy(&applied.stderr).trim()
        ))
        .into());
    }
    harness.wait_phase("Running", Duration::from_secs(600))?;

    let runtime_uid = harness.runtime_value("jsonpath={.metadata.uid}")?;
    let workspace = harness.runtime_value("jsonpath={.status.refs.workspace}")?;
    let sandbox = harness.runtime_value("jsonpath={.status.refs.sandbox}")?;
    let key_alias = harness.runtime_value("jsonpath={.status.refs.litellmKey}")?;
    for (label, value) in [
        ("runtime UID", runtime_uid.as_str()),
        ("workspace", workspace.as_str()),
        ("sandbox", sandbox.as_str()),
        ("LiteLLM key alias", key_alias.as_str()),
    ] {
        if value.is_empty() {
            return Err(io::Error::other(format!("S5 runtime has no {label}")).into());
        }
    }

    let tool_request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "search_repositories",
            "arguments": {}
        }
    })
    .to_string();
    let tool_response = harness.sandbox_request(
        &workspace,
        &sandbox,
        &harness.tool_url,
        &[
            ("Content-Type", "application/json"),
            ("MCP-Protocol-Version", "2025-06-18"),
        ],
        &tool_request,
    )?;
    assert!(
        tool_response.contains("example-org/fixture-repository"),
        "the pre-termination HOP-1 did not authorize the expected tool: {tool_response}"
    );

    let inference_response = harness.sandbox_request(
        &workspace,
        &sandbox,
        &harness.inference_url,
        &[("Content-Type", "application/json")],
        r#"{"model":"openai/priced-model","messages":[{"role":"user","content":"hello"}]}"#,
    )?;
    assert!(
        inference_response.contains("allowed fixture response"),
        "the pre-termination runtime key did not authorize inference: {inference_response}"
    );

    harness.expire_runtime()?;
    harness.wait_runtime_absent(Duration::from_secs(300))?;
    harness.assert_holds_nothing(&runtime_uid, &workspace, &sandbox, &key_alias)?;

    thread::sleep(Duration::from_secs(3));
    harness.assert_holds_nothing(&runtime_uid, &workspace, &sandbox, &key_alias)?;
    Ok(())
}

fn path_text(path: &Path) -> Result<&str, Box<dyn Error>> {
    path.to_str()
        .ok_or_else(|| io::Error::other("run path is not valid UTF-8").into())
}
