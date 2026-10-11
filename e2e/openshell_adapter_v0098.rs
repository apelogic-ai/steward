use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use steward_adapter_openshell::{
    OpenShellConnectionConfig, OpenShellRuntime, OpenShellTaskLogMode,
};
use steward_ports::{
    SandboxExecutionClass, SandboxObservation, SandboxRequest, SandboxRuntime,
    SandboxTaskObservation, SandboxTaskRequest, SandboxTaskRuntime, TaskAttemptId,
};
use steward_types::direct_package::{
    ContentDigest, DiagnosticsRequest, ExactGitCommit, ExecutionLogMode,
    MAX_EXECUTION_STREAM_BYTES, RepositoryUrl, ResolvedWorkspaceEntry, StableProviderId,
    WorkspaceAccess, WorkspaceEvidence, WorkspaceGitHistory, WorkspaceName, canonical_json_bytes,
};
use steward_types::task_input_archive::frame_task_input_archive;
use steward_types::task_output_archive::{
    TaskOutputArchiveCompatibility, task_output_archive_entries,
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
        runtime_class_name: env::var("STEWARD_OPENSHELL_RUNTIME_CLASS_NAME").unwrap_or_default(),
        task_log_mode: OpenShellTaskLogMode::Off,
        stable_bridge_image: None,
        stable_bridge_gateway_origin: None,
        bridge_image: None,
        bridge_artifact_trust_mode: None,
        bridge_gateway_origin: None,
        bridge_gateway_version: None,
        bridge_runtime_namespace: None,
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
    // OpenShell 0.0.98 caps each decoded gRPC message at 1 MiB. Keep this
    // fixture above that boundary so the adapter proves that task archives
    // are streamed instead of embedded in one ExecSandbox request.
    let payload = vec![b'x'; 1_100_000];
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

fn command_stdout(command: &mut Command, description: &str) -> Result<Vec<u8>, String> {
    let output = command
        .output()
        .map_err(|error| format!("failed to start {description}: {error}"))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(format!(
            "{description} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn make_workspace_archive(
    run_dir: &Path,
    caller_archive: &[u8],
) -> Result<(Vec<u8>, WorkspaceEvidence), String> {
    let repository = run_dir.join("workspace-git-fixture");
    fs::create_dir_all(&repository)
        .map_err(|error| format!("failed to create workspace Git fixture: {error}"))?;
    run(
        Command::new("git")
            .args(["init", "--initial-branch=main"])
            .arg(&repository),
        "workspace Git fixture initialization",
    )?;
    run(
        Command::new("git").args(["-C"]).arg(&repository).args([
            "config",
            "user.email",
            "alice@example.com",
        ]),
        "workspace Git author email configuration",
    )?;
    run(
        Command::new("git")
            .args(["-C"])
            .arg(&repository)
            .args(["config", "user.name", "alice"]),
        "workspace Git author configuration",
    )?;
    fs::write(repository.join("tracked.txt"), "first line\n")
        .map_err(|error| format!("failed to write first workspace revision: {error}"))?;
    run(
        Command::new("git")
            .args(["-C"])
            .arg(&repository)
            .args(["add", "tracked.txt"]),
        "workspace Git first add",
    )?;
    run(
        Command::new("git")
            .args(["-C"])
            .arg(&repository)
            .args(["commit", "-m", "first revision"]),
        "workspace Git first commit",
    )?;
    fs::write(repository.join("tracked.txt"), "first line\nsecond line\n")
        .map_err(|error| format!("failed to write second workspace revision: {error}"))?;
    run(
        Command::new("git").args(["-C"]).arg(&repository).args([
            "commit",
            "-am",
            "second revision",
        ]),
        "workspace Git second commit",
    )?;
    let head = String::from_utf8(command_stdout(
        Command::new("git")
            .args(["-C"])
            .arg(&repository)
            .args(["rev-parse", "HEAD"]),
        "workspace Git HEAD resolution",
    )?)
    .map_err(|_| "workspace Git HEAD was not UTF-8".to_owned())?;
    let commit = ExactGitCommit::parse(format!("git:sha1:{}", head.trim()))?;
    let mut pack = Command::new("git")
        .args(["-C"])
        .arg(&repository)
        .args(["pack-objects", "--stdout", "--revs"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to start workspace Git pack creation: {error}"))?;
    pack.stdin
        .take()
        .ok_or_else(|| "workspace Git pack stdin was unavailable".to_owned())?
        .write_all(format!("{}\n", head.trim()).as_bytes())
        .map_err(|error| format!("failed to select workspace Git pack commit: {error}"))?;
    let pack = pack
        .wait_with_output()
        .map_err(|error| format!("failed to wait for workspace Git pack: {error}"))?;
    if !pack.status.success() {
        return Err(format!(
            "workspace Git pack creation failed: {}",
            String::from_utf8_lossy(&pack.stderr).trim()
        ));
    }

    let content_digest = ContentDigest::parse(format!("steward:sha256:{}", "a".repeat(64)))?;
    let entries = vec![ResolvedWorkspaceEntry::Git {
        name: WorkspaceName::parse("source")?,
        access: WorkspaceAccess::Copy,
        repository: RepositoryUrl::parse("https://github.com/example-org/source.git")?,
        repository_id: StableProviderId::parse("123456")?,
        repository_owner_id: StableProviderId::parse("7890")?,
        commit,
        history: WorkspaceGitHistory::Depth(2),
        paths: Vec::new(),
        submodules: Vec::new(),
        max_materialized_bytes: 62_914_560,
        content_digest,
    }];
    let max_materialized_bytes = 62_914_560_u64;
    let digest = Sha256::digest(canonical_json_bytes(&serde_json::json!({
        "entries": &entries,
        "maxMaterializedBytes": max_materialized_bytes,
    }))?);
    let evidence = WorkspaceEvidence {
        entries,
        max_materialized_bytes,
        workspace_digest: ContentDigest::parse(format!("steward:sha256:{digest:x}"))?,
    };
    evidence.validate()?;

    let source = run_dir.join("workspace-archive-source");
    fs::create_dir_all(source.join("packs"))
        .map_err(|error| format!("failed to create workspace archive source: {error}"))?;
    fs::write(
        source.join("manifest.json"),
        canonical_json_bytes(&evidence)?,
    )
    .map_err(|error| format!("failed to write workspace manifest: {error}"))?;
    fs::write(source.join("packs/0.pack"), pack.stdout)
        .map_err(|error| format!("failed to write workspace Git pack: {error}"))?;
    let archive_path = run_dir.join("workspace.tar");
    run(
        Command::new("tar")
            .arg("-cf")
            .arg(&archive_path)
            .arg("-C")
            .arg(&source)
            .args(["manifest.json", "packs/0.pack"]),
        "workspace archive creation",
    )?;
    let workspace_archive = fs::read(archive_path)
        .map_err(|error| format!("failed to read workspace archive: {error}"))?;
    let framed = frame_task_input_archive(caller_archive, &workspace_archive)
        .map_err(|error| format!("failed to frame workspace archive: {error:?}"))?;
    Ok((framed, evidence))
}

fn output_payload(run_dir: &Path, archive: &[u8]) -> Result<Vec<u8>, String> {
    let archive_path = run_dir.join("adapter-output.tar");
    let output_root = run_dir.join("adapter-output");
    fs::write(&archive_path, archive)
        .map_err(|error| format!("failed to write output archive: {error}"))?;
    fs::create_dir_all(&output_root)
        .map_err(|error| format!("failed to create output directory: {error}"))?;
    let entries = task_output_archive_entries(archive, TaskOutputArchiveCompatibility::Strict)
        .map_err(|_| "adapter returned an archive outside the out/-only contract".to_owned())?;
    if entries.len() != 1 || entries[0].path != "payload.bin" {
        return Err(format!(
            "adapter returned unexpected output entries: {:?}",
            entries.iter().map(|entry| &entry.path).collect::<Vec<_>>()
        ));
    }
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

fn assert_default_runtime_class(workspace: &str, sandbox: &str) -> Result<(), String> {
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
    if !runtime_class.trim().is_empty() {
        return Err(format!(
            "OpenShell created explicit runtime class {:?}, expected the cluster default",
            runtime_class.trim(),
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

async fn verify_attempt_failure_semantics(
    runtime: &OpenShellRuntime,
    sandbox: &SandboxRequest,
    input_archive: &[u8],
) -> Result<(), String> {
    let task = SandboxTaskRequest {
        runtime: sandbox.runtime.clone(),
        refs: sandbox.refs.clone(),
        execution_class: SandboxExecutionClass::Agent,
        agent_type: sandbox.agent_type.clone(),
        command: vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "mkdir -p \"$STEWARD_OUTPUT_DIR/out\"; sleep 60".to_owned(),
        ],
        diagnostics: DiagnosticsRequest {
            execution_log: ExecutionLogMode::Full,
        },
        workspace: None,
        execution_binding: None,
    };
    let attempt = TaskAttemptId("00000000-0000-4000-8000-000000000002".to_owned());
    let started = runtime
        .start_task(&attempt, &task, input_archive)
        .await
        .map_err(|error| format!("detached cancellation task failed to start: {error:?}"))?;
    if !matches!(
        started,
        SandboxTaskObservation::Accepted { .. }
            | SandboxTaskObservation::Running { .. }
            | SandboxTaskObservation::RunningWithTranscript { .. }
    ) {
        return Err(format!(
            "detached cancellation task was not live after launch: {started:?}"
        ));
    }
    let cancelled = runtime
        .cancel_task(&attempt, &task)
        .await
        .map_err(|error| format!("attempt-scoped cancellation failed: {error:?}"))?;
    if !matches!(cancelled, SandboxTaskObservation::OutcomeUnknown { .. }) {
        return Err(format!(
            "attempt-scoped cancellation did not terminalize the process group: {cancelled:?}"
        ));
    }
    sleep(Duration::from_secs(2)).await;
    if !matches!(
        runtime.observe_task(&attempt, &task).await,
        Ok(SandboxTaskObservation::OutcomeUnknown { .. })
    ) {
        return Err("cancelled process group became live again".to_owned());
    }

    let oversized_attempt = TaskAttemptId("00000000-0000-4000-8000-000000000003".to_owned());
    let oversized_task = SandboxTaskRequest {
        command: vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            format!(
                "yes x | head -c {}; mkdir -p \"$STEWARD_OUTPUT_DIR/out\"",
                MAX_EXECUTION_STREAM_BYTES + 1
            ),
        ],
        ..task.clone()
    };
    let mut oversized = runtime
        .start_task(&oversized_attempt, &oversized_task, input_archive)
        .await
        .map_err(|error| format!("oversized transcript task failed to start: {error:?}"))?;
    let oversized_deadline = Instant::now() + Duration::from_secs(120);
    loop {
        match oversized {
            SandboxTaskObservation::Accepted { .. }
            | SandboxTaskObservation::Running { .. }
            | SandboxTaskObservation::RunningWithTranscript { .. } => {}
            SandboxTaskObservation::FailedWithTranscript {
                reason, transcript, ..
            } => {
                if reason != "Task execution transcript exceeded its bounded archive contract" {
                    return Err(format!(
                        "oversized transcript had unsafe reason: {reason:?}"
                    ));
                }
                if transcript.stdout.len()
                    != usize::try_from(MAX_EXECUTION_STREAM_BYTES)
                        .map_err(|_| "execution stream bound does not fit usize".to_owned())?
                    || !transcript
                        .stdout
                        .starts_with(b"[... execution output truncated ...]\n")
                    || !transcript.stderr.is_empty()
                {
                    return Err(
                        "oversized transcript was not returned as a bounded tail".to_owned()
                    );
                }
                break;
            }
            other => {
                return Err(format!(
                    "oversized transcript did not terminalize as a bounded failure: {other:?}"
                ));
            }
        }
        if Instant::now() >= oversized_deadline {
            return Err("oversized transcript did not terminalize within 120 seconds".to_owned());
        }
        sleep(Duration::from_secs(1)).await;
        oversized = runtime
            .observe_task(&oversized_attempt, &oversized_task)
            .await
            .map_err(|error| format!("oversized transcript observation failed: {error:?}"))?;
    }

    let launch_failure_attempt = TaskAttemptId("00000000-0000-4000-8000-000000000004".to_owned());
    let launch_failure_task = SandboxTaskRequest {
        command: Vec::new(),
        ..task.clone()
    };
    if runtime
        .start_task(&launch_failure_attempt, &launch_failure_task, input_archive)
        .await
        .is_ok()
    {
        return Err("a detached wrapper syntax failure was reported as accepted".to_owned());
    }

    let synchronous_attempt = TaskAttemptId("00000000-0000-4000-8000-000000000005".to_owned());
    let synchronous_task = SandboxTaskRequest {
        command: vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "mkdir -p \"$STEWARD_OUTPUT_DIR/out\"; printf sync > \"$STEWARD_OUTPUT_DIR/out/result.txt\""
                .to_owned(),
        ],
        diagnostics: DiagnosticsRequest {
            execution_log: ExecutionLogMode::Off,
        },
        ..task
    };
    if !matches!(
        runtime
            .start_task(&synchronous_attempt, &synchronous_task, input_archive)
            .await,
        Ok(SandboxTaskObservation::Succeeded { .. })
    ) {
        return Err(
            "executionLog=off did not retain synchronous terminal start behavior".to_owned(),
        );
    }
    Ok(())
}

#[tokio::test]
async fn adapter_round_trip_is_authenticated_with_default_runtime_and_cleanup() -> Result<(), String>
{
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

    let mut invalid_runtime_class = config.clone();
    invalid_runtime_class.runtime_class_name = "invalid/runtime".to_owned();
    assert!(
        OpenShellRuntime::connect(invalid_runtime_class)
            .await
            .is_err(),
        "an invalid Kubernetes RuntimeClass contract must fail closed"
    );

    let runtime = OpenShellRuntime::connect(config.clone())
        .await
        .map_err(|error| format!("authenticated OpenShell connection failed: {error:?}"))?;
    let mut request = SandboxRequest {
        runtime: RuntimeId("runtime-adapter-v0098".to_owned()),
        workspace_key: "team-a".to_owned(),
        execution_class: SandboxExecutionClass::Agent,
        agent_type: AgentType {
            name: "base".to_owned(),
        },
        models: Vec::new(),
        tools: Vec::new(),
        refs: RuntimeRefs::default(),
        execution_binding: None,
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
    assert_default_runtime_class(workspace, sandbox)?;
    request.refs = refs.clone();

    let run_dir = PathBuf::from(required("STEWARD_RUN_DIR")?);
    let (caller_archive, expected_payload) = make_input_archive(&run_dir)?;
    let (input_archive, workspace) = make_workspace_archive(&run_dir, &caller_archive)?;
    let attempt_id = TaskAttemptId("00000000-0000-4000-8000-000000000001".to_owned());
    let task = SandboxTaskRequest {
        runtime: request.runtime.clone(),
        refs,
        execution_class: SandboxExecutionClass::Agent,
        agent_type: request.agent_type.clone(),
        command: vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "set -eu; repository=/sandbox/workspace/source; test \"$(git -C \"$repository\" log --format=%s -2)\" = 'second revision\nfirst revision'; git -C \"$repository\" blame --porcelain tracked.txt | grep -Fq 'summary first revision'; test -z \"$(git -C \"$repository\" remote)\"; ! git -C \"$repository\" push >/dev/null 2>&1; mkdir -p \"$STEWARD_OUTPUT_DIR/out\"; printf task-; printf task- >&2; sleep 5; printf stdout; printf stderr >&2; sleep 5; cp in/payload.bin \"$STEWARD_OUTPUT_DIR/out/payload.bin\"".to_owned(),
        ],
        diagnostics: DiagnosticsRequest {
            execution_log: ExecutionLogMode::Full,
        },
        workspace: Some(workspace),
        execution_binding: None,
    };
    let task_result = async {
        let mut observation = runtime
            .start_task(&attempt_id, &task, &input_archive)
            .await
            .map_err(|error| format!("adapter task round trip failed: {error:?}"))?;
        let observing_runtime = OpenShellRuntime::connect(config.clone())
            .await
            .map_err(|error| format!("adapter reconnect during task failed: {error:?}"))?;
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut last_live_transcript: Option<(Vec<u8>, Vec<u8>)> = None;
        let mut live_transcript_grew = false;
        loop {
            match observation {
                SandboxTaskObservation::Accepted { .. } | SandboxTaskObservation::Running { .. } => {}
                SandboxTaskObservation::RunningWithTranscript { transcript, .. } => {
                    if let Some((previous_stdout, previous_stderr)) = &last_live_transcript {
                        if transcript.stdout.starts_with(previous_stdout)
                            && transcript.stderr.starts_with(previous_stderr)
                            && (transcript.stdout.len() > previous_stdout.len()
                                || transcript.stderr.len() > previous_stderr.len())
                        {
                            live_transcript_grew = true;
                        }
                    }
                    last_live_transcript = Some((transcript.stdout, transcript.stderr));
                }
                SandboxTaskObservation::SucceededWithTranscript {
                    output,
                    transcript,
                    ..
                } if transcript.stdout == b"task-stdout"
                    && transcript.stderr == b"task-stderr"
                    && live_transcript_grew =>
                {
                    break output_payload(&run_dir, &output.archive);
                }
                other => {
                    break Err(format!(
                        "adapter task round trip did not grow a live transcript before retaining its terminal transcript: {other:?}"
                    ));
                }
            }
            if Instant::now() >= deadline {
                break Err("adapter task round trip did not finish within 120 seconds".to_owned());
            }
            sleep(Duration::from_secs(1)).await;
            observation = observing_runtime
                .observe_task(&attempt_id, &task)
                .await
                .map_err(|error| format!("adapter task observation failed: {error:?}"))?;
        }
    }
    .await;

    let failure_semantics = if task_result.is_ok() {
        verify_attempt_failure_semantics(&runtime, &request, &caller_archive).await
    } else {
        Ok(())
    };
    let cleanup_result = delete_sandbox(&runtime, &request).await;
    let actual_payload = task_result?;
    failure_semantics?;
    cleanup_result?;
    assert_eq!(
        Sha256::digest(&actual_payload),
        Sha256::digest(&expected_payload),
        "copied output must have the same SHA-256 as the uploaded input"
    );
    Ok(())
}
