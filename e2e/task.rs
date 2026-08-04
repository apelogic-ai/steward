use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use kube::api::{Api, ListParams};
use steward_store::PgStore;
use steward_types::{AgentRuntime, Principal};

#[tokio::test]
async fn task_submission_real_sandbox_lifecycle() -> Result<(), Box<dyn Error>> {
    let base_url = required("STEWARD_TASK_URL")?;
    let runtime_api = Api::<AgentRuntime>::all(kube::Client::try_default().await?);
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
        &task_uid,
        "github-assertion",
        &input_tar,
        &run_dir,
    )?;
    execute(&base_url, &task_uid, "github-assertion", &run_dir)?;
    execute(&base_url, &task_uid, "github-assertion", &run_dir)?;

    let store = PgStore::connect(&required("STEWARD_TEST_DATABASE_URL")?).await?;
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
        &task_uid,
        "github-assertion",
        |status| status["phase"] == "succeeded",
        &run_dir,
    )?;
    assert_eq!(succeeded["finalized"], false);
    let output_tar = run_dir.join("output.tar");
    get_output(
        &base_url,
        &task_uid,
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
    delete_task(&base_url, &task_uid, "github-assertion", &run_dir)?;
    wait_for(
        &base_url,
        &task_uid,
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
    let scheduled_runtime_uid = json_string(&scheduled, "runtimeUid")?;
    let scheduled_runtime = runtime_by_uid(&runtime_api, scheduled_runtime_uid)
        .await?
        .ok_or_else(|| io::Error::other("scheduled Task runtime was not created"))?;
    assert_eq!(scheduled_runtime.spec.owner.0, "owner@example.org");
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
    let standing_runtime_uid = json_string(&standing, "runtimeUid")?;
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
    let failing_runtime_uid = json_string(&failing, "runtimeUid")?;
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

fn submit(
    base_url: &str,
    assertion: &str,
    key: &str,
    body: &str,
    run_dir: &Path,
) -> Result<serde_json::Value, Box<dyn Error>> {
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
    Ok(serde_json::from_slice(&fs::read(response)?)?)
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

fn wait_for(
    base_url: &str,
    task_uid: &str,
    assertion: &str,
    predicate: impl Fn(&serde_json::Value) -> bool,
    run_dir: &Path,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let response = run_dir.join(format!("status-{task_uid}.json"));
    for _attempt in 0..240 {
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
        if status == 200 {
            let value = serde_json::from_slice::<serde_json::Value>(&fs::read(&response)?)?;
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
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    Err(io::Error::other(format!("Task {task_uid} did not reach expected state")).into())
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
