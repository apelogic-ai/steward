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
use steward_types::{Budget, Duration as RuntimeDuration, ModelRef, RunnerRequirements};

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

    fn write_runtime(
        &self,
        name: &str,
        model: &str,
        monthly_limit: &str,
        ttl: &str,
    ) -> Result<PathBuf, Box<dyn Error>> {
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
                    "monthlyLimit": monthly_limit,
                    "currency": "USD"
                },
                "ttl": ttl
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
        let body = self.key_list(alias)?;
        assert_eq!(
            body.get("total_count").and_then(serde_json::Value::as_u64),
            Some(0),
            "a suspended runtime must have no live inference key"
        );
        Ok(())
    }

    fn key_list(&self, alias: &str) -> Result<serde_json::Value, Box<dyn Error>> {
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
        Ok(serde_json::from_slice(&output.stdout)?)
    }

    fn wait_key_budget(
        &self,
        alias: &str,
        expected: f64,
        timeout: Duration,
    ) -> Result<(), Box<dyn Error>> {
        let deadline = Instant::now() + timeout;
        let mut last = None;
        while Instant::now() < deadline {
            let body = self.key_list(alias)?;
            last = body.pointer("/keys/0/max_budget").and_then(number_value);
            if last == Some(expected) {
                return Ok(());
            }
            thread::sleep(Duration::from_secs(1));
        }
        Err(io::Error::other(format!(
            "inference key budget did not reconcile to {expected}; last value was {last:?}"
        ))
        .into())
    }

    fn key_spend(&self, alias: &str) -> Result<f64, Box<dyn Error>> {
        self.key_list(alias)?
            .pointer("/keys/0/spend")
            .and_then(number_value)
            .ok_or_else(|| io::Error::other("LiteLLM key spend was not numeric").into())
    }

    fn wait_key_spend(&self, alias: &str, timeout: Duration) -> Result<f64, Box<dyn Error>> {
        let deadline = Instant::now() + timeout;
        let mut last = 0.0;
        while Instant::now() < deadline {
            last = self.key_spend(alias)?;
            if last > 0.0 {
                return Ok(last);
            }
            thread::sleep(Duration::from_secs(1));
        }
        Err(io::Error::other(format!(
            "inference spend was not observed before the deadline; last value was {last}"
        ))
        .into())
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
                        monthly_limit: "100.00".to_owned(),
                        currency: "USD".to_owned(),
                    },
                    ttl: RuntimeDuration("1h".to_owned()),
                    runner: RunnerRequirements::default(),
                },
            },
            "admin@example.com",
        )
        .await?;

    let unpriced = harness.write_runtime(UNPRICED_RUNTIME, "unpriced-model", "1.00", "1h")?;
    let rejected = harness.apply_runtime(&unpriced)?;
    assert!(
        !rejected.status.success(),
        "a model with no registered positive cost must be rejected at admission"
    );
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("priced inference catalog"),
        "unpriced-model rejection must identify the priced-catalog control"
    );

    let priced = harness.write_runtime(PRICED_RUNTIME, "priced-model", "1.00", "1h")?;
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

    let increased = harness.write_runtime(PRICED_RUNTIME, "priced-model", "100.00", "1h")?;
    let applied = harness.apply_runtime(&increased)?;
    if !applied.status.success() {
        return Err(io::Error::other(format!(
            "budget increase admission failed: {}",
            String::from_utf8_lossy(&applied.stderr).trim()
        ))
        .into());
    }
    harness.wait_key_budget(&alias, 100.0, Duration::from_secs(120))?;
    let response = harness.call_inference()?;
    assert!(
        response.contains("allowed fixture response"),
        "the runtime must execute inference through its token-grant credential"
    );
    let spend = harness.wait_key_spend(&alias, Duration::from_secs(60))?;
    assert!(
        spend > 0.0 && spend < 100.0,
        "the fixture call must accumulate spend without exhausting the increased budget; observed {spend}"
    );

    let lowered = harness.write_runtime(PRICED_RUNTIME, "priced-model", "0.01", "1h")?;
    let applied = harness.apply_runtime(&lowered)?;
    if !applied.status.success() {
        return Err(io::Error::other(format!(
            "budget tightening admission failed: {}",
            String::from_utf8_lossy(&applied.stderr).trim()
        ))
        .into());
    }
    harness.wait_phase(PRICED_RUNTIME, "Suspended", Duration::from_secs(120))?;
    harness.assert_key_absent(&alias)?;

    let unrelated_edit = harness.write_runtime(PRICED_RUNTIME, "priced-model", "0.01", "30m")?;
    let applied = harness.apply_runtime(&unrelated_edit)?;
    if !applied.status.success() {
        return Err(io::Error::other(format!(
            "unrelated spec edit admission failed: {}",
            String::from_utf8_lossy(&applied.stderr).trim()
        ))
        .into());
    }
    harness.wait_phase(PRICED_RUNTIME, "Suspended", Duration::from_secs(120))?;
    harness.assert_key_absent(&alias)?;

    let renewed = harness.write_runtime(PRICED_RUNTIME, "priced-model", "100.00", "30m")?;
    let applied = harness.apply_runtime(&renewed)?;
    if !applied.status.success() {
        return Err(io::Error::other(format!(
            "post-exhaustion budget update admission failed: {}",
            String::from_utf8_lossy(&applied.stderr).trim()
        ))
        .into());
    }
    harness.wait_phase(PRICED_RUNTIME, "Running", Duration::from_secs(180))?;

    store
        .insert_envelope(
            "engineer",
            &Envelope {
                revision: 2,
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
                        monthly_limit: "50.00".to_owned(),
                        currency: "USD".to_owned(),
                    },
                    ttl: RuntimeDuration("1h".to_owned()),
                    runner: RunnerRequirements::default(),
                },
            },
            "admin@example.com",
        )
        .await?;
    harness.wait_phase(PRICED_RUNTIME, "Suspended", Duration::from_secs(120))?;
    harness.assert_key_absent(&alias)?;
    Ok(())
}

fn number_value(value: &serde_json::Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|number| number.parse().ok()))
}

fn path_text(path: &Path) -> Result<&str, io::Error> {
    path.to_str()
        .ok_or_else(|| io::Error::other("test path is not UTF-8"))
}
