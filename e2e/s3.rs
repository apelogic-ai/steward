use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const NAMESPACE: &str = "team-a";
const RUNTIME_NAME: &str = "runtime-a";
const COUNTEREXAMPLE: &str =
    "envelope exceeded: budget.monthlyLimit requested 220.00 USD, ceiling 200.00 USD";

struct Harness {
    api_url: String,
    ca_certificate: PathBuf,
    context: String,
    kubeconfig: PathBuf,
    resolve: String,
    run_directory: PathBuf,
}

impl Harness {
    fn from_environment() -> Result<Self, Box<dyn Error>> {
        let context = env::var("STEWARD_TEST_KUBE_CONTEXT")?;
        if !context.starts_with("kind-steward-s3-") {
            return Err(io::Error::other(format!(
                "refusing non-S3 ephemeral kube context: {context}"
            ))
            .into());
        }
        Ok(Self {
            api_url: env::var("STEWARD_S3_URL")?,
            ca_certificate: PathBuf::from(env::var("STEWARD_TEST_TLS_CA")?),
            context,
            kubeconfig: PathBuf::from(env::var("STEWARD_TEST_KUBECONFIG")?),
            resolve: env::var("STEWARD_S3_RESOLVE")?,
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

    fn curl(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
        output_name: &str,
    ) -> Result<(u16, String), Box<dyn Error>> {
        let output_path = self.run_directory.join(output_name);
        let mut command = Command::new("curl");
        command
            .args(["--silent", "--show-error", "--cacert"])
            .arg(&self.ca_certificate)
            .args(["--resolve", &self.resolve, "--request", method, "--output"])
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

    fn write_runtime(&self, filename: &str, budget: &str) -> Result<PathBuf, Box<dyn Error>> {
        let path = self.run_directory.join(filename);
        let manifest = serde_json::json!({
            "apiVersion": "agents.apelogic.ai/v1alpha1",
            "kind": "AgentRuntime",
            "metadata": {
                "name": RUNTIME_NAME,
                "namespace": NAMESPACE,
            },
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
                    "monthlyLimit": budget,
                    "currency": "USD",
                },
                "ttl": "24h",
            },
        });
        fs::write(&path, serde_json::to_vec_pretty(&manifest)?)?;
        Ok(path)
    }

    fn apply_as_acting_user(&self, manifest: &Path) -> Result<Output, Box<dyn Error>> {
        self.kubectl(&[
            "--as",
            "alice@example.com",
            "--as-group",
            "agents.apelogic.ai/member-role:engineer",
            "apply",
            "-f",
            path_text(manifest)?,
        ])
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
fn e2e_s3_composed_edits_rejected() -> Result<(), Box<dyn Error>> {
    let harness = Harness::from_environment()?;
    harness.kubectl_ok(&["create", "namespace", NAMESPACE])?;

    let envelope = serde_json::json!({
        "revision": 1,
        "spec": {
            "llms": [{"provider": "provider-a", "model": "model-a"}],
            "tools": [],
            "budget": {"monthlyLimit": "200.00", "currency": "USD"},
            "ttl": "24h",
        },
    });
    let (status, _) = harness.curl(
        "POST",
        "/admin/envelopes/engineer",
        Some(&envelope.to_string()),
        "authored-envelope.json",
    )?;
    assert_eq!(status, 201, "member-role envelope must be authored");

    let initial = harness.write_runtime("runtime-initial.json", "100.00")?;
    let initial_apply = harness.apply_as_acting_user(&initial)?;
    if !initial_apply.status.success() {
        return Err(io::Error::other(format!(
            "initial in-envelope manifest was denied: {}",
            String::from_utf8_lossy(&initial_apply.stderr).trim()
        ))
        .into());
    }

    let edit = r#"{"amount":"60.00"}"#;
    let (first_status, first_body) = harness.curl(
        "PATCH",
        "/v1/namespaces/team-a/runtimes/runtime-a/budget",
        Some(edit),
        "first-edit.json",
    )?;
    assert_eq!(first_status, 200, "first +60 edit must compose to 160");
    let first = serde_json::from_str::<serde_json::Value>(&first_body)?;
    assert_eq!(
        first.pointer("/proposedSpec/budget/monthlyLimit"),
        Some(&serde_json::json!("160.00"))
    );
    assert_eq!(
        harness
            .kubectl_ok(&[
                "-n",
                NAMESPACE,
                "get",
                "agentruntime",
                RUNTIME_NAME,
                "-o",
                "jsonpath={.spec.budget.monthlyLimit}",
            ])?
            .trim(),
        "160.00"
    );
    let runtime_uid = harness
        .kubectl_ok(&[
            "-n",
            NAMESPACE,
            "get",
            "agentruntime",
            RUNTIME_NAME,
            "-o",
            "jsonpath={.metadata.uid}",
        ])?
        .trim()
        .to_owned();
    assert!(!runtime_uid.is_empty(), "runtime UID must be assigned");

    let (second_status, second_body) = harness.curl(
        "PATCH",
        "/v1/namespaces/team-a/runtimes/runtime-a/budget",
        Some(edit),
        "second-edit.json",
    )?;
    assert_eq!(second_status, 202, "second +60 edit must park");
    let second = serde_json::from_str::<serde_json::Value>(&second_body)?;
    assert_eq!(
        second
            .get("counterexample")
            .and_then(serde_json::Value::as_str),
        Some(COUNTEREXAMPLE)
    );
    assert_eq!(
        second.pointer("/proposedSpec/budget/monthlyLimit"),
        Some(&serde_json::json!("220.00"))
    );
    assert_eq!(
        harness
            .kubectl_ok(&[
                "-n",
                NAMESPACE,
                "get",
                "agentruntime",
                RUNTIME_NAME,
                "-o",
                "jsonpath={.spec.budget.monthlyLimit}",
            ])?
            .trim(),
        "160.00",
        "parked API request must not mutate desired state"
    );

    let over_limit = harness.write_runtime("runtime-over-limit.json", "220.00")?;
    let kubectl_denial = harness.apply_as_acting_user(&over_limit)?;
    assert!(
        !kubectl_denial.status.success(),
        "the same over-envelope manifest must be hard-denied through kubectl"
    );
    let kubectl_message = format!(
        "{}{}",
        String::from_utf8_lossy(&kubectl_denial.stdout),
        String::from_utf8_lossy(&kubectl_denial.stderr)
    );
    assert!(
        kubectl_message.contains(COUNTEREXAMPLE),
        "kubectl denial must contain the API's exact counterexample: {kubectl_message}"
    );

    let (queue_status, queue) =
        harness.curl("GET", "/admin/approvals", None, "approval-queue.html")?;
    assert_eq!(queue_status, 200);
    for expected in [
        runtime_uid.as_str(),
        "engineer",
        "alice@example.com",
        "requested 220.00 USD",
        "ceiling 200.00 USD",
    ] {
        assert!(
            queue.contains(expected),
            "approval queue must render {expected:?} from the parked row"
        );
    }
    Ok(())
}

fn path_text(path: &Path) -> Result<&str, Box<dyn Error>> {
    path.to_str()
        .ok_or_else(|| io::Error::other("run path is not valid UTF-8").into())
}
