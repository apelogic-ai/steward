use std::error::Error;
use std::io;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::routing::any;
use serde_json::{Value, json};
use steward_adapter_claude_code::{CLAUDE_CODE_V1_ADAPTER, ClaudeCodeTaskExecutionAdapter};
use steward_adapter_codex::{CODEX_V1_ADAPTER, CodexTaskExecutionAdapter};
use steward_ports::{TaskExecutionAdapter, TaskExecutionPlanRequest};
use steward_types::{
    DisposableExecutionBinding, ExecutionModelCompatibility, ExecutionProviderProfiles,
    ExecutionVersionProbe, ModelRef,
};
use tower::ServiceExt;

const CONFIGURED_ALIAS: &str = "hosted-alias";
const RUNTIME_KEY: &str = "fake-runtime-key";

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

fn model() -> ModelRef {
    ModelRef {
        provider: "example-model".to_owned(),
        model: CONFIGURED_ALIAS.to_owned(),
    }
}

async fn hosted_litellm(request: Request) -> StatusCode {
    if request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        != Some("Bearer fake-runtime-key")
    {
        return StatusCode::UNAUTHORIZED;
    }
    if !matches!(request.uri().path(), "/v1/responses" | "/v1/messages") {
        return StatusCode::NOT_FOUND;
    }
    let Ok(bytes) = to_bytes(request.into_body(), 4096).await else {
        return StatusCode::BAD_REQUEST;
    };
    let Ok(body) = serde_json::from_slice::<Value>(&bytes) else {
        return StatusCode::BAD_REQUEST;
    };
    if body.get("model").and_then(Value::as_str) == Some(CONFIGURED_ALIAS) {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn inference_status(path: &str, model: &str) -> Result<StatusCode, Box<dyn Error>> {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {RUNTIME_KEY}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({ "model": model }).to_string()))?;
    Ok(Router::new()
        .fallback(any(hosted_litellm))
        .oneshot(request)
        .await?
        .status())
}

#[tokio::test]
async fn both_launch_paths_use_the_runtime_keys_exact_hosted_alias() -> Result<(), Box<dyn Error>> {
    let selected_model = model();

    let codex_binding = binding(CODEX_V1_ADAPTER, &selected_model.provider);
    let codex_plan = CodexTaskExecutionAdapter::new("https://inference.example.test/v1".to_owned())
        .map_err(|error| io::Error::other(format!("configure Codex adapter: {error:?}")))?
        .render(TaskExecutionPlanRequest {
            workflow_prompt: "Inspect input.",
            model: &selected_model,
            tools: &[],
            tool_transport_endpoint: None,
            binding: &codex_binding,
        })
        .map_err(|error| io::Error::other(format!("render Codex plan: {error:?}")))?;

    let claude_binding = binding(CLAUDE_CODE_V1_ADAPTER, &selected_model.provider);
    let claude_plan =
        ClaudeCodeTaskExecutionAdapter::new("https://inference.example.test".to_owned())
            .map_err(|error| io::Error::other(format!("configure Claude adapter: {error:?}")))?
            .render(TaskExecutionPlanRequest {
                workflow_prompt: "Inspect input.",
                model: &selected_model,
                tools: &[],
                tool_transport_endpoint: None,
                binding: &claude_binding,
            })
            .map_err(|error| io::Error::other(format!("render Claude plan: {error:?}")))?;

    for (path, rendered_model) in [
        ("/v1/responses", codex_plan.command[5].as_str()),
        ("/v1/messages", claude_plan.command[5].as_str()),
    ] {
        assert_eq!(
            inference_status(path, rendered_model).await?,
            StatusCode::OK
        );
        assert_eq!(
            inference_status(path, "unknown-alias").await?,
            StatusCode::NOT_FOUND
        );
    }
    Ok(())
}
