use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const NAMESPACE: &str = "team-a";
const RUNTIME_NAME: &str = "runtime-a";

struct Harness {
    binary: PathBuf,
    context: String,
    controller: Option<Child>,
    kubeconfig: PathBuf,
    manifest: PathBuf,
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
        let run_dir = PathBuf::from(env::var("STEWARD_RUN_DIR")?);
        Ok(Self {
            binary: PathBuf::from(env::var("STEWARD_CONTROLLER_BIN")?),
            context,
            controller: None,
            kubeconfig: PathBuf::from(env::var("STEWARD_TEST_KUBECONFIG")?),
            manifest: run_dir.join("e2e-s0-runtime.json"),
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

    fn start_controller(&mut self) -> Result<(), Box<dyn Error>> {
        let child = Command::new(&self.binary)
            .env("KUBECONFIG", &self.kubeconfig)
            .env("STEWARD_TEST_KUBE_CONTEXT", &self.context)
            .env(
                "STEWARD_OPENSHELL_ENDPOINT",
                env::var("STEWARD_OPENSHELL_ENDPOINT")?,
            )
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()?;
        self.controller = Some(child);
        Ok(())
    }

    fn stop_controller(&mut self) -> Result<(), Box<dyn Error>> {
        if let Some(mut child) = self.controller.take() {
            child.kill()?;
            let _status = child.wait()?;
        }
        Ok(())
    }

    fn restart_controller(&mut self) -> Result<(), Box<dyn Error>> {
        self.stop_controller()?;
        self.start_controller()
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
            "AgentRuntime did not reach {expected}; last phase was {last:?}"
        ))
        .into())
    }

    fn runtime_ref(&self, field: &str) -> Result<String, Box<dyn Error>> {
        self.kubectl_ok(&[
            "-n",
            NAMESPACE,
            "get",
            "agentruntime",
            RUNTIME_NAME,
            "-o",
            &format!("jsonpath={{.status.refs.{field}}}"),
        ])
        .map(|value| value.trim().to_owned())
    }

    fn inject_stale_status_cache(&self) -> Result<(), Box<dyn Error>> {
        self.kubectl_ok(&[
            "-n",
            NAMESPACE,
            "patch",
            "agentruntime",
            RUNTIME_NAME,
            "--subresource=status",
            "--type=merge",
            "-p",
            r#"{"status":{"refs":{"litellmKey":"stale-key-ref"},"spend":{"observedAmount":"9.99","currency":"USD"}}}"#,
        ])?;
        Ok(())
    }

    fn wait_status_cache_cleared(&self, timeout: Duration) -> Result<(), Box<dyn Error>> {
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
                "jsonpath={.status.refs.litellmKey}:{.status.spend.observedAmount}",
            ])?;
            if output.status.success() {
                last = String::from_utf8(output.stdout)?;
                if last == ":" {
                    return Ok(());
                }
            }
            thread::sleep(Duration::from_secs(1));
        }
        Err(io::Error::other(format!(
            "reconcile did not clear stale status cache fields; last value was {last:?}"
        ))
        .into())
    }

    fn begin_delete_runtime(&self) -> Result<(), Box<dyn Error>> {
        self.kubectl_ok(&[
            "-n",
            NAMESPACE,
            "delete",
            "agentruntime",
            RUNTIME_NAME,
            "--wait=false",
        ])?;
        Ok(())
    }

    fn terminating_waiter(&self) -> Result<Child, Box<dyn Error>> {
        Ok(Command::new("kubectl")
            .args(["--kubeconfig"])
            .arg(&self.kubeconfig)
            .args([
                "--context",
                &self.context,
                "-n",
                NAMESPACE,
                "wait",
                "--for=jsonpath={.status.phase}=Terminating",
                "agentruntime",
                RUNTIME_NAME,
                "--timeout=30s",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?)
    }

    fn wait_for_terminating(&self, mut waiter: Child) -> Result<(), Box<dyn Error>> {
        thread::sleep(Duration::from_millis(500));
        if waiter.try_wait()?.is_some() {
            return Err(io::Error::other(
                "Terminating phase waiter exited before runtime deletion began",
            )
            .into());
        }
        self.begin_delete_runtime()?;
        let output = waiter.wait_with_output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "AgentRuntime did not expose Terminating: {}",
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
        Err(io::Error::other("AgentRuntime deletion did not complete").into())
    }

    fn agent_sandbox_name(&self, workspace: &str, sandbox: &str) -> Result<String, Box<dyn Error>> {
        self.kubectl_ok(&[
            "-n",
            "openshell",
            "get",
            "sandboxes.agents.x-k8s.io",
            "--selector",
            &format!(
                "openshell.ai/sandbox-workspace={workspace},openshell.ai/sandbox-name={sandbox}"
            ),
            "-o",
            "jsonpath={.items[0].metadata.name}",
        ])
        .map(|value| value.trim().to_owned())
    }

    fn write_runtime_manifest(&self) -> Result<(), Box<dyn Error>> {
        let api_version = env::var("STEWARD_AGENTRUNTIME_API_VERSION")?;
        let manifest = format!(
            r#"{{
  "apiVersion": "{api_version}",
  "kind": "AgentRuntime",
  "metadata": {{
    "name": "{RUNTIME_NAME}",
    "namespace": "{NAMESPACE}"
  }},
  "spec": {{
    "principal": {{
      "kind": "user",
      "actingUser": "alice@example.com"
    }},
    "owner": "alice@example.com",
    "agentType": {{
      "name": "base"
    }},
    "llms": [],
    "tools": [],
    "budget": {{
      "monthlyLimit": "1.00",
      "currency": "USD"
    }},
    "ttl": "1h"
  }}
}}
"#
        );
        fs::write(&self.manifest, manifest)?;
        Ok(())
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if self.controller.is_some() {
            let _result = self.begin_delete_runtime();
            let _result = self.wait_runtime_absent(Duration::from_secs(300));
        }
        let _result = self.stop_controller();
        let _result = self.kubectl(&[
            "delete",
            "namespace",
            NAMESPACE,
            "--ignore-not-found=true",
            "--wait=true",
            "--timeout=120s",
        ]);
        let _result = fs::remove_file(&self.manifest);
    }
}

fn assert_agent_sandbox_absent(
    harness: &Harness,
    workspace: &str,
    sandbox: &str,
) -> Result<(), Box<dyn Error>> {
    let output = harness.kubectl_ok(&[
        "-n",
        "openshell",
        "get",
        "sandboxes.agents.x-k8s.io",
        "--selector",
        &format!("openshell.ai/sandbox-workspace={workspace},openshell.ai/sandbox-name={sandbox}"),
        "-o",
        "name",
    ])?;
    if !output.trim().is_empty() {
        return Err(io::Error::other(format!(
            "OpenShell tuple {workspace}/{sandbox} still has Agent Sandbox resources: {}",
            output.trim()
        ))
        .into());
    }
    Ok(())
}

#[test]
fn e2e_s0_provision_and_teardown() -> Result<(), Box<dyn Error>> {
    let mut harness = Harness::from_environment()?;
    harness.kubectl_ok(&["create", "namespace", NAMESPACE])?;
    harness.write_runtime_manifest()?;
    harness.start_controller()?;
    harness.kubectl_ok(&["apply", "-f", path_text(&harness.manifest)?])?;

    harness.wait_phase("Provisioning", Duration::from_secs(120))?;
    let workspace_before = harness.runtime_ref("workspace")?;
    let sandbox_before = harness.runtime_ref("sandbox")?;
    if workspace_before.is_empty() || sandbox_before.is_empty() {
        return Err(io::Error::other("Provisioning status did not populate refs").into());
    }

    harness.restart_controller()?;
    harness.wait_phase("Running", Duration::from_secs(600))?;
    let workspace_after = harness.runtime_ref("workspace")?;
    let sandbox_after = harness.runtime_ref("sandbox")?;
    assert_eq!(
        (workspace_after.as_str(), sandbox_after.as_str()),
        (workspace_before.as_str(), sandbox_before.as_str()),
        "controller restart must converge on the same external objects"
    );

    harness.inject_stale_status_cache()?;
    harness.wait_status_cache_cleared(Duration::from_secs(30))?;

    let sandbox_resource = harness.agent_sandbox_name(&workspace_after, &sandbox_after)?;
    if sandbox_resource.is_empty() {
        return Err(io::Error::other(
            "running sandbox was not discoverable by OpenShell's workspace/name labels",
        )
        .into());
    }
    let terminating_waiter = harness.terminating_waiter()?;
    harness.wait_for_terminating(terminating_waiter)?;
    harness.wait_runtime_absent(Duration::from_secs(300))?;
    assert_agent_sandbox_absent(&harness, &workspace_after, &sandbox_after)?;
    harness.stop_controller()?;
    Ok(())
}

fn path_text(path: &Path) -> Result<&str, Box<dyn Error>> {
    path.to_str()
        .ok_or_else(|| io::Error::other("run path is not valid UTF-8").into())
}
