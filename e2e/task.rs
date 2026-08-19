use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use kube::api::{Api, ListParams};
use steward_store::{AgentRunTimelineKind, PgStore};
use steward_types::{AgentRuntime, Phase, Principal, TaskPhase};

const HTTP_STATUS_MARKER: &str = "__STEWARD_HTTP_STATUS:";

struct HttpResponse {
    body: String,
    status: u16,
}

fn retryable_provider_grant_status(status: u16) -> bool {
    status == 502
}

fn parse_http_response(output: Output, context: &str) -> Result<HttpResponse, Box<dyn Error>> {
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "{context} did not complete: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
        .into());
    }
    let stdout = String::from_utf8(output.stdout)?;
    let (body, status) = stdout.rsplit_once(HTTP_STATUS_MARKER).ok_or_else(|| {
        io::Error::other(format!("{context} did not return an HTTP status marker"))
    })?;
    let status = status.trim().parse::<u16>().map_err(|error| {
        io::Error::other(format!(
            "{context} returned an invalid HTTP status: {error}"
        ))
    })?;
    Ok(HttpResponse {
        body: body.trim_end_matches('\n').to_owned(),
        status,
    })
}

#[test]
fn only_the_transparent_provider_grant_failure_is_retried() {
    assert!(
        retryable_provider_grant_status(502),
        "the OpenShell transparent relay uses 502 for an in-progress dynamic provider grant"
    );
    for status in [200, 400, 401, 403, 404, 500, 503] {
        assert!(
            !retryable_provider_grant_status(status),
            "status {status} must fail closed rather than hide a non-grant failure"
        );
    }
}

#[test]
fn http_probe_retains_the_safe_body_and_status() -> Result<(), Box<dyn Error>> {
    let response = parse_http_response(
        Output {
            status: success_status()?,
            stdout: b"{\"error\":\"grant pending\"}\n__STEWARD_HTTP_STATUS:502\n".to_vec(),
            stderr: Vec::new(),
        },
        "provider probe",
    )?;
    assert_eq!(response.status, 502);
    assert_eq!(response.body, r#"{"error":"grant pending"}"#);
    Ok(())
}

#[test]
fn task_client_selects_a_rustls_crypto_provider() -> Result<(), String> {
    install_rustls_crypto_provider().map_err(|error| error.to_string())?;
    assert!(
        tokio_rustls::rustls::crypto::CryptoProvider::get_default().is_some(),
        "the Task client must select Rustls cryptography before it constructs Kubernetes clients"
    );
    Ok(())
}

#[cfg(unix)]
fn success_status() -> Result<std::process::ExitStatus, Box<dyn Error>> {
    Ok(std::os::unix::process::ExitStatusExt::from_raw(0))
}

#[tokio::test]
async fn e2e_controller_owned_task_runtime_lifecycle() -> Result<(), Box<dyn Error>> {
    install_rustls_crypto_provider()?;
    let base_url = required("STEWARD_TASK_URL")?;
    let runtime_api = Api::<AgentRuntime>::all(kube::Client::try_default().await?);
    let store = PgStore::connect(&required("STEWARD_TEST_DATABASE_URL")?).await?;
    let run_dir = PathBuf::from(required("STEWARD_RUN_DIR")?).join("task-client");
    fs::create_dir_all(run_dir.join("input/in"))?;
    fs::write(
        run_dir.join("input/in/payload.bin"),
        b"governed task payload\n",
    )?;
    let input_tar = run_dir.join("input.tar");
    command(
        Command::new("tar")
            .args(["-cf"])
            .arg(&input_tar)
            .args(["-C"])
            .arg(run_dir.join("input"))
            .arg("."),
        "create task input tar",
    )?;

    let copy_submission = submit_response(
        &base_url,
        "github-assertion",
        "copy-smoke-123",
        r#"{"workflow":"copy-smoke","codingAgentRuntime":"base"}"#,
        &run_dir,
    )?;
    assert_eq!(
        copy_submission.http_status, 202,
        "an admitted Task must acknowledge durable acceptance before the controller creates its runtime"
    );
    let copy_smoke = copy_submission.body;
    assert_eq!(copy_smoke["phase"], "submitted");
    assert!(
        copy_smoke["runtimeUid"].is_null(),
        "the Task API must not bind a runtime UID before controller-owned creation"
    );
    let copy_task_uid = json_string(&copy_smoke, "taskUid")?;
    let copy_bound =
        wait_for_runtime_binding(&base_url, copy_task_uid, "github-assertion", &run_dir)?;
    let copy_runtime_uid = json_string(&copy_bound, "runtimeUid")?;
    put_archive(
        &base_url,
        copy_task_uid,
        "github-assertion",
        &input_tar,
        &run_dir,
    )?;
    execute(&base_url, copy_task_uid, "github-assertion", &run_dir)?;
    wait_for(
        &base_url,
        copy_task_uid,
        "github-assertion",
        |status| status["phase"] == "succeeded",
        &run_dir,
    )?;
    let copy_output_tar = run_dir.join("copy-output.tar");
    get_output(
        &base_url,
        copy_task_uid,
        "github-assertion",
        &copy_output_tar,
        &run_dir,
    )?;
    let copy_output_dir = run_dir.join("copy-output");
    fs::create_dir_all(&copy_output_dir)?;
    command(
        Command::new("tar")
            .args(["-xf"])
            .arg(&copy_output_tar)
            .args(["-C"])
            .arg(&copy_output_dir),
        "extract copy-smoke output tar",
    )?;
    assert_eq!(
        fs::read(copy_output_dir.join("out/payload.bin"))?,
        b"governed task payload\n",
        "copy-smoke must preserve the input bytes at the declared output path"
    );
    assert!(
        runtime_by_uid(&runtime_api, copy_runtime_uid)
            .await?
            .is_some(),
        "a successful provisioned Task must retain its exact runtime until finalization is requested"
    );
    delete_task(&base_url, copy_task_uid, "github-assertion", &run_dir)?;
    wait_for(
        &base_url,
        copy_task_uid,
        "github-assertion",
        |status| status["finalized"] == true,
        &run_dir,
    )?;
    assert!(
        runtime_by_uid(&runtime_api, copy_runtime_uid)
            .await?
            .is_none(),
        "finalizing a successful provisioned Task must delete its exact runtime"
    );
    let copy_timeline = store
        .agent_run_timeline(copy_task_uid.parse()?)
        .await?
        .ok_or_else(|| io::Error::other("successful Task lifecycle timeline was not persisted"))?;
    assert_eq!(
        copy_timeline
            .iter()
            .map(|event| event.kind)
            .collect::<Vec<_>>(),
        vec![
            AgentRunTimelineKind::Phase(TaskPhase::Submitted),
            AgentRunTimelineKind::Phase(TaskPhase::Queued),
            AgentRunTimelineKind::Phase(TaskPhase::Running),
            AgentRunTimelineKind::Phase(TaskPhase::Succeeded),
            AgentRunTimelineKind::FinalizationRequested,
            AgentRunTimelineKind::Finalized,
        ],
        "the durable Task timeline must evidence submit, execute, output persistence, finalization request, and exact runtime cleanup"
    );

    let stale = submit(
        &base_url,
        "github-assertion",
        "stale-runtime-123",
        r#"{"workflow":"code-review","codingAgentRuntime":"base"}"#,
        &run_dir,
    )?;
    let stale_task_uid = json_string(&stale, "taskUid")?;
    let stale_bound =
        wait_for_runtime_binding(&base_url, stale_task_uid, "github-assertion", &run_dir)?;
    let stale_runtime_uid = json_string(&stale_bound, "runtimeUid")?;
    let replacement_runtime_uid =
        replace_runtime_for_stale_uid_test(&base_url, stale_runtime_uid, &run_dir)?;
    assert_ne!(
        replacement_runtime_uid, stale_runtime_uid,
        "the stale-runtime control must create a distinct UID under the same runtime name"
    );
    put_archive(
        &base_url,
        stale_task_uid,
        "github-assertion",
        &input_tar,
        &run_dir,
    )?;
    execute(&base_url, stale_task_uid, "github-assertion", &run_dir)?;
    assert_phase_stays(
        &base_url,
        stale_task_uid,
        "github-assertion",
        "queued",
        &run_dir,
    )?;
    delete_task(&base_url, stale_task_uid, "github-assertion", &run_dir)?;
    wait_for(
        &base_url,
        stale_task_uid,
        "github-assertion",
        |status| status["finalized"] == true,
        &run_dir,
    )?;
    assert!(
        runtime_by_uid(&runtime_api, &replacement_runtime_uid)
            .await?
            .is_some(),
        "finalizing against a stale UID must not delete a same-name replacement runtime"
    );

    let diagnostic = submit(
        &base_url,
        "github-assertion",
        "provider-readiness-123",
        r#"{"workflow":"code-review","codingAgentRuntime":"base"}"#,
        &run_dir,
    )?;
    assert_eq!(diagnostic["phase"], "submitted");
    let diagnostic_task_uid = json_string(&diagnostic, "taskUid")?;
    let diagnostic_bound =
        wait_for_runtime_binding(&base_url, diagnostic_task_uid, "github-assertion", &run_dir)?;
    let diagnostic_runtime_uid = json_string(&diagnostic_bound, "runtimeUid")?;
    assert_controller_runtime_tool_ready(&runtime_api, diagnostic_runtime_uid).await?;
    delete_task(&base_url, diagnostic_task_uid, "github-assertion", &run_dir)?;
    wait_for(
        &base_url,
        diagnostic_task_uid,
        "github-assertion",
        |status| status["finalized"] == true,
        &run_dir,
    )?;

    let body = r#"{"workflow":"approval-review","codingAgentRuntime":"base"}"#;
    let parked = submit(
        &base_url,
        "github-assertion",
        "approval-job-123",
        body,
        &run_dir,
    )?;
    assert_eq!(parked["phase"], "parked");
    let task_uid = json_string(&parked, "taskUid")?;
    let runtime_uid = json_string(&parked, "runtimeUid")?;
    let retried = submit(
        &base_url,
        "github-assertion",
        "approval-job-123",
        body,
        &run_dir,
    )?;
    assert_eq!(retried["taskUid"], parked["taskUid"]);
    assert_eq!(retried["runtimeUid"], parked["runtimeUid"]);
    put_archive(
        &base_url,
        task_uid,
        "github-assertion",
        &input_tar,
        &run_dir,
    )?;
    execute(&base_url, task_uid, "github-assertion", &run_dir)?;
    execute(&base_url, task_uid, "github-assertion", &run_dir)?;

    let pending = store
        .pending_approvals()
        .await?
        .into_iter()
        .find(|approval| approval.runtime_uid == runtime_uid)
        .ok_or_else(|| io::Error::other("parked Task approval was not persisted"))?;
    let evidence_url = pending
        .evidence_url
        .as_deref()
        .ok_or_else(|| io::Error::other("parked Task approval was not filed"))?;
    let approval_id = pending.approval_id.to_string();
    approve(&base_url, &approval_id, evidence_url, &run_dir)?;
    assert_resolution_emitted(&base_url, &approval_id, &run_dir)?;

    let succeeded = wait_for(
        &base_url,
        task_uid,
        "github-assertion",
        |status| status["phase"] == "succeeded",
        &run_dir,
    )?;
    assert_eq!(succeeded["finalized"], false);
    let output_tar = run_dir.join("output.tar");
    get_output(
        &base_url,
        task_uid,
        "github-assertion",
        &output_tar,
        &run_dir,
    )?;
    let output_dir = run_dir.join("output");
    fs::create_dir_all(&output_dir)?;
    command(
        Command::new("tar")
            .args(["-xf"])
            .arg(&output_tar)
            .args(["-C"])
            .arg(&output_dir),
        "extract task output tar",
    )?;
    assert_eq!(
        fs::read(output_dir.join("out/payload.bin"))?,
        b"governed task payload\n"
    );
    assert!(
        fs::read_to_string(output_dir.join("out/tool.json"))?
            .contains("example-org/fixture-repository"),
        "the Task output must contain the governed tool result"
    );
    delete_task(&base_url, task_uid, "github-assertion", &run_dir)?;
    wait_for(
        &base_url,
        task_uid,
        "github-assertion",
        |status| status["finalized"] == true,
        &run_dir,
    )?;

    for (assertion, key) in [
        ("github-assertion", "github-stub-123"),
        ("slack-assertion", "slack-stub-123"),
        ("portal-assertion", "portal-stub-123"),
    ] {
        let submitted = submit(
            &base_url,
            assertion,
            key,
            r#"{"workflow":"code-review","codingAgentRuntime":"base"}"#,
            &run_dir,
        )?;
        assert_eq!(submitted["phase"], "submitted");
        let submitted_task_uid = json_string(&submitted, "taskUid")?;
        wait_for_runtime_binding(&base_url, submitted_task_uid, assertion, &run_dir)?;
        put_archive(
            &base_url,
            submitted_task_uid,
            assertion,
            &input_tar,
            &run_dir,
        )?;
        execute(&base_url, submitted_task_uid, assertion, &run_dir)?;
        wait_for(
            &base_url,
            submitted_task_uid,
            assertion,
            |status| status["phase"] == "succeeded",
            &run_dir,
        )?;
        delete_task(&base_url, submitted_task_uid, assertion, &run_dir)?;
        wait_for(
            &base_url,
            submitted_task_uid,
            assertion,
            |status| status["finalized"] == true,
            &run_dir,
        )?;
    }

    let scheduled = submit(
        &base_url,
        "scheduled-assertion",
        "scheduled-stub-123",
        r#"{"workflow":"code-review","codingAgentRuntime":"base"}"#,
        &run_dir,
    )?;
    let scheduled_task_uid = json_string(&scheduled, "taskUid")?;
    let scheduled_bound = wait_for_runtime_binding(
        &base_url,
        scheduled_task_uid,
        "scheduled-assertion",
        &run_dir,
    )?;
    let scheduled_runtime_uid = json_string(&scheduled_bound, "runtimeUid")?;
    let scheduled_runtime = runtime_by_uid(&runtime_api, scheduled_runtime_uid)
        .await?
        .ok_or_else(|| io::Error::other("scheduled Task runtime was not created"))?;
    assert_eq!(scheduled_runtime.spec.owner.0, "owner@example.com");
    assert_eq!(
        scheduled_runtime.spec.principal,
        Principal::Service {
            name: "scheduled-scanner".to_owned(),
            acting_user: None,
        }
    );
    put_archive(
        &base_url,
        scheduled_task_uid,
        "scheduled-assertion",
        &input_tar,
        &run_dir,
    )?;
    execute(
        &base_url,
        scheduled_task_uid,
        "scheduled-assertion",
        &run_dir,
    )?;
    wait_for(
        &base_url,
        scheduled_task_uid,
        "scheduled-assertion",
        |status| status["phase"] == "succeeded",
        &run_dir,
    )?;
    delete_task(
        &base_url,
        scheduled_task_uid,
        "scheduled-assertion",
        &run_dir,
    )?;
    wait_for(
        &base_url,
        scheduled_task_uid,
        "scheduled-assertion",
        |status| status["finalized"] == true,
        &run_dir,
    )?;

    let standing = submit(
        &base_url,
        "github-assertion",
        "standing-runtime-123",
        r#"{"workflow":"code-review","codingAgentRuntime":"base"}"#,
        &run_dir,
    )?;
    let standing_task_uid = json_string(&standing, "taskUid")?;
    let standing_bound =
        wait_for_runtime_binding(&base_url, standing_task_uid, "github-assertion", &run_dir)?;
    let standing_runtime_uid = json_string(&standing_bound, "runtimeUid")?;
    let adopted_body = format!(
        r#"{{"workflow":"code-review","codingAgentRuntime":"base","agentRuntimeUid":"{standing_runtime_uid}"}}"#
    );
    let adopted = submit(
        &base_url,
        "github-assertion",
        "adopted-runtime-123",
        &adopted_body,
        &run_dir,
    )?;
    assert_eq!(adopted["runtimeOwnership"], "adopted");
    let adopted_task_uid = json_string(&adopted, "taskUid")?;
    put_archive(
        &base_url,
        adopted_task_uid,
        "github-assertion",
        &input_tar,
        &run_dir,
    )?;
    execute(&base_url, adopted_task_uid, "github-assertion", &run_dir)?;
    wait_for(
        &base_url,
        adopted_task_uid,
        "github-assertion",
        |status| status["phase"] == "succeeded",
        &run_dir,
    )?;
    delete_task(&base_url, adopted_task_uid, "github-assertion", &run_dir)?;
    wait_for(
        &base_url,
        adopted_task_uid,
        "github-assertion",
        |status| status["finalized"] == true,
        &run_dir,
    )?;
    assert!(
        runtime_by_uid(&runtime_api, standing_runtime_uid)
            .await?
            .is_some()
    );
    delete_task(&base_url, standing_task_uid, "github-assertion", &run_dir)?;
    wait_for(
        &base_url,
        standing_task_uid,
        "github-assertion",
        |status| status["finalized"] == true,
        &run_dir,
    )?;

    let failing = submit(
        &base_url,
        "github-assertion",
        "failing-task-123",
        r#"{"workflow":"failing-review","codingAgentRuntime":"base"}"#,
        &run_dir,
    )?;
    let failing_task_uid = json_string(&failing, "taskUid")?;
    let failing_bound =
        wait_for_runtime_binding(&base_url, failing_task_uid, "github-assertion", &run_dir)?;
    let failing_runtime_uid = json_string(&failing_bound, "runtimeUid")?;
    put_archive(
        &base_url,
        failing_task_uid,
        "github-assertion",
        &input_tar,
        &run_dir,
    )?;
    execute(&base_url, failing_task_uid, "github-assertion", &run_dir)?;
    wait_for(
        &base_url,
        failing_task_uid,
        "github-assertion",
        |status| status["phase"] == "failed" && status["finalized"] == true,
        &run_dir,
    )?;
    assert!(
        runtime_by_uid(&runtime_api, failing_runtime_uid)
            .await?
            .is_none()
    );
    Ok(())
}

fn install_rustls_crypto_provider() -> Result<(), io::Error> {
    use tokio_rustls::rustls::crypto::{CryptoProvider, ring};

    if CryptoProvider::get_default().is_none() {
        let _ = ring::default_provider().install_default();
    }
    if CryptoProvider::get_default().is_some() {
        Ok(())
    } else {
        Err(io::Error::other("Rustls crypto provider is unavailable"))
    }
}

async fn runtime_by_uid(
    runtimes: &Api<AgentRuntime>,
    runtime_uid: &str,
) -> Result<Option<AgentRuntime>, kube::Error> {
    Ok(runtimes
        .list(&ListParams::default())
        .await?
        .items
        .into_iter()
        .find(|runtime| runtime.metadata.uid.as_deref() == Some(runtime_uid)))
}

struct TaskSubmission {
    http_status: u16,
    body: serde_json::Value,
}

async fn wait_for_running_runtime(
    runtimes: &Api<AgentRuntime>,
    runtime_uid: &str,
) -> Result<AgentRuntime, Box<dyn Error>> {
    for _attempt in 0..240 {
        if let Some(runtime) = runtime_by_uid(runtimes, runtime_uid).await?
            && runtime.status.as_ref().is_some_and(|status| {
                status.phase == Phase::Running
                    && status.refs.workspace.is_some()
                    && status.refs.sandbox.is_some()
            })
        {
            return Ok(runtime);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Err(io::Error::other(format!(
        "controller-created Task runtime {runtime_uid} did not reach Running with workspace and sandbox references"
    ))
    .into())
}

async fn assert_controller_runtime_tool_ready(
    runtimes: &Api<AgentRuntime>,
    runtime_uid: &str,
) -> Result<(), Box<dyn Error>> {
    let runtime = wait_for_running_runtime(runtimes, runtime_uid).await?;
    assert!(
        !runtime.spec.tools.is_empty(),
        "the controller-created Task runtime must request a governed tool provider"
    );
    let refs = runtime
        .status
        .as_ref()
        .map(|status| &status.refs)
        .ok_or_else(|| io::Error::other("running Task runtime has no status references"))?;
    let workspace = refs
        .workspace
        .as_deref()
        .ok_or_else(|| io::Error::other("running Task runtime has no workspace reference"))?;
    let sandbox = refs
        .sandbox
        .as_deref()
        .ok_or_else(|| io::Error::other("running Task runtime has no sandbox reference"))?;
    let openshell = required("STEWARD_OPENSHELL_CLI")?;
    let endpoint = required("STEWARD_OPENSHELL_ENDPOINT")?;
    let providers = Command::new(&openshell)
        .args(["--gateway-endpoint", &endpoint, "--workspace", workspace])
        .args(["sandbox", "provider", "list", sandbox])
        .output()?;
    if !providers.status.success() {
        return Err(io::Error::other(format!(
            "controller-created runtime provider inspection failed: {}",
            String::from_utf8_lossy(&providers.stderr).trim()
        ))
        .into());
    }
    assert!(
        String::from_utf8_lossy(&providers.stdout).contains("steward-mcp-gw"),
        "controller-created runtime must attach the governed tool provider before execution"
    );

    let deadline = Instant::now() + Duration::from_secs(60);
    let terminal_response = loop {
        let tool_call = Command::new(&openshell)
            .args(["--gateway-endpoint", &endpoint, "--workspace", workspace])
            .args(["sandbox", "exec", "--name", sandbox, "--no-tty", "--"])
            .args([
                "curl",
                "-sS",
                "--max-time",
                "20",
                "-w",
                "\n__STEWARD_HTTP_STATUS:%{http_code}\n",
                "-H",
                "Content-Type: application/json",
                "-H",
                "MCP-Protocol-Version: 2025-06-18",
                "-d",
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"search_repositories","arguments":{}}}"#,
                "http://hop1-capture-tools.steward-system.svc.cluster.local:8085/mcp",
            ])
            .output()?;
        let response = parse_http_response(tool_call, "exact OpenShell ExecSandbox tool call")?;
        if response.status == 200 && response.body.contains("example-org/fixture-repository") {
            return Ok(());
        }
        if !retryable_provider_grant_status(response.status) || Instant::now() >= deadline {
            break response;
        }
        std::thread::sleep(Duration::from_secs(1));
    };
    let diagnostic = provider_grant_diagnostic()?;
    Err(io::Error::other(format!(
        "controller-created runtime did not obtain a dynamic tool grant: status {}; body {}; captured Mint response: {diagnostic}",
        terminal_response.status, terminal_response.body
    ))
    .into())
}

fn provider_grant_diagnostic() -> Result<String, Box<dyn Error>> {
    let capture_url = required("STEWARD_TEST_CAPTURE_URL")?;
    let output = Command::new("curl")
        .args(["-fsS", "--max-time", "5"])
        .arg(format!("{capture_url}/token-grant"))
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "capture token-grant diagnostic failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
        .into());
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn submit(
    base_url: &str,
    assertion: &str,
    key: &str,
    body: &str,
    run_dir: &Path,
) -> Result<serde_json::Value, Box<dyn Error>> {
    Ok(submit_response(base_url, assertion, key, body, run_dir)?.body)
}

fn submit_response(
    base_url: &str,
    assertion: &str,
    key: &str,
    body: &str,
    run_dir: &Path,
) -> Result<TaskSubmission, Box<dyn Error>> {
    let response = run_dir.join(format!("submit-{key}.json"));
    let status = curl_status(
        Command::new("curl")
            .args(["-sS", "-o"])
            .arg(&response)
            .args([
                "-w",
                "%{http_code}",
                "-X",
                "POST",
                "-H",
                &format!("Authorization: Bearer {assertion}"),
                "-H",
                &format!("Idempotency-Key: {key}"),
                "-H",
                "Content-Type: application/json",
                "--data",
                body,
                &format!("{base_url}/v1/tasks"),
            ]),
        "submit Task",
    )?;
    if status != 201 && status != 202 {
        return Err(io::Error::other(format!(
            "Task submission returned {status}: {}",
            fs::read_to_string(response)?
        ))
        .into());
    }
    Ok(TaskSubmission {
        http_status: status,
        body: serde_json::from_slice(&fs::read(response)?)?,
    })
}

fn put_archive(
    base_url: &str,
    task_uid: &str,
    assertion: &str,
    archive: &Path,
    run_dir: &Path,
) -> Result<(), Box<dyn Error>> {
    let response = run_dir.join("put-inputs.json");
    let status = curl_status(
        Command::new("curl")
            .args(["-sS", "-o"])
            .arg(&response)
            .args([
                "-w",
                "%{http_code}",
                "-X",
                "PUT",
                "-H",
                &format!("Authorization: Bearer {assertion}"),
                "-H",
                "Content-Type: application/x-tar",
                "--data-binary",
            ])
            .arg(format!("@{}", archive.display()))
            .arg(format!("{base_url}/v1/tasks/{task_uid}/inputs")),
        "put Task inputs",
    )?;
    if status != 204 {
        return Err(io::Error::other(format!(
            "Task input returned {status}: {}",
            fs::read_to_string(response)?
        ))
        .into());
    }
    Ok(())
}

fn execute(
    base_url: &str,
    task_uid: &str,
    assertion: &str,
    run_dir: &Path,
) -> Result<(), Box<dyn Error>> {
    let response = run_dir.join("execute.json");
    let status = curl_status(
        Command::new("curl")
            .args(["-sS", "-o"])
            .arg(response)
            .args([
                "-w",
                "%{http_code}",
                "-X",
                "POST",
                "-H",
                &format!("Authorization: Bearer {assertion}"),
                &format!("{base_url}/v1/tasks/{task_uid}/execute"),
            ]),
        "execute Task",
    )?;
    if status != 202 {
        return Err(io::Error::other(format!("Task execute returned {status}")).into());
    }
    Ok(())
}

fn delete_task(
    base_url: &str,
    task_uid: &str,
    assertion: &str,
    run_dir: &Path,
) -> Result<(), Box<dyn Error>> {
    let response = run_dir.join(format!("delete-{task_uid}.json"));
    let status = curl_status(
        Command::new("curl")
            .args(["-sS", "-o"])
            .arg(response)
            .args([
                "-w",
                "%{http_code}",
                "-X",
                "DELETE",
                "-H",
                &format!("Authorization: Bearer {assertion}"),
                &format!("{base_url}/v1/tasks/{task_uid}"),
            ]),
        "delete Task",
    )?;
    if status != 202 {
        return Err(io::Error::other(format!("Task delete returned {status}")).into());
    }
    Ok(())
}

fn approve(
    base_url: &str,
    approval_id: &str,
    evidence_url: &str,
    run_dir: &Path,
) -> Result<(), Box<dyn Error>> {
    let response = run_dir.join(format!("approval-{approval_id}.json"));
    let body = serde_json::json!({
        "rationale": "task e2e approval",
        "evidenceUrl": evidence_url,
        "expiresAt": "2999-01-01T00:00:00Z"
    })
    .to_string();
    let status = curl_status(
        Command::new("curl")
            .args(["-sS", "-o"])
            .arg(response)
            .args([
                "-w",
                "%{http_code}",
                "-X",
                "POST",
                "-H",
                "Authorization: Bearer admin-assertion",
                "-H",
                "Content-Type: application/json",
                "-d",
                &body,
                &format!("{base_url}/admin/approvals/{approval_id}/approve"),
            ]),
        "approve parked Task",
    )?;
    if status != 200 {
        return Err(io::Error::other(format!("Task approval returned {status}")).into());
    }
    Ok(())
}

fn assert_resolution_emitted(
    base_url: &str,
    approval_id: &str,
    run_dir: &Path,
) -> Result<(), Box<dyn Error>> {
    let response = run_dir.join(format!("resolution-{approval_id}.json"));
    let status = curl_status(
        Command::new("curl")
            .args(["-sS", "-o"])
            .arg(&response)
            .args([
                "-w",
                "%{http_code}",
                &format!("{base_url}/test/resolutions"),
            ]),
        "read Task approval resolution notifications",
    )?;
    if status != 200 {
        return Err(io::Error::other(format!(
            "Task resolution notification lookup returned {status}"
        ))
        .into());
    }
    let resolutions = fs::read_to_string(response)?;
    assert!(
        resolutions.contains(approval_id),
        "the parked Task submitter's decision channel must receive its approval resolution"
    );
    Ok(())
}

fn get_output(
    base_url: &str,
    task_uid: &str,
    assertion: &str,
    output: &Path,
    _run_dir: &Path,
) -> Result<(), Box<dyn Error>> {
    let status = curl_status(
        Command::new("curl").args(["-sS", "-o"]).arg(output).args([
            "-w",
            "%{http_code}",
            "-H",
            &format!("Authorization: Bearer {assertion}"),
            &format!("{base_url}/v1/tasks/{task_uid}/outputs"),
        ]),
        "get Task outputs",
    )?;
    if status != 200 {
        return Err(io::Error::other(format!("Task output returned {status}")).into());
    }
    Ok(())
}

fn replace_runtime_for_stale_uid_test(
    base_url: &str,
    runtime_uid: &str,
    run_dir: &Path,
) -> Result<String, Box<dyn Error>> {
    let response = run_dir.join(format!("replace-runtime-{runtime_uid}.json"));
    let status = curl_status(
        Command::new("curl")
            .args(["-sS", "-o"])
            .arg(&response)
            .args([
                "-w",
                "%{http_code}",
                "-X",
                "POST",
                &format!("{base_url}/test/runtimes/{runtime_uid}/replace"),
            ]),
        "replace test runtime with a stale same-name UID",
    )?;
    if status != 201 {
        return Err(io::Error::other(format!(
            "test runtime replacement returned {status}: {}",
            fs::read_to_string(response)?
        ))
        .into());
    }
    json_string(&serde_json::from_slice(&fs::read(response)?)?, "runtimeUid").map(str::to_owned)
}

fn assert_phase_stays(
    base_url: &str,
    task_uid: &str,
    assertion: &str,
    expected_phase: &str,
    run_dir: &Path,
) -> Result<(), Box<dyn Error>> {
    for _ in 0..4 {
        let status = task_status(base_url, task_uid, assertion, run_dir)?;
        assert_eq!(
            status["phase"], expected_phase,
            "a Task bound to a stale runtime UID must fail closed instead of executing on a replacement"
        );
        std::thread::sleep(Duration::from_secs(1));
    }
    Ok(())
}

fn wait_for_runtime_binding(
    base_url: &str,
    task_uid: &str,
    assertion: &str,
    run_dir: &Path,
) -> Result<serde_json::Value, Box<dyn Error>> {
    wait_for(
        base_url,
        task_uid,
        assertion,
        |status| status["phase"] == "submitted" && status["runtimeUid"].is_string(),
        run_dir,
    )
}

fn wait_for(
    base_url: &str,
    task_uid: &str,
    assertion: &str,
    predicate: impl Fn(&serde_json::Value) -> bool,
    run_dir: &Path,
) -> Result<serde_json::Value, Box<dyn Error>> {
    for _attempt in 0..240 {
        let value = task_status(base_url, task_uid, assertion, run_dir)?;
        if predicate(&value) {
            return Ok(value);
        }
        let finalized_terminal = value["finalized"] == true
            && matches!(value["phase"].as_str(), Some("failed" | "cancelled"));
        if finalized_terminal {
            return Err(io::Error::other(format!(
                "Task {task_uid} reached terminal state: {value}"
            ))
            .into());
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    Err(io::Error::other(format!("Task {task_uid} did not reach expected state")).into())
}

fn task_status(
    base_url: &str,
    task_uid: &str,
    assertion: &str,
    run_dir: &Path,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let response = run_dir.join(format!("status-{task_uid}.json"));
    let status = curl_status(
        Command::new("curl")
            .args(["-sS", "-o"])
            .arg(&response)
            .args([
                "-w",
                "%{http_code}",
                "-H",
                &format!("Authorization: Bearer {assertion}"),
                &format!("{base_url}/v1/tasks/{task_uid}"),
            ]),
        "get Task status",
    )?;
    if status != 200 {
        return Err(io::Error::other(format!(
            "Task status returned {status}: {}",
            fs::read_to_string(response)?
        ))
        .into());
    }
    Ok(serde_json::from_slice(&fs::read(response)?)?)
}

fn curl_status(command: &mut Command, context: &str) -> Result<u16, Box<dyn Error>> {
    let output = command.output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "{context} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
        .into());
    }
    Ok(String::from_utf8(output.stdout)?.parse()?)
}

fn command(command: &mut Command, context: &str) -> Result<(), Box<dyn Error>> {
    let status = command.status()?;
    if !status.success() {
        return Err(io::Error::other(format!("{context} failed")).into());
    }
    Ok(())
}

fn json_string<'a>(value: &'a serde_json::Value, key: &str) -> Result<&'a str, Box<dyn Error>> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| io::Error::other(format!("Task response has no {key}")))
        .map_err(Into::into)
}

fn required(name: &str) -> Result<String, io::Error> {
    env::var(name).map_err(|_| io::Error::other(format!("{name} is required")))
}
