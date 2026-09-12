//! Claude Code command rendering for immutable disposable Task execution.

use steward_ports::{PortError, TaskExecutionAdapter, TaskExecutionPlan, TaskExecutionPlanRequest};

pub const CLAUDE_CODE_V1_ADAPTER: &str = "claude-code-v1";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaudeCodeTaskExecutionAdapter {
    inference_endpoint: String,
}

impl ClaudeCodeTaskExecutionAdapter {
    pub fn new(inference_endpoint: String) -> Result<Self, PortError> {
        let inference_endpoint = validate_endpoint(
            &inference_endpoint,
            "Claude Code inference endpoint must be an exact HTTP(S) URL",
        )?;
        Ok(Self { inference_endpoint })
    }

    fn render_with_config_dir(
        &self,
        request: TaskExecutionPlanRequest<'_>,
        config_dir: &str,
    ) -> Result<TaskExecutionPlan, PortError> {
        if request.model.provider != "anthropic" {
            return Err(PortError::Rejected {
                reason: "Claude Code execution requires an approved Anthropic model".to_owned(),
            });
        }
        if !request.tools.is_empty() && request.tool_transport_endpoint.is_none() {
            return Err(PortError::Rejected {
                reason: "tool-bearing Claude Code execution requires a governed tool endpoint"
                    .to_owned(),
            });
        }
        let tool_transport_endpoint = if request.tools.is_empty() {
            None
        } else {
            request.tool_transport_endpoint
        };
        let mcp_config = match tool_transport_endpoint {
            Some(endpoint) => {
                let endpoint = validate_endpoint(
                    endpoint,
                    "Claude Code tool endpoint must be an exact HTTP(S) URL",
                )?;
                serde_json::to_string(&serde_json::json!({
                    "mcpServers": {
                        "steward": {
                            "type": "http",
                            "url": endpoint,
                            "headers": {
                                "Authorization": "Bearer ${STEWARD_MCP_GW_BEARER_TOKEN}"
                            }
                        }
                    }
                }))
                .map_err(|error| PortError::Failed {
                    reason: format!("render Claude Code tool endpoint: {error}"),
                })?
            }
            None => String::new(),
        };
        let tool_setup = if tool_transport_endpoint.is_some() {
            concat!(
                "printf '%s' \"$mcp_config\" > \"$CLAUDE_CONFIG_DIR/mcp.json\"; ",
                "export STEWARD_MCP_GW_BEARER_TOKEN=openshell-token-grant-placeholder; ",
                "set -- --strict-mcp-config --mcp-config \"$CLAUDE_CONFIG_DIR/mcp.json\"; ",
            )
        } else {
            "set --; "
        };
        let shell_command = format!(
            concat!(
                "set -eu; umask 077; ",
                "test \"$#\" -ge 8; ",
                "prompt=$1; model=$2; inference_endpoint=$3; mcp_config=$4; ",
                "config_dir=$5; executable=$6; expected_version=$7; shift 7; ",
                "command -v id >/dev/null 2>&1; test \"$(id -u)\" -ne 0; ",
                "test \"$(\"$executable\" \"$@\")\" = \"$expected_version\"; ",
                "export CLAUDE_CONFIG_DIR=\"$config_dir\"; ",
                "unset CLAUDE_CODE_USE_BEDROCK CLAUDE_CODE_USE_VERTEX ",
                "CLAUDE_CODE_USE_FOUNDRY CLAUDE_CODE_USE_ANTHROPIC_AWS ",
                "ANTHROPIC_AUTH_TOKEN CLAUDE_CODE_OAUTH_TOKEN ",
                "CLAUDE_CODE_OAUTH_REFRESH_TOKEN CLAUDE_CODE_OAUTH_SCOPES ",
                "ANTHROPIC_CUSTOM_HEADERS; ",
                "export ANTHROPIC_BASE_URL=\"$inference_endpoint\"; ",
                "export ANTHROPIC_API_KEY=openshell-token-grant-placeholder; ",
                "export DISABLE_UPDATES=1; ",
                "export CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1; ",
                "export CLAUDE_CODE_SKIP_PROMPT_HISTORY=1; ",
                "mkdir -p \"$CLAUDE_CONFIG_DIR\" \"$STEWARD_OUTPUT_DIR/out\"; ",
                "test ! -e out; ln -s \"$STEWARD_OUTPUT_DIR/out\" out; ",
                "{}",
                "status_file=\"$CLAUDE_CONFIG_DIR/exit-status\"; ",
                "( set +e; ",
                "\"$executable\" --bare --print --model \"$model\" --output-format text ",
                "--permission-mode bypassPermissions --no-session-persistence --no-chrome ",
                "--disable-slash-commands \"$@\" -- \"$prompt\"; status=$?; ",
                "printf '%s' \"$status\" > \"$status_file\" ) ",
                "| tee \"$STEWARD_OUTPUT_DIR/result.txt\"; ",
                "status=$(cat \"$status_file\"); exit \"$status\"",
            ),
            tool_setup,
        );
        let mut command = vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            shell_command,
            "steward-workflow".to_owned(),
            request.workflow_prompt.to_owned(),
            request.model.model.clone(),
            self.inference_endpoint.clone(),
            mcp_config,
            config_dir.to_owned(),
            request.binding.executable.clone(),
            request.binding.version_probe.expected_stdout.clone(),
        ];
        command.extend(request.binding.version_probe.arguments.iter().cloned());
        Ok(TaskExecutionPlan { command })
    }
}

fn validate_endpoint(value: &str, reason: &'static str) -> Result<String, PortError> {
    if value.trim() != value || value.chars().any(char::is_control) {
        return Err(PortError::Rejected {
            reason: reason.to_owned(),
        });
    }
    let endpoint = reqwest::Url::parse(value).map_err(|_| PortError::Rejected {
        reason: reason.to_owned(),
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
            reason: reason.to_owned(),
        });
    }
    Ok(endpoint.to_string().trim_end_matches('/').to_owned())
}

impl TaskExecutionAdapter for ClaudeCodeTaskExecutionAdapter {
    fn contract(&self) -> &'static str {
        CLAUDE_CODE_V1_ADAPTER
    }

    fn render(
        &self,
        request: TaskExecutionPlanRequest<'_>,
    ) -> Result<TaskExecutionPlan, PortError> {
        self.render_with_config_dir(request, "/sandbox/steward-claude")
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        process::Command,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use steward_ports::{TaskExecutionAdapter, TaskExecutionPlanRequest};
    use steward_types::{
        DisposableExecutionBinding, ExecutionProviderProfile, ExecutionProviderProfiles,
        ExecutionVersionProbe, ModelRef, ToolGrant,
    };

    use super::{CLAUDE_CODE_V1_ADAPTER, ClaudeCodeTaskExecutionAdapter};

    #[cfg(unix)]
    static TEMP_DIR_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

    #[cfg(unix)]
    struct TempDir(PathBuf);

    #[cfg(unix)]
    impl TempDir {
        fn new() -> Result<Self, String> {
            let sequence = TEMP_DIR_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "steward-claude-adapter-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path)
                .map_err(|error| format!("create temporary adapter directory: {error}"))?;
            Ok(Self(path))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    #[cfg(unix)]
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn binding() -> DisposableExecutionBinding {
        DisposableExecutionBinding {
            schema_version: "steward/task-execution-binding/v1".to_owned(),
            binding_id: format!("sha256:{}", "a".repeat(64)),
            binding_digest: format!("sha256:{}", "a".repeat(64)),
            agent_ref: "claude-code@2.1.222".to_owned(),
            display_name: Some("Claude Code".to_owned()),
            adapter: CLAUDE_CODE_V1_ADAPTER.to_owned(),
            image: format!(
                "registry.example.test/agents/claude-code@sha256:{}",
                "b".repeat(64)
            ),
            executable: "/usr/local/bin/claude".to_owned(),
            version_probe: ExecutionVersionProbe {
                arguments: vec!["--version".to_owned()],
                expected_stdout: "2.1.222 (Claude Code)".to_owned(),
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

    fn model() -> ModelRef {
        ModelRef {
            provider: "anthropic".to_owned(),
            model: "claude-sonnet-example".to_owned(),
        }
    }

    #[test]
    fn renders_isolated_unattended_tool_bearing_command_without_secret_material()
    -> Result<(), String> {
        let adapter =
            ClaudeCodeTaskExecutionAdapter::new("http://inference.example.test:4000".to_owned())
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
                model: &model(),
                tools: &tools,
                tool_transport_endpoint: Some("https://tools.example.test/mcp"),
                binding: &binding,
            })
            .map_err(|error| format!("render adapter plan: {error:?}"))?;

        let shell = &plan.command[2];
        let mcp_config = &plan.command[7];
        assert!(shell.contains("ANTHROPIC_API_KEY=openshell-token-grant-placeholder"));
        assert!(shell.contains("CLAUDE_CONFIG_DIR=\"$config_dir\""));
        assert!(shell.contains("test \"$(id -u)\" -ne 0"));
        for inherited_selector in [
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
            "CLAUDE_CODE_USE_ANTHROPIC_AWS",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_REFRESH_TOKEN",
            "CLAUDE_CODE_OAUTH_SCOPES",
            "ANTHROPIC_CUSTOM_HEADERS",
        ] {
            assert!(shell.contains(inherited_selector));
        }
        assert!(shell.contains("--bare --print"));
        assert!(shell.contains("--permission-mode bypassPermissions"));
        assert!(shell.contains("--no-session-persistence"));
        assert!(shell.contains("--disable-slash-commands"));
        assert!(shell.contains("--strict-mcp-config --mcp-config"));
        assert!(shell.contains("| tee \"$STEWARD_OUTPUT_DIR/result.txt\""));
        assert!(shell.contains("status=$(cat \"$status_file\")"));
        assert!(shell.contains("exit \"$status\""));
        assert!(shell.contains("ln -s \"$STEWARD_OUTPUT_DIR/out\" out"));
        assert_eq!(plan.command[4], "Review the repository state.");
        assert_eq!(plan.command[5], "claude-sonnet-example");
        assert_eq!(plan.command[6], "http://inference.example.test:4000");
        assert_eq!(plan.command[8], "/sandbox/steward-claude");
        assert_eq!(plan.command[9], binding.executable);
        assert_eq!(plan.command[10], binding.version_probe.expected_stdout);
        assert_eq!(plan.command[11], "--version");
        assert!(mcp_config.contains("https://tools.example.test/mcp"));
        assert!(mcp_config.contains("Bearer ${STEWARD_MCP_GW_BEARER_TOKEN}"));
        assert!(!mcp_config.contains("openshell-token-grant-placeholder"));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn shell_preserves_literal_prompt_output_diagnostics_and_nonzero_status() -> Result<(), String>
    {
        let temp_dir = TempDir::new()?;
        let executable = temp_dir.path().join("fake claude");
        fs::write(
            &executable,
            concat!(
                "#!/bin/sh\n",
                "if [ \"$1\" = --version ]; then\n",
                "  printf '%s\\n' '2.1.222 (Claude Code)'\n",
                "  exit 0\n",
                "fi\n",
                "test -z \"${CLAUDE_CODE_USE_BEDROCK+x}\" || exit 91\n",
                "test -z \"${CLAUDE_CODE_USE_VERTEX+x}\" || exit 91\n",
                "test -z \"${CLAUDE_CODE_USE_FOUNDRY+x}\" || exit 91\n",
                "test -z \"${CLAUDE_CODE_USE_ANTHROPIC_AWS+x}\" || exit 91\n",
                "test -z \"${ANTHROPIC_AUTH_TOKEN+x}\" || exit 91\n",
                "test -z \"${CLAUDE_CODE_OAUTH_TOKEN+x}\" || exit 91\n",
                "test \"$ANTHROPIC_BASE_URL\" = http://inference.example.test:4000 || exit 92\n",
                "test \"$ANTHROPIC_API_KEY\" = openshell-token-grant-placeholder || exit 93\n",
                "for argument do prompt=$argument; done\n",
                "printf 'stdout:%s\\n' \"$prompt\"\n",
                "printf 'stderr:%s\\n' \"$prompt\" >&2\n",
                "exit 23\n",
            ),
        )
        .map_err(|error| format!("write fake Claude executable: {error}"))?;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("make fake Claude executable runnable: {error}"))?;

        let marker = temp_dir.path().join("must-not-exist");
        let prompt = format!(
            "literal $(touch {}) ; `touch {}` 'single' \"double\"\nnext line",
            marker.display(),
            marker.display()
        );
        let output_dir = temp_dir.path().join("output");
        let config_dir = temp_dir.path().join("config with spaces");
        fs::create_dir(&output_dir)
            .map_err(|error| format!("create adapter output directory: {error}"))?;
        let mut test_binding = binding();
        test_binding.executable = executable.to_string_lossy().into_owned();
        let adapter =
            ClaudeCodeTaskExecutionAdapter::new("http://inference.example.test:4000".to_owned())
                .map_err(|error| format!("configure adapter: {error:?}"))?;
        let test_model = model();
        let plan = adapter
            .render_with_config_dir(
                TaskExecutionPlanRequest {
                    workflow_prompt: &prompt,
                    model: &test_model,
                    tools: &[],
                    tool_transport_endpoint: None,
                    binding: &test_binding,
                },
                &config_dir.to_string_lossy(),
            )
            .map_err(|error| format!("render adapter plan: {error:?}"))?;

        let output = Command::new(&plan.command[0])
            .args(&plan.command[1..])
            .current_dir(temp_dir.path())
            .env("STEWARD_OUTPUT_DIR", &output_dir)
            .env("CLAUDE_CODE_USE_BEDROCK", "1")
            .env("CLAUDE_CODE_USE_VERTEX", "1")
            .env("CLAUDE_CODE_USE_FOUNDRY", "1")
            .env("CLAUDE_CODE_USE_ANTHROPIC_AWS", "1")
            .env("ANTHROPIC_AUTH_TOKEN", "untrusted")
            .env("CLAUDE_CODE_OAUTH_TOKEN", "untrusted")
            .output()
            .map_err(|error| format!("execute rendered adapter plan: {error}"))?;

        assert_eq!(output.status.code(), Some(23));
        let expected_stdout = format!("stdout:{prompt}\n");
        let expected_stderr = format!("stderr:{prompt}\n");
        assert_eq!(output.stdout, expected_stdout.as_bytes());
        assert_eq!(output.stderr, expected_stderr.as_bytes());
        assert_eq!(
            fs::read(output_dir.join("result.txt"))
                .map_err(|error| format!("read result artifact: {error}"))?,
            expected_stdout.as_bytes()
        );
        assert!(!marker.exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn shell_rejects_root_before_the_version_probe_or_agent_launch() -> Result<(), String> {
        let temp_dir = TempDir::new()?;
        let bin_dir = temp_dir.path().join("bin");
        let output_dir = temp_dir.path().join("output");
        fs::create_dir(&bin_dir)
            .map_err(|error| format!("create fake binary directory: {error}"))?;
        fs::create_dir(&output_dir)
            .map_err(|error| format!("create adapter output directory: {error}"))?;
        let fake_id = bin_dir.join("id");
        fs::write(&fake_id, "#!/bin/sh\nprintf '0\\n'\n")
            .map_err(|error| format!("write fake id executable: {error}"))?;
        fs::set_permissions(&fake_id, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("make fake id executable runnable: {error}"))?;

        let marker = temp_dir.path().join("agent-was-invoked");
        let executable = temp_dir.path().join("fake-claude");
        fs::write(
            &executable,
            format!(
                "#!/bin/sh\ntouch '{}'\nprintf '%s\\n' '2.1.222 (Claude Code)'\n",
                marker.display()
            ),
        )
        .map_err(|error| format!("write fake Claude executable: {error}"))?;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("make fake Claude executable runnable: {error}"))?;

        let mut test_binding = binding();
        test_binding.executable = executable.to_string_lossy().into_owned();
        let adapter =
            ClaudeCodeTaskExecutionAdapter::new("http://inference.example.test:4000".to_owned())
                .map_err(|error| format!("configure adapter: {error:?}"))?;
        let test_model = model();
        let config_dir = temp_dir.path().join("config");
        let plan = adapter
            .render_with_config_dir(
                TaskExecutionPlanRequest {
                    workflow_prompt: "must not run",
                    model: &test_model,
                    tools: &[],
                    tool_transport_endpoint: None,
                    binding: &test_binding,
                },
                &config_dir.to_string_lossy(),
            )
            .map_err(|error| format!("render adapter plan: {error:?}"))?;
        let inherited_path = std::env::var_os("PATH").unwrap_or_default();
        let mut path_entries = vec![bin_dir];
        path_entries.extend(std::env::split_paths(&inherited_path));
        let path = std::env::join_paths(path_entries)
            .map_err(|error| format!("construct fake command path: {error}"))?;
        let output = Command::new(&plan.command[0])
            .args(&plan.command[1..])
            .current_dir(temp_dir.path())
            .env("PATH", path)
            .env("STEWARD_OUTPUT_DIR", &output_dir)
            .output()
            .map_err(|error| format!("execute rendered adapter plan as fake root: {error}"))?;

        assert!(!output.status.success());
        assert!(!marker.exists());
        assert!(!output_dir.join("result.txt").exists());
        Ok(())
    }

    #[test]
    fn tool_less_plan_contains_no_mcp_server_or_tool_token() -> Result<(), String> {
        let adapter =
            ClaudeCodeTaskExecutionAdapter::new("https://inference.example.test".to_owned())
                .map_err(|error| format!("configure adapter: {error:?}"))?;
        let binding = binding();
        let plan = adapter
            .render(TaskExecutionPlanRequest {
                workflow_prompt: "Inspect input.",
                model: &model(),
                tools: &[],
                tool_transport_endpoint: Some("https://unused-tools.example.test/mcp"),
                binding: &binding,
            })
            .map_err(|error| format!("render adapter plan: {error:?}"))?;
        assert!(plan.command[7].is_empty());
        assert!(!plan.command[7].contains("mcpServers"));
        assert!(!plan.command[2].contains("STEWARD_MCP_GW_BEARER_TOKEN"));
        Ok(())
    }

    #[test]
    fn rejects_missing_or_malformed_governed_endpoints_and_non_anthropic_models()
    -> Result<(), String> {
        for endpoint in [
            " https://inference.example.test",
            "ftp://inference.example.test",
            "https://alice@inference.example.test",
            "https://inference.example.test?target=other",
            "https://inference.example.test#fragment",
        ] {
            assert!(ClaudeCodeTaskExecutionAdapter::new(endpoint.to_owned()).is_err());
        }
        let adapter =
            ClaudeCodeTaskExecutionAdapter::new("https://inference.example.test".to_owned())
                .map_err(|error| format!("configure adapter: {error:?}"))?;
        let binding = binding();
        let tools = [ToolGrant {
            provider: "example-tool".to_owned(),
            resource: "repository".to_owned(),
            action: "read".to_owned(),
        }];
        for endpoint in [
            None,
            Some("https://alice@tools.example.test/mcp"),
            Some("file:///tmp/mcp"),
        ] {
            assert!(
                adapter
                    .render(TaskExecutionPlanRequest {
                        workflow_prompt: "Inspect input.",
                        model: &model(),
                        tools: &tools,
                        tool_transport_endpoint: endpoint,
                        binding: &binding,
                    })
                    .is_err()
            );
        }
        assert!(
            adapter
                .render(TaskExecutionPlanRequest {
                    workflow_prompt: "Inspect input.",
                    model: &ModelRef {
                        provider: "example-provider".to_owned(),
                        model: "claude-sonnet-example".to_owned(),
                    },
                    tools: &[],
                    tool_transport_endpoint: None,
                    binding: &binding,
                })
                .is_err()
        );
        Ok(())
    }
}
