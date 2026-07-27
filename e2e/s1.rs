use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const NAMESPACE: &str = "team-a";
const RUNTIME_NAME: &str = "runtime-s1";
const MCP_URL: &str = "http://mcp-gw.steward-system.svc.cluster.local:8080/mcp";

struct Harness {
    context: String,
    controller: Option<Child>,
    kubeconfig: PathBuf,
    mint_forward: Option<Child>,
    openshell: PathBuf,
    runtime_manifest: PathBuf,
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
            controller: None,
            kubeconfig: PathBuf::from(env::var("STEWARD_TEST_KUBECONFIG")?),
            mint_forward: None,
            openshell: PathBuf::from(env::var("STEWARD_OPENSHELL_CLI")?),
            runtime_manifest: PathBuf::from(env::var("STEWARD_RUN_DIR")?)
                .join("e2e-s1-runtime.json"),
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
        self.controller = Some(
            Command::new(env::var("STEWARD_CONTROLLER_BIN")?)
                .env("KUBECONFIG", &self.kubeconfig)
                .env("STEWARD_OPENSHELL_ENDPOINT", env::var("STEWARD_OPENSHELL_ENDPOINT")?)
                .env("STEWARD_S0_BOOTSTRAP", "1")
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .spawn()?,
        );
        Ok(())
    }

    fn write_runtime(&self, acting_user: &str) -> Result<(), Box<dyn Error>> {
        let manifest = serde_json::json!({
            "apiVersion": env::var("STEWARD_AGENTRUNTIME_API_VERSION")?,
            "kind": "AgentRuntime",
            "metadata": {
                "name": RUNTIME_NAME,
                "namespace": NAMESPACE,
            },
            "spec": {
                "principal": {
                    "kind": "user",
                    "actingUser": acting_user,
                },
                "owner": acting_user,
                "agentType": { "name": "base" },
                "llms": [],
                "tools": [{
                    "provider": "github",
                    "resource": "search_repositories",
                    "action": "read",
                }],
                "budget": {
                    "monthlyLimit": "1.00",
                    "currency": "USD",
                },
                "ttl": "1h",
            },
        });
        fs::write(&self.runtime_manifest, serde_json::to_vec_pretty(&manifest)?)?;
        self.kubectl_ok(&["apply", "-f", path_text(&self.runtime_manifest)?])?;
        Ok(())
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

    fn call_tool(&self, tool: &str) -> Result<String, Box<dyn Error>> {
        let workspace = self.runtime_ref("workspace")?;
        let sandbox = self.runtime_ref("sandbox")?;
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": tool,
                "arguments": {},
            },
        })
        .to_string();
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
                "-H",
                "MCP-Protocol-Version: 2025-06-18",
                "-d",
                &request,
                MCP_URL,
            ])
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "sandbox tool call failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))
            .into());
        }
        Ok(String::from_utf8(output.stdout)?)
    }

    fn start_mint_forward(&mut self) -> Result<u16, Box<dyn Error>> {
        let namespace = env::var("STEWARD_MINT_TEST_NAMESPACE")?;
        let service = env::var("STEWARD_MINT_TEST_SERVICE")?;
        let log = PathBuf::from(env::var("STEWARD_RUN_DIR")?).join("mint-port-forward.log");
        let stdout = fs::File::create(&log)?;
        let stderr = stdout.try_clone()?;
        self.mint_forward = Some(
            Command::new("kubectl")
                .args(["--kubeconfig"])
                .arg(&self.kubeconfig)
                .args([
                    "--context",
                    &self.context,
                    "-n",
                    &namespace,
                    "port-forward",
                    &format!("svc/{service}"),
                    ":8080",
                ])
                .stdout(stdout)
                .stderr(stderr)
                .spawn()?,
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            let contents = fs::read_to_string(&log)?;
            if let Some(port) = parse_forwarded_port(&contents) {
                return Ok(port);
            }
            thread::sleep(Duration::from_millis(200));
        }
        Err(io::Error::other("mint port-forward did not become ready").into())
    }

    fn forged_svid_status(&mut self) -> Result<u16, Box<dyn Error>> {
        let port = self.start_mint_forward()?;
        let output = Command::new("curl")
            .args([
                "-sS",
                "-o",
                "/dev/null",
                "-w",
                "%{http_code}",
                "-X",
                "POST",
                "-H",
                "Content-Type: application/x-www-form-urlencoded",
                "--data-urlencode",
                "grant_type=client_credentials",
                "--data-urlencode",
                "client_assertion_type=urn:ietf:params:oauth:client-assertion-type:jwt-spiffe",
                "--data-urlencode",
                "client_assertion=eyJhbGciOiJub25lIn0.eyJleHAiOjB9.",
                "--data-urlencode",
                "audience=steward-mcp",
                "--data-urlencode",
                "scope=mcp",
                &format!("http://127.0.0.1:{port}/token"),
            ])
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other("forged SVID request failed to execute").into());
        }
        String::from_utf8(output.stdout)?
            .parse()
            .map_err(|error| io::Error::other(format!("invalid HTTP status: {error}")).into())
    }

    fn delete_runtime(&self) -> Result<(), Box<dyn Error>> {
        let output = self.kubectl(&[
            "-n",
            NAMESPACE,
            "delete",
            "agentruntime",
            RUNTIME_NAME,
            "--timeout=120s",
        ])?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "runtime teardown failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))
            .into());
        }
        Ok(())
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.delete_runtime();
        if let Some(mut controller) = self.controller.take() {
            let _ = controller.kill();
            let _ = controller.wait();
        }
        if let Some(mut forward) = self.mint_forward.take() {
            let _ = forward.kill();
            let _ = forward.wait();
        }
    }
}

fn path_text(path: &std::path::Path) -> Result<&str, io::Error> {
    path.to_str()
        .ok_or_else(|| io::Error::other("test path is not valid UTF-8"))
}

fn parse_forwarded_port(log: &str) -> Option<u16> {
    log.lines().find_map(|line| {
        let (_, suffix) = line.split_once("127.0.0.1:")?;
        suffix.split(" ->").next()?.parse().ok()
    })
}

#[test]
fn e2e_s1_tool_call_as_acting_user() -> Result<(), Box<dyn Error>> {
    let mut harness = Harness::from_environment()?;
    harness.start_controller()?;
    harness.write_runtime("alice@example.com")?;
    harness.wait_phase("Running", Duration::from_secs(300))?;

    let allowed = harness.call_tool("search_repositories")?;
    assert!(
        allowed.contains("example-org/fixture-repository"),
        "the acting user's provider credential was not resolved: {allowed}"
    );

    let denied = harness.call_tool("create_issue")?;
    assert!(
        denied.contains("Policy denied create_issue"),
        "a tool outside spec.tools was not rejected by mcp-gw: {denied}"
    );

    assert_eq!(
        harness.forged_svid_status()?,
        401,
        "a forged and expired SVID must fail closed at the mint"
    );

    harness.write_runtime("bob@example.org")?;
    thread::sleep(Duration::from_secs(3));
    let wrong_user = harness.call_tool("search_repositories")?;
    assert!(
        wrong_user.contains("GitHub account is not connected"),
        "a HOP-1 for a different email obtained the acting user's credential: {wrong_user}"
    );

    harness.delete_runtime()?;
    Ok(())
}
