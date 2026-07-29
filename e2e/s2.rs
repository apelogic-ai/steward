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
use steward_types::{Budget, Duration as RuntimeDuration, ModelRef};

const NAMESPACE: &str = "team-a";
const PRICED_RUNTIME: &str = "runtime-priced";
const UNPRICED_RUNTIME: &str = "runtime-unpriced";

struct Harness {
    context: String,
    inference_url: String,
    kubeconfig: PathBuf,
    litellm_url: String,
    master_key: String,
    openshell: PathBuf,
    run_dir: PathBuf,
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
            context,
            inference_url: env::var("STEWARD_TEST_INFERENCE_URL")?,
            kubeconfig: PathBuf::from(env::var("STEWARD_TEST_KUBECONFIG")?),
            litellm_url: env::var("STEWARD_TEST_LITELLM_URL")?,
            master_key: fs::read_to_string(env::var("STEWARD_TEST_LITELLM_MASTER_KEY_FILE")?)?,
            openshell: PathBuf::from(env::var("STEWARD_OPENSHELL_CLI")?),
            run_dir: PathBuf::from(env::var("STEWARD_RUN_DIR")?),
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

    fn write_runtime(&self, name: &str, model: &str) -> Result<PathBuf, Box<dyn Error>> {
        let path = self.run_dir.join(format!("e2e-s2-{name}.json"));
        let manifest = serde_json::json!({
            "apiVersion": "agents.apelogic.ai/v1alpha1",
            "kind": "AgentRuntime",
            "metadata": {
                "name": name,
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
                    "model": model
                }],
                "tools": [],
                "budget": {
                    "monthlyLimit": "1.00",
                    "currency": "USD"
                },
                "ttl": "1h"
            }
        });
        fs::write(&path, serde_json::to_vec_pretty(&manifest)?)?;
        Ok(path)
    }

    fn apply_runtime(&self, path: &Path) -> Result<Output, Box<dyn Error>> {
        self.kubectl_as_actor(&["apply", "-f", path_text(path)?])
    }

    fn wait_phase(
        &self,
        name: &str,
        expected: &str,
        timeout: Duration,
    ) -> Result<(), Box<dyn Error>> {
        let deadline = Instant::now() + timeout;
        let mut last = String::new();
        while Instant::now() < deadline {
            let output = self.kubectl(&[
                "-n",
                NAMESPACE,
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
                        "{name} reached Failed while waiting for {expected}"
                    ))
                    .into());
                }
            }
            thread::sleep(Duration::from_secs(1));
        }
        Err(io::Error::other(format!(
            "{name} did not reach {expected}; last phase was {last:?}"
        ))
        .into())
    }

    fn runtime_ref(&self, field: &str) -> Result<String, Box<dyn Error>> {
        let output = self.kubectl(&[
            "-n",
            NAMESPACE,
            "get",
            "agentruntime",
            PRICED_RUNTIME,
            "-o",
            &format!("jsonpath={{.status.refs.{field}}}"),
        ])?;
        if !output.status.success() {
            return Err(io::Error::other("runtime reference lookup failed").into());
        }
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    }

    fn call_inference(&self) -> Result<String, Box<dyn Error>> {
        let workspace = self.runtime_ref("workspace")?;
        let sandbox = self.runtime_ref("sandbox")?;
        let output = Command::new(&self.openshell)
            .args(["--gateway-endpoint"])
            .arg(env::var("STEWARD_OPENSHELL_ENDPOINT")?)
            .args(["--workspace", &workspace, "sandbox", "exec", "--name", &sandbox])
            .args([
                "--no-tty",
                "--",
                "curl",
                "-sS",
                "--max-time",
                "20",
                "-H",
                "Content-Type: application/json",
                "-d",
                "{\"model\":\"openai/priced-model\",\"messages\":[{\"role\":\"user\",\"content\":\"hello\"}]}",
                &self.inference_url,
            ])
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "sandbox inference call failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))
            .into());
        }
        Ok(String::from_utf8(output.stdout)?)
    }

    fn assert_key_absent(&self, alias: &str) -> Result<(), Box<dyn Error>> {
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
        assert_eq!(
            body.get("total_count").and_then(serde_json::Value::as_u64),
            Some(0),
            "a suspended runtime must have no live inference key"
        );
        Ok(())
    }

    fn delete_runtime(&self, name: &str) {
        let _ = self.kubectl_as_actor(&[
            "-n",
            NAMESPACE,
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
        self.delete_runtime(PRICED_RUNTIME);
        self.delete_runtime(UNPRICED_RUNTIME);
    }
}

#[tokio::test]
async fn e2e_s2_budget_exhaustion_suspends() -> Result<(), Box<dyn Error>> {
    let harness = Harness::from_environment()?;
    let store = PgStore::connect(&env::var("STEWARD_TEST_DATABASE_URL")?).await?;
    store.migrate().await?;
    store
        .insert_envelope(
            "engineer",
            &Envelope {
                revision: 1,
                spec: EnvelopeSpec {
                    llms: vec![
                        ModelRef {
                            provider: "openai".to_owned(),
                            model: "priced-model".to_owned(),
                        },
                        ModelRef {
                            provider: "openai".to_owned(),
                            model: "unpriced-model".to_owned(),
                        },
                    ],
                    tools: Vec::new(),
                    budget: Budget {
                        monthly_limit: "1.00".to_owned(),
                        currency: "USD".to_owned(),
                    },
                    ttl: RuntimeDuration("1h".to_owned()),
                },
            },
            "admin@example.com",
        )
        .await?;

    let unpriced = harness.write_runtime(UNPRICED_RUNTIME, "unpriced-model")?;
    let rejected = harness.apply_runtime(&unpriced)?;
    assert!(
        !rejected.status.success(),
        "a model with no registered positive cost must be rejected at admission"
    );
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("priced inference catalog"),
        "unpriced-model rejection must identify the priced-catalog control"
    );

    let priced = harness.write_runtime(PRICED_RUNTIME, "priced-model")?;
    let applied = harness.apply_runtime(&priced)?;
    if !applied.status.success() {
        return Err(io::Error::other(format!(
            "priced runtime admission failed: {}",
            String::from_utf8_lossy(&applied.stderr).trim()
        ))
        .into());
    }
    harness.wait_phase(PRICED_RUNTIME, "Running", Duration::from_secs(180))?;
    let alias = harness.runtime_ref("litellmKey")?;
    let response = harness.call_inference()?;
    assert!(
        response.contains("allowed fixture response"),
        "the runtime must execute inference through its token-grant credential"
    );
    harness.wait_phase(PRICED_RUNTIME, "Suspended", Duration::from_secs(120))?;
    harness.assert_key_absent(&alias)?;
    Ok(())
}

fn path_text(path: &Path) -> Result<&str, io::Error> {
    path.to_str()
        .ok_or_else(|| io::Error::other("test path is not UTF-8"))
}
