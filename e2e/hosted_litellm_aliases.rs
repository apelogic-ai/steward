use std::env;
use std::error::Error;
use std::io;

use reqwest::{Client, Response, StatusCode};
use serde_json::json;
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

    let client = Client::new();
    require_success(
        client
            .post(format!("{base_url}/v1/messages"))
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
            .post(format!("{base_url}/v1/responses"))
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
        .post(format!("{base_url}/v1/messages"))
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
        return Err(response_error(refused, "cross-alias request").await.into());
    }

    require_success(
        client
            .get(format!("{base_url}/spend/logs"))
            .bearer_auth(management_key)
            .send()
            .await?,
        "spend-log read",
    )
    .await?;

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
