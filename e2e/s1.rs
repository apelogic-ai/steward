use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const ALICE_NAMESPACE: &str = "team-a";
const ALICE_RUNTIME: &str = "runtime-alice";
const ALICE_CANONICAL_USER: &str = "usr_0123456789abcdef0123456789abcdef";
const BOB_NAMESPACE: &str = "team-b";
const BOB_RUNTIME: &str = "runtime-bob";
const BOB_CANONICAL_USER: &str = "usr_abcdef0123456789abcdef0123456789";
const DELEGATED_SERVICE_RUNTIME: &str = "runtime-steward-run";
const PURE_SERVICE_RUNTIME: &str = "runtime-scheduled-scanner";
const MCP_URL: &str = "http://mcp-gw.steward-system.svc.cluster.local:8080/mcp";

struct Harness {
    context: String,
    controller: Option<Child>,
    kubeconfig: PathBuf,
    mint_forward: Option<Child>,
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
            controller: None,
            kubeconfig: PathBuf::from(env::var("STEWARD_TEST_KUBECONFIG")?),
            mint_forward: None,
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
                .env(
                    "STEWARD_OPENSHELL_ENDPOINT",
                    env::var("STEWARD_OPENSHELL_ENDPOINT")?,
                )
                .env(
                    "STEWARD_OPENSHELL_CA_CERTIFICATE_FILE",
                    env::var("STEWARD_OPENSHELL_CA_CERTIFICATE_FILE")?,
                )
                .env(
                    "STEWARD_OPENSHELL_CLIENT_CERTIFICATE_FILE",
                    env::var("STEWARD_OPENSHELL_CLIENT_CERTIFICATE_FILE")?,
                )
                .env(
                    "STEWARD_OPENSHELL_CLIENT_PRIVATE_KEY_FILE",
                    env::var("STEWARD_OPENSHELL_CLIENT_PRIVATE_KEY_FILE")?,
                )
                .env(
                    "STEWARD_WORKLOAD_EXCHANGE_ENDPOINT",
                    env::var("STEWARD_WORKLOAD_EXCHANGE_ENDPOINT")?,
                )
                .env(
                    "STEWARD_WORKLOAD_EXCHANGE_SERVER_NAME",
                    env::var("STEWARD_WORKLOAD_EXCHANGE_SERVER_NAME")?,
                )
                .env(
                    "STEWARD_WORKLOAD_EXCHANGE_CA_CERTIFICATE_FILE",
                    env::var("STEWARD_WORKLOAD_EXCHANGE_CA_CERTIFICATE_FILE")?,
                )
                .env(
                    "STEWARD_WORKLOAD_SOURCE_CREDENTIAL_FILE",
                    env::var("STEWARD_WORKLOAD_SOURCE_CREDENTIAL_FILE")?,
                )
                .env(
                    "STEWARD_OPENSHELL_SERVER_NAME",
                    env::var("STEWARD_OPENSHELL_SERVER_NAME")?,
                )
                .env(
                    "STEWARD_OPENSHELL_RUNTIME_CLASS_NAME",
                    env::var("STEWARD_OPENSHELL_RUNTIME_CLASS_NAME")?,
                )
                .env("STEWARD_S0_BOOTSTRAP", "1")
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .spawn()?,
        );
        Ok(())
    }

    fn write_runtime(
        &self,
        namespace: &str,
        name: &str,
        acting_user: &str,
        tools_enabled: bool,
    ) -> Result<(), Box<dyn Error>> {
        let runtime_manifest = self.run_dir.join(format!("e2e-s1-{name}.json"));
        let tools = tools_enabled
            .then(|| {
                serde_json::json!({
                    "provider": "github",
                    "resource": "search_repositories",
                    "action": "read",
                })
            })
            .into_iter()
            .collect::<Vec<_>>();
        let manifest = serde_json::json!({
            "apiVersion": env::var("STEWARD_AGENTRUNTIME_API_VERSION")?,
            "kind": "AgentRuntime",
            "metadata": {
                "name": name,
                "namespace": namespace,
            },
            "spec": {
                "principal": {
                    "kind": "user",
                    "actingUser": acting_user,
                },
                "owner": acting_user,
                "canonicalAuthority": canonical_authority(acting_user)?,
                "agentType": { "name": "base" },
                "llms": [],
                "tools": tools,
                "budget": {
                    "monthlyLimit": "1.00",
                    "currency": "USD",
                },
                "ttl": "1h",
            },
        });
        fs::write(&runtime_manifest, serde_json::to_vec_pretty(&manifest)?)?;
        self.kubectl_ok(&["apply", "-f", path_text(&runtime_manifest)?])?;
        Ok(())
    }

    fn write_service_runtime(
        &self,
        namespace: &str,
        name: &str,
        service: &str,
        acting_user: Option<&str>,
    ) -> Result<(), Box<dyn Error>> {
        let runtime_manifest = self.run_dir.join(format!("e2e-s1-{name}.json"));
        let mut principal = serde_json::json!({
            "kind": "service",
            "name": service,
        });
        if let Some(acting_user) = acting_user {
            principal["actingUser"] = serde_json::json!(acting_user);
        }
        let canonical_authority = acting_user.map(canonical_authority).transpose()?;
        let manifest = serde_json::json!({
            "apiVersion": env::var("STEWARD_AGENTRUNTIME_API_VERSION")?,
            "kind": "AgentRuntime",
            "metadata": {
                "name": name,
                "namespace": namespace,
                "annotations": {
                    "agents.apelogic.ai/service-principal": service,
                },
            },
            "spec": {
                "principal": principal,
                "owner": "alice@example.com",
                "canonicalAuthority": canonical_authority,
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
        fs::write(&runtime_manifest, serde_json::to_vec_pretty(&manifest)?)?;
        self.kubectl_ok(&["apply", "-f", path_text(&runtime_manifest)?])?;
        Ok(())
    }

    fn wait_phase(
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
                        "AgentRuntime {namespace}/{name} reached Failed while waiting for {expected}"
                    ))
                    .into());
                }
            }
            thread::sleep(Duration::from_secs(1));
        }
        Err(io::Error::other(format!(
            "AgentRuntime {namespace}/{name} did not reach {expected}; last phase was {last:?}"
        ))
        .into())
    }

    fn runtime_ref(
        &self,
        namespace: &str,
        name: &str,
        field: &str,
    ) -> Result<String, Box<dyn Error>> {
        self.kubectl_ok(&[
            "-n",
            namespace,
            "get",
            "agentruntime",
            name,
            "-o",
            &format!("jsonpath={{.status.refs.{field}}}"),
        ])
        .map(|value| value.trim().to_owned())
    }

    fn call_tool(&self, namespace: &str, name: &str, tool: &str) -> Result<String, Box<dyn Error>> {
        let workspace = self.runtime_ref(namespace, name, "workspace")?;
        let sandbox = self.runtime_ref(namespace, name, "sandbox")?;
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

    fn wait_tool_response(
        &self,
        namespace: &str,
        name: &str,
        tool: &str,
        expected: &str,
        timeout: Duration,
    ) -> Result<String, Box<dyn Error>> {
        let deadline = Instant::now() + timeout;
        let mut last = "tool call was not attempted".to_owned();
        while Instant::now() < deadline {
            match self.call_tool(namespace, name, tool) {
                Ok(response) if response.contains(expected) => return Ok(response),
                Ok(response) => last = response,
                Err(error) => last = error.to_string(),
            }
            thread::sleep(Duration::from_secs(1));
        }
        Err(io::Error::other(format!(
            "tool call for {namespace}/{name} did not contain {expected:?}; last result: {last}"
        ))
        .into())
    }

    fn tool_provider_is_attached(
        &self,
        namespace: &str,
        name: &str,
    ) -> Result<bool, Box<dyn Error>> {
        let workspace = self.runtime_ref(namespace, name, "workspace")?;
        let sandbox = self.runtime_ref(namespace, name, "sandbox")?;
        let output = Command::new(&self.openshell)
            .args(["--gateway-endpoint"])
            .arg(env::var("STEWARD_OPENSHELL_ENDPOINT")?)
            .args([
                "--workspace",
                &workspace,
                "sandbox",
                "provider",
                "list",
                &sandbox,
            ])
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "sandbox provider lookup failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))
            .into());
        }
        Ok(String::from_utf8(output.stdout)?.contains("steward-mcp-gw"))
    }

    fn wait_tool_provider_attached(
        &self,
        namespace: &str,
        name: &str,
        expected: bool,
        timeout: Duration,
    ) -> Result<(), Box<dyn Error>> {
        let deadline = Instant::now() + timeout;
        let mut last = !expected;
        while Instant::now() < deadline {
            last = self.tool_provider_is_attached(namespace, name)?;
            if last == expected {
                return Ok(());
            }
            thread::sleep(Duration::from_secs(1));
        }
        Err(io::Error::other(format!(
            "tool provider attachment for {namespace}/{name} remained {last}, expected {expected}"
        ))
        .into())
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

    fn delete_runtime(&self, namespace: &str, name: &str) -> Result<(), Box<dyn Error>> {
        let output = self.kubectl(&[
            "-n",
            namespace,
            "delete",
            "agentruntime",
            name,
            "--ignore-not-found=true",
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
        let _ = self.delete_runtime(ALICE_NAMESPACE, PURE_SERVICE_RUNTIME);
        let _ = self.delete_runtime(ALICE_NAMESPACE, DELEGATED_SERVICE_RUNTIME);
        let _ = self.delete_runtime(BOB_NAMESPACE, BOB_RUNTIME);
        let _ = self.delete_runtime(ALICE_NAMESPACE, ALICE_RUNTIME);
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

fn canonical_authority(acting_user: &str) -> Result<serde_json::Value, io::Error> {
    let canonical_user = match acting_user {
        "alice@example.com" => ALICE_CANONICAL_USER,
        "bob@example.org" => BOB_CANONICAL_USER,
        _ => {
            return Err(io::Error::other(format!(
                "S1 fixture has no canonical authority for acting user {acting_user}"
            )));
        }
    };
    Ok(serde_json::json!({
        "schemaVersion": "steward/canonical-authority-binding/v1",
        "ownerUserId": canonical_user,
        "actingUserId": canonical_user,
    }))
}

#[test]
fn s1_provider_seed_uses_alices_canonical_hop1_subject() {
    let seed = include_str!("../config/s1/seed-mcp-gw.ts");
    assert!(
        seed.contains(&format!(r#"hop1Subject: "{ALICE_CANONICAL_USER}""#)),
        "the S1 GitHub fixture must use the canonical HOP-1 subject, not Alice's mutable email"
    );
}

#[test]
fn s1_ephemeral_runtimes_use_the_same_canonical_authority_as_hop1() -> Result<(), Box<dyn Error>> {
    let alice = canonical_authority("alice@example.com")?;
    assert_eq!(alice["ownerUserId"], ALICE_CANONICAL_USER);
    assert_eq!(alice["actingUserId"], ALICE_CANONICAL_USER);
    let bob = canonical_authority("bob@example.org")?;
    assert_eq!(bob["ownerUserId"], BOB_CANONICAL_USER);
    assert_eq!(bob["actingUserId"], BOB_CANONICAL_USER);
    assert!(canonical_authority("unknown@example.org").is_err());
    Ok(())
}

#[test]
fn e2e_s1_tool_call_as_acting_user() -> Result<(), Box<dyn Error>> {
    let mut harness = Harness::from_environment()?;
    harness.start_controller()?;
    harness.write_runtime(ALICE_NAMESPACE, ALICE_RUNTIME, "alice@example.com", false)?;
    harness.wait_phase(
        ALICE_NAMESPACE,
        ALICE_RUNTIME,
        "Running",
        Duration::from_secs(300),
    )?;

    harness.write_runtime(ALICE_NAMESPACE, ALICE_RUNTIME, "alice@example.com", true)?;
    harness.wait_tool_provider_attached(
        ALICE_NAMESPACE,
        ALICE_RUNTIME,
        true,
        Duration::from_secs(60),
    )?;
    harness.wait_tool_response(
        ALICE_NAMESPACE,
        ALICE_RUNTIME,
        "search_repositories",
        "example-org/fixture-repository",
        Duration::from_secs(60),
    )?;

    let denied = harness.call_tool(ALICE_NAMESPACE, ALICE_RUNTIME, "create_issue")?;
    assert!(
        denied.contains("Policy denied create_issue"),
        "a tool outside spec.tools was not rejected by mcp-gw: {denied}"
    );

    assert_eq!(
        harness.forged_svid_status()?,
        401,
        "a forged and expired SVID must fail closed at the mint"
    );

    harness.write_runtime(ALICE_NAMESPACE, ALICE_RUNTIME, "alice@example.com", false)?;
    harness.wait_tool_provider_attached(
        ALICE_NAMESPACE,
        ALICE_RUNTIME,
        false,
        Duration::from_secs(60),
    )?;

    harness.write_runtime(BOB_NAMESPACE, BOB_RUNTIME, "bob@example.org", true)?;
    harness.wait_phase(
        BOB_NAMESPACE,
        BOB_RUNTIME,
        "Running",
        Duration::from_secs(300),
    )?;
    harness.wait_tool_provider_attached(
        BOB_NAMESPACE,
        BOB_RUNTIME,
        true,
        Duration::from_secs(60),
    )?;
    harness.wait_tool_response(
        BOB_NAMESPACE,
        BOB_RUNTIME,
        "search_repositories",
        "GitHub account is not connected",
        Duration::from_secs(60),
    )?;

    harness.write_service_runtime(
        ALICE_NAMESPACE,
        DELEGATED_SERVICE_RUNTIME,
        "steward-run",
        Some("alice@example.com"),
    )?;
    harness.wait_phase(
        ALICE_NAMESPACE,
        DELEGATED_SERVICE_RUNTIME,
        "Running",
        Duration::from_secs(300),
    )?;
    harness.wait_tool_provider_attached(
        ALICE_NAMESPACE,
        DELEGATED_SERVICE_RUNTIME,
        true,
        Duration::from_secs(60),
    )?;
    harness.wait_tool_response(
        ALICE_NAMESPACE,
        DELEGATED_SERVICE_RUNTIME,
        "search_repositories",
        "example-org/fixture-repository",
        Duration::from_secs(60),
    )?;

    harness.write_service_runtime(
        ALICE_NAMESPACE,
        PURE_SERVICE_RUNTIME,
        "scheduled-scanner",
        None,
    )?;
    harness.wait_phase(
        ALICE_NAMESPACE,
        PURE_SERVICE_RUNTIME,
        "Running",
        Duration::from_secs(300),
    )?;
    harness.wait_tool_provider_attached(
        ALICE_NAMESPACE,
        PURE_SERVICE_RUNTIME,
        true,
        Duration::from_secs(60),
    )?;
    harness.wait_tool_response(
        ALICE_NAMESPACE,
        PURE_SERVICE_RUNTIME,
        "search_repositories",
        "example-org/fixture-repository",
        Duration::from_secs(60),
    )?;

    harness.delete_runtime(ALICE_NAMESPACE, PURE_SERVICE_RUNTIME)?;
    harness.delete_runtime(ALICE_NAMESPACE, DELEGATED_SERVICE_RUNTIME)?;
    harness.delete_runtime(BOB_NAMESPACE, BOB_RUNTIME)?;
    harness.delete_runtime(ALICE_NAMESPACE, ALICE_RUNTIME)?;
    Ok(())
}
