use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use steward_adapter_openshell::{OpenShellConnectionConfig, OpenShellRuntime};
use steward_ports::{
    SandboxObservation, SandboxRequest, SandboxRuntime, SandboxTaskRequest, SandboxTaskRuntime,
};
use steward_types::{AgentType, RuntimeId, RuntimeRefs};
use tokio::time::sleep;

const EXPECTED_RELEASE: &str = "v0.0.98";

fn required(name: &str) -> Result<String, String> {
    env::var(name).map_err(|_| format!("{name} is required from the ephemeral OpenShell harness"))
}

fn required_file(name: &str) -> Result<Vec<u8>, String> {
    let path = required(name)?;
    fs::read(&path).map_err(|error| format!("failed to read {name}: {error}"))
}

fn required_path(name: &str) -> Result<PathBuf, String> {
    required(name).map(PathBuf::from)
}

fn valid_config() -> Result<OpenShellConnectionConfig, String> {
    Ok(OpenShellConnectionConfig {
        endpoint: required("STEWARD_OPENSHELL_ENDPOINT")?,
        ca_certificate_pem: required_file("STEWARD_OPENSHELL_CA_CERTIFICATE_FILE")?,
        client_certificate_pem: required_file("STEWARD_OPENSHELL_CLIENT_CERTIFICATE_FILE")?,
        client_private_key_pem: required_file("STEWARD_OPENSHELL_CLIENT_PRIVATE_KEY_FILE")?,
        workload_exchange_endpoint: required("STEWARD_WORKLOAD_EXCHANGE_ENDPOINT")?,
        workload_exchange_server_name: required("STEWARD_WORKLOAD_EXCHANGE_SERVER_NAME")?,
        workload_exchange_ca_certificate_pem: required_file(
            "STEWARD_WORKLOAD_EXCHANGE_CA_CERTIFICATE_FILE",
        )?,
        workload_source_credential_file: required_path("STEWARD_WORKLOAD_SOURCE_CREDENTIAL_FILE")?,
        server_name: required("STEWARD_OPENSHELL_SERVER_NAME")?,
        runtime_class_name: "kata-qemu".to_owned(),
    })
}

fn run(command: &mut Command, description: &str) -> Result<(), String> {
    let output = command
        .output()
        .map_err(|error| format!("failed to start {description}: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{description} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn make_input_archive(run_dir: &Path) -> Result<(Vec<u8>, Vec<u8>), String> {
    let input_root = run_dir.join("adapter-input");
    let input_path = input_root.join("in/payload.bin");
    fs::create_dir_all(
        input_path
            .parent()
            .ok_or_else(|| "input fixture has no parent directory".to_owned())?,
    )
    .map_err(|error| format!("failed to create input fixture directory: {error}"))?;
    let payload = b"steward-openshell-v0098\n".to_vec();
    fs::write(&input_path, &payload)
        .map_err(|error| format!("failed to write input fixture: {error}"))?;
    let archive_path = run_dir.join("adapter-input.tar");
    run(
        Command::new("tar")
            .arg("-cf")
            .arg(&archive_path)
            .arg("-C")
            .arg(&input_root)
            .arg("in/payload.bin"),
        "input archive creation",
    )?;
    let archive = fs::read(&archive_path)
        .map_err(|error| format!("failed to read input archive: {error}"))?;
    Ok((archive, payload))
}

fn output_payload(run_dir: &Path, archive: &[u8]) -> Result<Vec<u8>, String> {
    let archive_path = run_dir.join("adapter-output.tar");
    let output_root = run_dir.join("adapter-output");
    fs::write(&archive_path, archive)
        .map_err(|error| format!("failed to write output archive: {error}"))?;
    fs::create_dir_all(&output_root)
        .map_err(|error| format!("failed to create output directory: {error}"))?;
    run(
        Command::new("tar")
            .arg("-xf")
            .arg(&archive_path)
            .arg("-C")
            .arg(&output_root),
        "output archive extraction",
    )?;
    fs::read(output_root.join("out/payload.bin"))
        .map_err(|error| format!("declared output out/payload.bin is missing: {error}"))
}

fn assert_kata_runtime(workspace: &str, sandbox: &str) -> Result<(), String> {
    let kubeconfig = required("STEWARD_TEST_KUBECONFIG")?;
    let context = required("STEWARD_TEST_KUBE_CONTEXT")?;
    let selector =
        format!("openshell.ai/sandbox-workspace={workspace},openshell.ai/sandbox-name={sandbox}");
    let output = Command::new("kubectl")
        .args([
            "--kubeconfig",
            &kubeconfig,
            "--context",
            &context,
            "-n",
            "openshell",
            "get",
            "sandboxes.agents.x-k8s.io",
            "--selector",
            &selector,
            "-o",
            "jsonpath={.items[0].spec.podTemplate.spec.runtimeClassName}",
        ])
        .output()
        .map_err(|error| format!("failed to inspect sandbox runtime class: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "sandbox runtime-class lookup failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let runtime_class = String::from_utf8(output.stdout)
        .map_err(|_| "sandbox runtime class was not UTF-8".to_owned())?;
    if runtime_class.trim() != "kata-qemu" {
        return Err(format!(
            "OpenShell created runtime class {:?}, expected kata-qemu",
            runtime_class.trim()
        ));
    }
    Ok(())
}

async fn wait_running(
    runtime: &OpenShellRuntime,
    request: &SandboxRequest,
) -> Result<RuntimeRefs, String> {
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        match runtime
            .ensure(request)
            .await
            .map_err(|error| format!("OpenShell ensure failed: {error:?}"))?
        {
            SandboxObservation::Running { refs } => return Ok(refs),
            SandboxObservation::Provisioning { .. } => {}
            SandboxObservation::Absent => {
                return Err("OpenShell returned Absent while ensuring a sandbox".to_owned());
            }
        }
        if Instant::now() >= deadline {
            return Err("OpenShell sandbox did not become Ready within 600 seconds".to_owned());
        }
        sleep(Duration::from_secs(2)).await;
    }
}

async fn delete_sandbox(
    runtime: &OpenShellRuntime,
    request: &SandboxRequest,
) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        match runtime
            .delete(request)
            .await
            .map_err(|error| format!("OpenShell delete failed: {error:?}"))?
        {
            SandboxObservation::Absent => return Ok(()),
            SandboxObservation::Provisioning { .. } | SandboxObservation::Running { .. } => {}
        }
        if Instant::now() >= deadline {
            return Err(
                "OpenShell sandbox deletion did not complete within 300 seconds".to_owned(),
            );
        }
        sleep(Duration::from_secs(2)).await;
    }
}

#[tokio::test]
async fn adapter_round_trip_is_authenticated_kata_bound_and_cleanup_safe() -> Result<(), String> {
    if required("STEWARD_OPEN_SHELL_RELEASE")? != EXPECTED_RELEASE {
        return Err(format!(
            "adapter integration requires OpenShell {EXPECTED_RELEASE}"
        ));
    }
    let config = valid_config()?;

    let mut unauthenticated = config.clone();
    unauthenticated.workload_source_credential_file =
        required_path("STEWARD_WORKLOAD_INVALID_SOURCE_CREDENTIAL_FILE")?;
    assert!(
        OpenShellRuntime::connect(unauthenticated).await.is_err(),
        "an invalid workload source credential must fail closed at exchange"
    );

    let mut untrusted = config.clone();
    untrusted.ca_certificate_pem = required_file("STEWARD_OPENSHELL_UNTRUSTED_CA_FILE")?;
    assert!(
        OpenShellRuntime::connect(untrusted).await.is_err(),
        "an untrusted OpenShell gateway CA must fail closed"
    );

    let mut wrong_server = config.clone();
    wrong_server.server_name = "wrong.example.test".to_owned();
    assert!(
        OpenShellRuntime::connect(wrong_server).await.is_err(),
        "a mismatched OpenShell TLS server name must fail closed"
    );

    let mut non_kata = config.clone();
    non_kata.runtime_class_name = "runc".to_owned();
    assert!(
        OpenShellRuntime::connect(non_kata).await.is_err(),
        "a non-Kata runtime contract must fail closed"
    );

    let runtime = OpenShellRuntime::connect(config)
        .await
        .map_err(|error| format!("authenticated OpenShell connection failed: {error:?}"))?;
    let mut request = SandboxRequest {
        runtime: RuntimeId("runtime-adapter-v0098".to_owned()),
        workspace_key: "team-a".to_owned(),
        agent_type: AgentType {
            name: "base".to_owned(),
        },
        models: Vec::new(),
        tools: Vec::new(),
        refs: RuntimeRefs::default(),
    };
    let refs = wait_running(&runtime, &request).await?;

    let workspace = refs
        .workspace
        .as_deref()
        .ok_or_else(|| "running sandbox has no workspace reference".to_owned())?;
    let sandbox = refs
        .sandbox
        .as_deref()
        .ok_or_else(|| "running sandbox has no sandbox reference".to_owned())?;
    assert_kata_runtime(workspace, sandbox)?;
    request.refs = refs.clone();

    let run_dir = PathBuf::from(required("STEWARD_RUN_DIR")?);
    let (input_archive, expected_payload) = make_input_archive(&run_dir)?;
    let task_result = runtime
        .run_task(
            &SandboxTaskRequest {
                runtime: request.runtime.clone(),
                refs,
                command: vec![
                    "/bin/sh".to_owned(),
                    "-c".to_owned(),
                    "set -eu; mkdir -p \"$STEWARD_OUTPUT_DIR/out\"; cp in/payload.bin \"$STEWARD_OUTPUT_DIR/out/payload.bin\"".to_owned(),
                ],
            },
            &input_archive,
        )
        .await
        .map_err(|error| format!("adapter task round trip failed: {error:?}"))
        .and_then(|output| output_payload(&run_dir, &output.archive));

    let cleanup_result = delete_sandbox(&runtime, &request).await;
    let actual_payload = task_result?;
    cleanup_result?;
    assert_eq!(
        Sha256::digest(&actual_payload),
        Sha256::digest(&expected_payload),
        "copied output must have the same SHA-256 as the uploaded input"
    );
    Ok(())
}
