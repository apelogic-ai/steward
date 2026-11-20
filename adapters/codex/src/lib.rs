//! Codex command rendering for immutable disposable Task execution.

use steward_ports::{
    PortError, TaskExecutionAdapter, TaskExecutionPlan, TaskExecutionPlanRequest,
    governed_command_grants,
};

pub const CODEX_V1_ADAPTER: &str = "codex-v1";
const ABSENT_COMMAND_URL: &str = "-";

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
        let mut endpoint =
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
        let normalized_path = endpoint.path().trim_end_matches('/').to_owned();
        if let Some(base_path) = normalized_path.strip_suffix("/responses") {
            endpoint.set_path(if base_path.is_empty() { "/" } else { base_path });
        } else {
            endpoint.set_path(&normalized_path);
        }
        Ok(Self {
            inference_endpoint: endpoint.to_string().trim_end_matches('/').to_owned(),
        })
    }
}

fn governed_command_urls(
    request: &TaskExecutionPlanRequest<'_>,
) -> Result<(String, String), PortError> {
    let grants = governed_command_grants(request.tools);
    if !grants.cli && !grants.remote_shell {
        return Ok((ABSENT_COMMAND_URL.to_owned(), ABSENT_COMMAND_URL.to_owned()));
    }
    let endpoint = request
        .tool_transport_endpoint
        .ok_or_else(|| PortError::Rejected {
            reason: "governed command execution requires a governed tool endpoint".to_owned(),
        })?;
    let mut endpoint = reqwest::Url::parse(endpoint).map_err(|_| PortError::Rejected {
        reason: "Codex tool endpoint must be an exact HTTP(S) URL".to_owned(),
    })?;
    if !matches!(endpoint.scheme(), "http" | "https")
        || endpoint.host_str().is_none()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
        || endpoint.port() == Some(0)
    {
        return Err(PortError::Rejected {
            reason: "Codex tool endpoint must be an exact HTTP(S) URL".to_owned(),
        });
    }
    let cli_url = if grants.cli {
        endpoint.set_path("/cli/run");
        endpoint.to_string()
    } else {
        ABSENT_COMMAND_URL.to_owned()
    };
    let shell_url = if grants.remote_shell {
        endpoint.set_path("/shell/run");
        endpoint.to_string()
    } else {
        ABSENT_COMMAND_URL.to_owned()
    };
    Ok((cli_url, shell_url))
}

/// Shell step that writes `$CODEX_HOME/config.toml` from `$adapter_config`.
///
/// It first asks the model gateway, with the same proxy-swapped credential Codex itself uses,
/// for the selected model's input context window. A reported window becomes Codex's top-level
/// `model_context_window`, from which Codex derives its own automatic compaction threshold.
/// Otherwise Codex starts exactly as before and one line says so.
fn codex_config_command(model_info_url: &str) -> String {
    format!(
        concat!(
            "context_window=$(curl --silent --fail --max-time 30 ",
            "${{HTTPS_PROXY:+--proxy \"$HTTPS_PROXY\"}} ${{SSL_CERT_FILE:+--cacert \"$SSL_CERT_FILE\"}} ",
            "--header 'Authorization: Bearer openshell-token-grant-placeholder' {} 2>/dev/null ",
            "| jq --raw-output --arg model \"$model\" ",
            "'[.data[]? | select(.model_name == $model) | .model_info.max_input_tokens? ",
            "| select(type == \"number\" and . > 0 and . == floor)] | min // empty' 2>/dev/null) ",
            "|| context_window=; ",
            // Codex reads the window as a signed 64-bit integer.
            "case $context_window in ''|0*|*[!0-9]*|???????????????????*) context_window= ;; esac; ",
            "if [ -z \"$context_window\" ]; then ",
            "echo 'The model gateway did not report a context window for this model; the coding agent uses its default.' >&2; fi; ",
            "{{ if [ -n \"$context_window\" ]; then printf 'model_context_window = %s\\n' \"$context_window\"; fi; ",
            "printf '%s' \"$adapter_config\"; }} > \"$CODEX_HOME/config.toml\"; ",
        ),
        shell_single_quote(model_info_url),
    )
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
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
        if !request.binding.supports_model(request.model) {
            return Err(PortError::Rejected {
                reason: "This coding agent does not support the selected model.".to_owned(),
            });
        }
        if !request.tools.is_empty() && request.tool_transport_endpoint.is_none() {
            return Err(PortError::Rejected {
                reason: "tool-bearing Codex execution requires a governed tool endpoint".to_owned(),
            });
        }
        let model = request.model.model.clone();
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
        let (cli_url, shell_url) = governed_command_urls(&request)?;
        let shell_command = format!(
            concat!(
                "set -eu; umask 077; ",
                "test \"$#\" -ge 8; ",
                "prompt=$1; model=$2; adapter_config=$3; executable=$4; expected_version=$5; cli_url=$6; shell_url=$7; ",
                "shift 7; ",
                "repository_context=${{STEWARD_REPOSITORY_CONTEXT-}}; ",
                "if [ -n \"$repository_context\" ]; then prompt=$(printf '%s\\n\\n%s' \"$repository_context\" \"$prompt\"); fi; ",
                "test \"$(\"$executable\" \"$@\")\" = \"$expected_version\"; ",
                "export CODEX_HOME=/sandbox/steward-codex; ",
                "mkdir -p \"$CODEX_HOME\" \"$STEWARD_OUTPUT_DIR/out\"; ",
                "test ! -e out; ln -s \"$STEWARD_OUTPUT_DIR/out\" out; ",
                "{}",
                "if [ \"$cli_url\" != \"-\" ]; then export MCP_GW_CLI_URL=\"$cli_url\"; fi; ",
                "if [ \"$shell_url\" != \"-\" ]; then export MCP_GW_SHELL_URL=\"$shell_url\"; fi; ",
                "{}",
                "OPENAI_API_KEY=openshell-token-grant-placeholder ",
                "\"$executable\" exec --ephemeral --skip-git-repo-check ",
                "--sandbox danger-full-access --model \"$model\" ",
                "--output-last-message \"$STEWARD_OUTPUT_DIR/result.txt\" -- \"$prompt\""
            ),
            codex_config_command(&format!("{}/model/info", self.inference_endpoint)),
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
            cli_url,
            shell_url,
        ];
        command.extend(request.binding.version_probe.arguments.iter().cloned());
        Ok(TaskExecutionPlan { command })
    }
}

#[cfg(test)]
mod tests {
    use steward_ports::{TaskExecutionAdapter, TaskExecutionPlanRequest};
    use steward_types::{
        DisposableExecutionBinding, ExecutionModelCompatibility, ExecutionProviderProfile,
        ExecutionProviderProfiles, ExecutionVersionProbe, ModelRef, ToolGrant,
    };

    use super::{
        CODEX_V1_ADAPTER, CodexTaskExecutionAdapter, codex_config_command, shell_single_quote,
    };

    const MODEL_INFO_URL: &str = "https://inference.example.test/v1/model/info";

    struct ConfigStep {
        config: String,
        curl_arguments: String,
        stderr: String,
    }

    /// Runs the plan's config step in a real shell with real `jq` and a fake `curl` that answers
    /// with `model_info` (or fails when it is `None`).
    fn run_config_step(
        adapter_config: &str,
        model: &str,
        model_info: Option<&str>,
    ) -> Result<ConfigStep, String> {
        let root = std::env::temp_dir().join(format!(
            "steward-codex-config-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|error| error.to_string())?
                .as_nanos()
        ));
        let result = run_config_step_in(&root, adapter_config, model, model_info);
        std::fs::remove_dir_all(&root).map_err(|error| error.to_string())?;
        result
    }

    fn run_config_step_in(
        root: &std::path::Path,
        adapter_config: &str,
        model: &str,
        model_info: Option<&str>,
    ) -> Result<ConfigStep, String> {
        use std::os::unix::fs::PermissionsExt;

        let bin = root.join("bin");
        let codex_home = root.join("codex-home");
        std::fs::create_dir_all(&bin).map_err(|error| error.to_string())?;
        std::fs::create_dir_all(&codex_home).map_err(|error| error.to_string())?;
        let curl_arguments = root.join("curl-arguments");
        let response = root.join("model-info.json");
        let fake_curl = match model_info {
            Some(body) => {
                std::fs::write(&response, body).map_err(|error| error.to_string())?;
                format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\ncat '{}'\n",
                    curl_arguments.display(),
                    response.display()
                )
            }
            None => format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\necho 'curl: (22) The requested URL returned error: 403' >&2\nexit 22\n",
                curl_arguments.display()
            ),
        };
        let curl = bin.join("curl");
        std::fs::write(&curl, fake_curl).map_err(|error| error.to_string())?;
        std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755))
            .map_err(|error| error.to_string())?;
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").map_err(|error| error.to_string())?
        );
        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                "set -eu; adapter_config=$1; model=$2; {}",
                codex_config_command(MODEL_INFO_URL)
            ))
            .arg("codex-config-test")
            .arg(adapter_config)
            .arg(model)
            .env_clear()
            .env("PATH", path)
            .env("CODEX_HOME", &codex_home)
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(format!(
                "config step failed: {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(ConfigStep {
            config: std::fs::read_to_string(codex_home.join("config.toml"))
                .map_err(|error| error.to_string())?,
            curl_arguments: std::fs::read_to_string(&curl_arguments).unwrap_or_default(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn tool_less_plan() -> Result<Vec<String>, String> {
        let adapter =
            CodexTaskExecutionAdapter::new("https://inference.example.test/v1".to_owned())
                .map_err(|error| format!("configure adapter: {error:?}"))?;
        let binding = binding();
        adapter
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
            .map(|plan| plan.command)
            .map_err(|error| format!("render adapter plan: {error:?}"))
    }

    #[test]
    fn model_info_url_reaches_the_shell_as_one_literal_word() -> Result<(), String> {
        let url = "https://inference.example.test/it's $(id) `id`/model/info";
        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("printf '%s' {}", shell_single_quote(url)))
            .output()
            .map_err(|error| error.to_string())?;
        assert_eq!(String::from_utf8_lossy(&output.stdout), url);
        Ok(())
    }

    #[test]
    fn gateway_context_window_is_written_into_the_codex_config() -> Result<(), String> {
        let plan = tool_less_plan()?;
        assert!(plan[2].contains(&codex_config_command(MODEL_INFO_URL)));
        let adapter_config = &plan[6];

        let step = run_config_step(
            adapter_config,
            "hosted-alias",
            Some(
                r#"{"data":[
                    {"model_name":"other-alias","model_info":{"max_input_tokens":1000}},
                    {"model_name":"hosted-alias","model_info":{"max_input_tokens":3000,"max_tokens":999}},
                    {"model_name":"hosted-alias","model_info":{}},
                    {"model_name":"hosted-alias","model_info":{"max_input_tokens":2000},"unknown_field":true}
                ]}"#,
            ),
        )?;

        assert_eq!(
            step.config,
            format!("model_context_window = 2000\n{adapter_config}"),
            "the smallest reported window is a top-level key ahead of every table"
        );
        assert!(
            step.curl_arguments
                .lines()
                .any(|line| line == MODEL_INFO_URL)
        );
        assert!(
            step.curl_arguments
                .lines()
                .any(|line| line == "Authorization: Bearer openshell-token-grant-placeholder"),
            "the lookup uses the same proxy-swapped credential as Codex"
        );
        assert_eq!(step.stderr, "");
        Ok(())
    }

    #[test]
    fn codex_config_is_unchanged_without_a_reported_context_window() -> Result<(), String> {
        let plan = tool_less_plan()?;
        let adapter_config = &plan[6];

        for model_info in [
            None,
            Some("not json"),
            Some("{}"),
            Some(
                r#"{"data":[{"model_name":"other-alias","model_info":{"max_input_tokens":1000}}]}"#,
            ),
            Some(r#"{"data":[{"model_name":"hosted-alias"}]}"#),
            Some(r#"{"data":[{"model_name":"hosted-alias","model_info":{"max_tokens":1000}}]}"#),
            Some(
                r#"{"data":[{"model_name":"hosted-alias","model_info":{"max_input_tokens":null}}]}"#,
            ),
            Some(r#"{"data":[{"model_name":"hosted-alias","model_info":{"max_input_tokens":0}}]}"#),
            Some(
                r#"{"data":[{"model_name":"hosted-alias","model_info":{"max_input_tokens":-1}}]}"#,
            ),
            Some(
                r#"{"data":[{"model_name":"hosted-alias","model_info":{"max_input_tokens":1.5}}]}"#,
            ),
            Some(
                r#"{"data":[{"model_name":"hosted-alias","model_info":{"max_input_tokens":"1000"}}]}"#,
            ),
            Some(
                r#"{"data":[{"model_name":"hosted-alias","model_info":{"max_input_tokens":18446744073709551615}}]}"#,
            ),
        ] {
            let step = run_config_step(adapter_config, "hosted-alias", model_info)?;
            assert_eq!(&step.config, adapter_config, "{model_info:?}");
            assert_eq!(
                step.stderr.lines().count(),
                1,
                "exactly one line explains the default for {model_info:?}: {:?}",
                step.stderr
            );
        }
        Ok(())
    }

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
            models: Some(ExecutionModelCompatibility {
                providers: vec!["example-model".to_owned()],
            }),
            remote_shell_targets: Default::default(),
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
        assert!(shell.contains("STEWARD_REPOSITORY_CONTEXT"));
        assert!(shell.contains("prompt=$(printf"));
        assert!(shell.contains("ln -s \"$STEWARD_OUTPUT_DIR/out\" out"));
        assert!(!shell.contains("$STEWARD_OUTPUT_DIR/out/result.txt"));
        assert!(shell.contains("--ephemeral"));
        assert!(shell.contains("--sandbox danger-full-access"));
        assert!(config.contains("http://inference.example.test:4000/v1"));
        assert!(config.contains("[mcp_servers.steward]"));
        assert!(config.contains("https://tools.example.test/mcp"));
        assert!(config.contains("bearer_token_env_var = \"STEWARD_MCP_GW_BEARER_TOKEN\""));
        assert_eq!(plan.command[4], "Review the repository state.");
        assert_eq!(plan.command[5], "large");
        assert_eq!(plan.command[7], binding.executable);
        assert_eq!(plan.command[8], binding.version_probe.expected_stdout);
        assert_eq!(plan.command[11], "--version");
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
    fn keeps_an_explicitly_qualified_model_alias_exact() -> Result<(), String> {
        let adapter =
            CodexTaskExecutionAdapter::new("https://inference.example.test/v1".to_owned())
                .map_err(|error| format!("configure adapter: {error:?}"))?;
        let binding = binding();
        let plan = adapter
            .render(TaskExecutionPlanRequest {
                workflow_prompt: "Inspect input.",
                model: &ModelRef {
                    provider: "example-model".to_owned(),
                    model: "example-model/legacy-alias".to_owned(),
                },
                tools: &[],
                tool_transport_endpoint: None,
                binding: &binding,
            })
            .map_err(|error| format!("render adapter plan: {error:?}"))?;

        assert_eq!(plan.command[5], "example-model/legacy-alias");
        Ok(())
    }

    #[test]
    fn governed_command_grants_expose_routes_without_helper_token_in_agent_environment()
    -> Result<(), String> {
        let adapter =
            CodexTaskExecutionAdapter::new("https://inference.example.test/v1".to_owned())
                .map_err(|error| format!("configure adapter: {error:?}"))?;
        let binding = binding();
        let model = ModelRef {
            provider: "example-model".to_owned(),
            model: "small".to_owned(),
        };
        let endpoint = Some("https://tools.example.test/mcp");
        let command_tools = [
            ToolGrant {
                provider: "cli".to_owned(),
                resource: "kubectl".to_owned(),
                action: "read".to_owned(),
            },
            ToolGrant {
                provider: "remote-shell".to_owned(),
                resource: "remote_shell_run".to_owned(),
                action: "write".to_owned(),
            },
        ];
        let command_plan = adapter
            .render(TaskExecutionPlanRequest {
                workflow_prompt: "Inspect input.",
                model: &model,
                tools: &command_tools,
                tool_transport_endpoint: endpoint,
                binding: &binding,
            })
            .map_err(|error| format!("render governed command plan: {error:?}"))?;
        let shell = &command_plan.command[2];
        assert!(!shell.contains("MCP_GW_RUN_TOKEN"));
        assert!(!shell.contains("https://tools.example.test/cli/run"));
        assert!(!shell.contains("https://tools.example.test/shell/run"));
        assert!(
            command_plan
                .command
                .contains(&"https://tools.example.test/cli/run".to_owned())
        );
        assert!(
            command_plan
                .command
                .contains(&"https://tools.example.test/shell/run".to_owned())
        );
        assert!(!shell.contains("MCP_GW_RUN_TOKEN_FILE"));

        let unrelated_tools = [ToolGrant {
            provider: "example-tool".to_owned(),
            resource: "repository".to_owned(),
            action: "read".to_owned(),
        }];
        let unrelated_plan = adapter
            .render(TaskExecutionPlanRequest {
                workflow_prompt: "Inspect input.",
                model: &model,
                tools: &unrelated_tools,
                tool_transport_endpoint: endpoint,
                binding: &binding,
            })
            .map_err(|error| format!("render unrelated tool plan: {error:?}"))?;
        assert!(!unrelated_plan.command[2].contains("MCP_GW_RUN_TOKEN"));
        assert_eq!(unrelated_plan.command[9], "-");
        assert_eq!(unrelated_plan.command[10], "-");

        let over_class_tools = [ToolGrant {
            provider: "cli".to_owned(),
            resource: "kubectl".to_owned(),
            action: "admin".to_owned(),
        }];
        let over_class_plan = adapter
            .render(TaskExecutionPlanRequest {
                workflow_prompt: "Inspect input.",
                model: &model,
                tools: &over_class_tools,
                tool_transport_endpoint: endpoint,
                binding: &binding,
            })
            .map_err(|error| format!("render over-class tool plan: {error:?}"))?;
        assert!(!over_class_plan.command[2].contains("MCP_GW_RUN_TOKEN"));
        assert_eq!(over_class_plan.command[9], "-");
        assert_eq!(over_class_plan.command[10], "-");
        Ok(())
    }

    #[test]
    fn exact_responses_operation_is_normalized_to_the_codex_base_url() -> Result<(), String> {
        let adapter = CodexTaskExecutionAdapter::new(
            "https://inference.example.test/v1/responses".to_owned(),
        )
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

        assert!(plan.command[6].contains("base_url = \"https://inference.example.test/v1\""));
        assert!(!plan.command[6].contains("/responses"));
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
        assert!(
            adapter
                .render(TaskExecutionPlanRequest {
                    workflow_prompt: "Inspect input.",
                    model: &ModelRef {
                        provider: "other-provider".to_owned(),
                        model: "small".to_owned(),
                    },
                    tools: &[],
                    tool_transport_endpoint: None,
                    binding: &binding,
                })
                .is_err(),
            "an unlisted provider must fail closed"
        );
        Ok(())
    }
}
