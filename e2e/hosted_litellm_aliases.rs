use std::collections::BTreeSet;
use std::env;
use std::error::Error;
use std::io;
use std::time::Duration;

use reqwest::{Client, Response, StatusCode};
use serde_json::{Value, json};
use steward_adapter_claude_code::{CLAUDE_CODE_V1_ADAPTER, ClaudeCodeTaskExecutionAdapter};
use steward_adapter_codex::{CODEX_V1_ADAPTER, CodexTaskExecutionAdapter};
use steward_adapter_litellm::{LiteLlmAdapter, LiteLlmConfig};
use steward_ports::{
    InferencePlane, InferenceRequest, TaskExecutionAdapter, TaskExecutionPlanRequest,
};
use steward_types::{
    Budget, DisposableExecutionBinding, ExecutionModelCompatibility, ExecutionProviderProfiles,
    ExecutionVersionProbe, ModelRef, RuntimeId,
};

const CLAUDE_ALIAS: &str = "claude-sonnet";
const CODEX_ALIAS: &str = "codex-mini";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

fn binding(adapter: &str, provider: &str) -> DisposableExecutionBinding {
    DisposableExecutionBinding {
        schema_version: "steward/task-execution-binding/v1".to_owned(),
        binding_id: format!("sha256:{}", "a".repeat(64)),
        binding_digest: format!("sha256:{}", "a".repeat(64)),
        agent_ref: "example-agent@1.0.0".to_owned(),
        display_name: None,
        adapter: adapter.to_owned(),
        image: format!(
            "registry.example.test/agents/example@sha256:{}",
            "b".repeat(64)
        ),
        executable: "/opt/example/bin/agent".to_owned(),
        version_probe: ExecutionVersionProbe {
            arguments: vec!["--version".to_owned()],
            expected_stdout: "example-agent 1.0.0".to_owned(),
        },
        models: Some(ExecutionModelCompatibility {
            providers: vec![provider.to_owned()],
        }),
        remote_shell_targets: Default::default(),
        provider_profiles: ExecutionProviderProfiles::default(),
    }
}

fn model(provider: &str, alias: &str) -> ModelRef {
    ModelRef {
        provider: provider.to_owned(),
        model: alias.to_owned(),
    }
}

fn inference_request(runtime: &str, model: ModelRef) -> InferenceRequest {
    InferenceRequest {
        runtime: RuntimeId(runtime.to_owned()),
        models: vec![model],
        budget: Budget {
            monthly_limit: "10.00".to_owned(),
            single_run_limit: Some("1.00".to_owned()),
            currency: "USD".to_owned(),
        },
    }
}

fn rendered_model(
    plan: &steward_ports::TaskExecutionPlan,
    launcher: &str,
) -> Result<String, io::Error> {
    plan.command.get(5).cloned().ok_or_else(|| {
        io::Error::other(format!(
            "{launcher} launcher omitted the configured model argument"
        ))
    })
}

fn rendered_claude_endpoint(plan: &steward_ports::TaskExecutionPlan) -> Result<String, io::Error> {
    plan.command
        .get(6)
        .cloned()
        .ok_or_else(|| io::Error::other("Claude Code launcher omitted its inference endpoint"))
}

fn rendered_codex_endpoint(plan: &steward_ports::TaskExecutionPlan) -> Result<String, io::Error> {
    let config = plan
        .command
        .get(6)
        .ok_or_else(|| io::Error::other("Codex launcher omitted its adapter configuration"))?;
    let encoded = config
        .lines()
        .find_map(|line| line.strip_prefix("base_url = "))
        .ok_or_else(|| io::Error::other("Codex launcher omitted its inference endpoint"))?;
    serde_json::from_str(encoded)
        .map_err(|error| io::Error::other(format!("read Codex inference endpoint: {error}")))
}

fn contains_string(value: &Value, expected: &str) -> bool {
    match value {
        Value::String(value) => value == expected,
        Value::Array(values) => values.iter().any(|value| contains_string(value, expected)),
        Value::Object(values) => values
            .values()
            .any(|value| contains_string(value, expected)),
        _ => false,
    }
}

async fn response_error(response: Response, operation: &str) -> io::Error {
    let status = response.status();
    let detail = response
        .text()
        .await
        .unwrap_or_else(|_| "response body was unreadable".to_owned());
    let detail = detail.chars().take(512).collect::<String>();
    io::Error::other(format!("{operation} returned HTTP {status}: {detail}"))
}

async fn require_success(response: Response, operation: &str) -> Result<(), io::Error> {
    if response.status().is_success() {
        Ok(())
    } else {
        Err(response_error(response, operation).await)
    }
}

async fn exercise_aliases(
    base_url: &str,
    management_key: &str,
    adapter: &LiteLlmAdapter,
    claude_request: &InferenceRequest,
    codex_request: &InferenceRequest,
) -> Result<(), Box<dyn Error>> {
    let claude_key = adapter
        .provision(claude_request)
        .await
        .map_err(|error| io::Error::other(format!("provision Claude runtime key: {error:?}")))?;
    let codex_key = adapter
        .provision(codex_request)
        .await
        .map_err(|error| io::Error::other(format!("provision Codex runtime key: {error:?}")))?;

    let claude_model = &claude_request.models[0];
    let claude_plan = ClaudeCodeTaskExecutionAdapter::new(base_url.to_owned())
        .map_err(|error| io::Error::other(format!("configure Claude launcher: {error:?}")))?
        .render(TaskExecutionPlanRequest {
            workflow_prompt: "Inspect input.",
            model: claude_model,
            tools: &[],
            tool_transport_endpoint: None,
            binding: &binding(CLAUDE_CODE_V1_ADAPTER, &claude_model.provider),
        })
        .map_err(|error| io::Error::other(format!("render Claude launcher: {error:?}")))?;
    let claude_model = rendered_model(&claude_plan, "Claude Code")?;
    let claude_endpoint = rendered_claude_endpoint(&claude_plan)?;

    let codex_model = &codex_request.models[0];
    let codex_plan = CodexTaskExecutionAdapter::new(format!("{base_url}/v1"))
        .map_err(|error| io::Error::other(format!("configure Codex launcher: {error:?}")))?
        .render(TaskExecutionPlanRequest {
            workflow_prompt: "Inspect input.",
            model: codex_model,
            tools: &[],
            tool_transport_endpoint: None,
            binding: &binding(CODEX_V1_ADAPTER, &codex_model.provider),
        })
        .map_err(|error| io::Error::other(format!("render Codex launcher: {error:?}")))?;
    let codex_model = rendered_model(&codex_plan, "Codex")?;
    let codex_endpoint = rendered_codex_endpoint(&codex_plan)?;

    let client = Client::builder().timeout(REQUEST_TIMEOUT).build()?;
    require_success(
        client
            .post(format!("{claude_endpoint}/v1/messages"))
            .header("x-api-key", claude_key.credential.expose_secret())
            .header("anthropic-version", "2023-06-01")
            .json(&json!({
                "model": claude_model,
                "max_tokens": 16,
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .send()
            .await?,
        "Claude alias request",
    )
    .await?;

    require_success(
        client
            .post(format!("{codex_endpoint}/responses"))
            .bearer_auth(codex_key.credential.expose_secret())
            .json(&json!({
                "model": codex_model,
                "input": "hello",
                "max_output_tokens": 16
            }))
            .send()
            .await?,
        "Codex alias request",
    )
    .await?;

    let refused = client
        .post(format!("{claude_endpoint}/v1/messages"))
        .header("x-api-key", claude_key.credential.expose_secret())
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": CODEX_ALIAS,
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .send()
        .await?;
    if refused.status() != StatusCode::FORBIDDEN {
        return Err(response_error(refused, "Claude-key cross-alias request")
            .await
            .into());
    }

    let refused = client
        .post(format!("{codex_endpoint}/responses"))
        .bearer_auth(codex_key.credential.expose_secret())
        .json(&json!({
            "model": CLAUDE_ALIAS,
            "input": "hello",
            "max_output_tokens": 16
        }))
        .send()
        .await?;
    if refused.status() != StatusCode::FORBIDDEN {
        return Err(response_error(refused, "Codex-key cross-alias request")
            .await
            .into());
    }

    let mut spend_logs = Value::Array(Vec::new());
    for attempt in 1..=30 {
        let spend_response = client
            .get(format!("{base_url}/spend/logs"))
            .bearer_auth(management_key)
            .send()
            .await?;
        if !spend_response.status().is_success() {
            return Err(response_error(spend_response, "spend-log read")
                .await
                .into());
        }
        spend_logs = spend_response.json().await?;
        if spend_logs.as_array().is_some_and(|rows| rows.len() >= 2) {
            break;
        }
        if attempt < 30 {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    let rows = spend_logs
        .as_array()
        .ok_or_else(|| io::Error::other("spend-log read did not return rows"))?;
    let fields = rows
        .iter()
        .filter_map(Value::as_object)
        .flat_map(|row| row.keys().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(", ");
    for reference in [&claude_key.reference, &codex_key.reference] {
        if !rows.iter().any(|row| contains_string(row, reference)) {
            return Err(io::Error::other(format!(
                "spend logs omitted runtime key {reference}; available fields: {fields}"
            ))
            .into());
        }
    }

    Ok(())
}

#[tokio::test]
async fn real_hosted_litellm_accepts_both_launchers_exact_aliases() -> Result<(), Box<dyn Error>> {
    let base_url = env::var("STEWARD_TEST_LITELLM_URL")?;
    let management_key = env::var("STEWARD_TEST_LITELLM_MANAGEMENT_KEY")?;
    let adapter = LiteLlmAdapter::new(LiteLlmConfig {
        base_url: base_url.clone(),
        master_key: management_key.clone(),
    })
    .map_err(|error| io::Error::other(format!("configure LiteLLM adapter: {error:?}")))?;
    let claude_request = inference_request(
        "hosted-claude-alias-runtime",
        model("anthropic", CLAUDE_ALIAS),
    );
    let codex_request =
        inference_request("hosted-codex-alias-runtime", model("openai", CODEX_ALIAS));

    let outcome = exercise_aliases(
        &base_url,
        &management_key,
        &adapter,
        &claude_request,
        &codex_request,
    )
    .await;
    let mut cleanup_error = None;
    for request in [&claude_request, &codex_request] {
        if let Err(error) = adapter.revoke(request).await {
            cleanup_error = Some(io::Error::other(format!(
                "revoke test runtime key: {error:?}"
            )));
        }
    }
    outcome?;
    if let Some(error) = cleanup_error {
        return Err(error.into());
    }
    Ok(())
}
