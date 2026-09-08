//! Codex command rendering for immutable disposable Task execution.

use steward_ports::{PortError, TaskExecutionAdapter, TaskExecutionPlan, TaskExecutionPlanRequest};

pub const CODEX_V1_ADAPTER: &str = "codex-v1";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexTaskExecutionAdapter {
    inference_endpoint: String,
}

impl CodexTaskExecutionAdapter {
    pub fn new(inference_endpoint: String) -> Result<Self, PortError> {
        if inference_endpoint.trim() != inference_endpoint
            || inference_endpoint.chars().any(char::is_control)
        {
            return Err(invalid_inference_endpoint());
        }
        let endpoint =
            reqwest::Url::parse(&inference_endpoint).map_err(|_| invalid_inference_endpoint())?;
        if !matches!(endpoint.scheme(), "http" | "https")
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || endpoint.port() == Some(0)
        {
            return Err(invalid_inference_endpoint());
        }
        Ok(Self {
            inference_endpoint: endpoint.to_string().trim_end_matches('/').to_owned(),
        })
    }
}

fn invalid_inference_endpoint() -> PortError {
    PortError::Rejected {
        reason: "Codex inference endpoint must be an exact HTTP(S) URL".to_owned(),
    }
}

impl TaskExecutionAdapter for CodexTaskExecutionAdapter {
    fn contract(&self) -> &'static str {
        CODEX_V1_ADAPTER
    }

    fn render(
        &self,
        request: TaskExecutionPlanRequest<'_>,
    ) -> Result<TaskExecutionPlan, PortError> {
        if !request.tools.is_empty() && request.tool_transport_endpoint.is_none() {
            return Err(PortError::Rejected {
                reason: "tool-bearing Codex execution requires a governed tool endpoint".to_owned(),
            });
        }
        let model = format!("{}/{}", request.model.provider, request.model.model);
        let mut adapter_config = format!(
            concat!(
                "model_provider = \"litellm\"\n",
                "approval_policy = \"never\"\n",
                "web_search = \"disabled\"\n",
                "[model_providers.litellm]\n",
                "name = \"LiteLLM\"\n",
                "base_url = {}\n",
                "env_key = \"OPENAI_API_KEY\"\n",
                "wire_api = \"responses\"\n",
                "requires_openai_auth = false\n",
            ),
            serde_json::to_string(&self.inference_endpoint).map_err(|error| PortError::Failed {
                reason: format!("render Codex inference endpoint: {error}"),
            })?,
        );
        if let Some(endpoint) = request.tool_transport_endpoint {
            let endpoint = serde_json::to_string(endpoint).map_err(|error| PortError::Failed {
                reason: format!("render Codex tool endpoint: {error}"),
            })?;
            adapter_config.push_str("[mcp_servers.steward]\nurl = ");
            adapter_config.push_str(&endpoint);
            adapter_config.push_str("\nbearer_token_env_var = \"STEWARD_MCP_GW_BEARER_TOKEN\"\n");
        }
        let tool_bearer_environment = if request.tool_transport_endpoint.is_some() {
            "STEWARD_MCP_GW_BEARER_TOKEN=openshell-token-grant-placeholder "
        } else {
            ""
        };
        let shell_command = format!(
            concat!(
                "set -eu; umask 077; ",
                "test \"$#\" -ge 6; ",
                "prompt=$1; model=$2; adapter_config=$3; executable=$4; expected_version=$5; ",
                "shift 5; ",
                "test \"$(\"$executable\" \"$@\")\" = \"$expected_version\"; ",
                "export CODEX_HOME=/sandbox/steward-codex; ",
                "mkdir -p \"$CODEX_HOME\" \"$STEWARD_OUTPUT_DIR/out\"; ",
                "test ! -e out; ln -s \"$STEWARD_OUTPUT_DIR/out\" out; ",
                "printf '%s' \"$adapter_config\" > \"$CODEX_HOME/config.toml\"; ",
                "{}",
                "OPENAI_API_KEY=openshell-token-grant-placeholder ",
                "\"$executable\" exec --ephemeral --skip-git-repo-check ",
                "--sandbox danger-full-access --model \"$model\" ",
                "--output-last-message \"$STEWARD_OUTPUT_DIR/result.txt\" -- \"$prompt\""
            ),
            tool_bearer_environment,
        );
        let mut command = vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            shell_command,
            "steward-workflow".to_owned(),
            request.workflow_prompt.to_owned(),
            model,
            adapter_config,
            request.binding.executable.clone(),
            request.binding.version_probe.expected_stdout.clone(),
        ];
        command.extend(request.binding.version_probe.arguments.iter().cloned());
        Ok(TaskExecutionPlan { command })
    }
}

#[cfg(test)]
mod tests {
    use steward_ports::{TaskExecutionAdapter, TaskExecutionPlanRequest};
    use steward_types::{
        DisposableExecutionBinding, ExecutionProviderProfile, ExecutionProviderProfiles,
        ExecutionVersionProbe, ModelRef, ToolGrant,
    };

    use super::{CODEX_V1_ADAPTER, CodexTaskExecutionAdapter};

    fn binding() -> DisposableExecutionBinding {
        DisposableExecutionBinding {
            schema_version: "steward/task-execution-binding/v1".to_owned(),
            binding_id: format!("sha256:{}", "a".repeat(64)),
            binding_digest: format!("sha256:{}", "a".repeat(64)),
            agent_ref: "example-agent@1.0.0".to_owned(),
            display_name: None,
            adapter: CODEX_V1_ADAPTER.to_owned(),
            image: format!(
                "registry.example.test/agents/example@sha256:{}",
                "b".repeat(64)
            ),
            executable: "/opt/example/bin/agent".to_owned(),
            version_probe: ExecutionVersionProbe {
                arguments: vec!["--version".to_owned()],
                expected_stdout: "example-agent 1.0.0".to_owned(),
            },
            provider_profiles: ExecutionProviderProfiles {
                tools: Some(ExecutionProviderProfile {
                    id: "example-tools-profile-v7".to_owned(),
                    digest: format!("sha256:{}", "c".repeat(64)),
                }),
                inference: Some(ExecutionProviderProfile {
                    id: "example-inference-profile-v7".to_owned(),
                    digest: format!("sha256:{}", "d".repeat(64)),
                }),
            },
        }
    }

    #[test]
    fn renders_pinned_governed_codex_command_without_secret_material() -> Result<(), String> {
        let adapter =
            CodexTaskExecutionAdapter::new("http://inference.example.test:4000/v1".to_owned())
                .map_err(|error| format!("configure adapter: {error:?}"))?;
        let binding = binding();
        let tools = [ToolGrant {
            provider: "example-tool".to_owned(),
            resource: "repository".to_owned(),
            action: "read".to_owned(),
        }];
        let plan = adapter
            .render(TaskExecutionPlanRequest {
                workflow_prompt: "Review the repository state.",
                model: &ModelRef {
                    provider: "example-model".to_owned(),
                    model: "large".to_owned(),
                },
                tools: &tools,
                tool_transport_endpoint: Some("https://tools.example.test/mcp"),
                binding: &binding,
            })
            .map_err(|error| format!("render adapter plan: {error:?}"))?;

        let shell = &plan.command[2];
        let config = &plan.command[6];
        assert!(shell.contains("OPENAI_API_KEY=openshell-token-grant-placeholder"));
        assert!(shell.contains("STEWARD_MCP_GW_BEARER_TOKEN=openshell-token-grant-placeholder"));
        assert!(shell.contains("--model \"$model\""));
        assert!(shell.contains("ln -s \"$STEWARD_OUTPUT_DIR/out\" out"));
        assert!(!shell.contains("$STEWARD_OUTPUT_DIR/out/result.txt"));
        assert!(shell.contains("--ephemeral"));
        assert!(shell.contains("--sandbox danger-full-access"));
        assert!(config.contains("http://inference.example.test:4000/v1"));
        assert!(config.contains("[mcp_servers.steward]"));
        assert!(config.contains("https://tools.example.test/mcp"));
        assert!(config.contains("bearer_token_env_var = \"STEWARD_MCP_GW_BEARER_TOKEN\""));
        assert_eq!(plan.command[4], "Review the repository state.");
        assert_eq!(plan.command[5], "example-model/large");
        assert_eq!(plan.command[7], binding.executable);
        assert_eq!(plan.command[8], binding.version_probe.expected_stdout);
        assert_eq!(plan.command[9], "--version");
        Ok(())
    }

    #[test]
    fn tool_less_plan_contains_no_tool_server_or_bearer_placeholder() -> Result<(), String> {
        let adapter =
            CodexTaskExecutionAdapter::new("https://inference.example.test/v1".to_owned())
                .map_err(|error| format!("configure adapter: {error:?}"))?;
        let binding = binding();
        let plan = adapter
            .render(TaskExecutionPlanRequest {
                workflow_prompt: "Inspect input.",
                model: &ModelRef {
                    provider: "example-model".to_owned(),
                    model: "small".to_owned(),
                },
                tools: &[],
                tool_transport_endpoint: None,
                binding: &binding,
            })
            .map_err(|error| format!("render adapter plan: {error:?}"))?;
        assert!(!plan.command[6].contains("[mcp_servers."));
        assert!(!plan.command[6].contains("STEWARD_MCP_GW_BEARER_TOKEN"));
        assert!(!plan.command[2].contains("STEWARD_MCP_GW_BEARER_TOKEN"));
        Ok(())
    }

    #[test]
    fn rejects_missing_tool_endpoint_and_ambiguous_inference_urls() -> Result<(), String> {
        for endpoint in [
            " https://inference.example.test/v1",
            "ftp://inference.example.test/v1",
            "https://alice@inference.example.test/v1",
            "https://inference.example.test/v1?target=other",
            "https://inference.example.test/v1#fragment",
        ] {
            assert!(CodexTaskExecutionAdapter::new(endpoint.to_owned()).is_err());
        }
        let adapter =
            CodexTaskExecutionAdapter::new("https://inference.example.test/v1".to_owned())
                .map_err(|error| format!("configure adapter: {error:?}"))?;
        let binding = binding();
        assert!(
            adapter
                .render(TaskExecutionPlanRequest {
                    workflow_prompt: "Inspect input.",
                    model: &ModelRef {
                        provider: "example-model".to_owned(),
                        model: "small".to_owned(),
                    },
                    tools: &[ToolGrant {
                        provider: "example-tool".to_owned(),
                        resource: "repository".to_owned(),
                        action: "read".to_owned(),
                    }],
                    tool_transport_endpoint: None,
                    binding: &binding,
                })
                .is_err(),
            "tool-bearing execution must fail closed without a governed endpoint"
        );
        Ok(())
    }
}
