//! Deployment-owned browser onboarding defaults.

use std::collections::BTreeMap;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use steward_types::direct_package::{
    AgentRef, BrowserPackageLocator, BrowserTaskSubmission, DIRECT_TASK_DEFINITION_SCHEMA,
    DeclaredOutput, DiagnosticsRequest, DirectTaskDefinition, ExecutionLogMode, OutputKind,
    RelativePath, RuntimeSelection,
};

use crate::ExecutionBindingAdvertisement;
use crate::browser_auth::{BrowserAuthService, protect_browser_routes};
use crate::workflows::WorkflowReference;

pub const STARTER_TASK_API_VERSION: &str = "steward.onboarding-starter-task/v1";
pub const MAX_STARTER_TASK_BYTES: usize = 128 * 1024;
pub const BUILT_IN_STARTER_PROMPT: &str = "Write the single line hello world to $STEWARD_OUTPUT_DIR/out/hello.txt using your shell, for example mkdir -p \"$STEWARD_OUTPUT_DIR/out\" && printf 'hello world\\n' > \"$STEWARD_OUTPUT_DIR/out/hello.txt\". Do not create any other files, do not use the network, and do not call any MCP or GitHub tools.";
const BUILT_IN_AGENT_WITHOUT_CATALOG: &str = "codex@0.140.0";
const BUILT_IN_PACKAGE_PATH: &str = ".steward/tasks/browser-task/task-definition.json";

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StarterTaskGitExample {
    pub repository: String,
    pub revision: String,
    pub path: RelativePath,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StarterTaskSetting {
    pub task_definition: DirectTaskDefinition,
    #[schema(value_type = Object)]
    pub inputs: Value,
    pub execution_log: ExecutionLogMode,
    pub package_path: RelativePath,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<StarterTaskGitExample>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_workflow: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct StarterTaskResponse {
    pub api_version: &'static str,
    pub starter_task: StarterTaskSetting,
}

impl StarterTaskSetting {
    pub fn from_optional_json(
        configured: Option<&str>,
        agents: &[ExecutionBindingAdvertisement],
    ) -> Result<Self, String> {
        let require_advertised_agent = configured.is_some() || !agents.is_empty();
        let setting = match configured {
            Some(value) => {
                if value.len() > MAX_STARTER_TASK_BYTES {
                    return Err(format!(
                        "starter task setting exceeds {MAX_STARTER_TASK_BYTES} bytes"
                    ));
                }
                serde_json::from_str(value)
                    .map_err(|error| format!("starter task setting is invalid: {error}"))?
            }
            None => built_in_starter_task(agents)?,
        };
        setting.validate_inner(agents, require_advertised_agent)?;
        Ok(setting)
    }

    pub fn validate(&self, agents: &[ExecutionBindingAdvertisement]) -> Result<(), String> {
        self.validate_inner(agents, true)
    }

    fn validate_inner(
        &self,
        agents: &[ExecutionBindingAdvertisement],
        require_advertised_agent: bool,
    ) -> Result<(), String> {
        if self.task_definition.prompt_text.is_none() || self.task_definition.prompt.is_some() {
            return Err("starter task TaskDefinition must use promptText".to_owned());
        }
        self.task_definition
            .validate()
            .map_err(|error| format!("starter task TaskDefinition is invalid: {error}"))?;
        if require_advertised_agent
            && !agents
                .iter()
                .any(|agent| agent.agent_ref == self.task_definition.runtime.agent_ref.as_str())
        {
            return Err(format!(
                "starter task agentRef {} is not advertised by the execution catalog",
                self.task_definition.runtime.agent_ref.as_str()
            ));
        }
        validate_package_path(self.package_path.as_str())?;
        validate_presentation("title", self.title.as_deref(), 128)?;
        validate_presentation("description", self.description.as_deref(), 512)?;
        if let Some(git) = &self.git {
            BrowserPackageLocator {
                source: git.repository.clone(),
                revision: Some(git.revision.clone()),
                path: git.path.clone(),
                files: None,
            }
            .validate()
            .map_err(|error| format!("starter task Git example is invalid: {error}"))?;
        }
        if let Some(reference) = &self.published_workflow {
            WorkflowReference::parse(reference)
                .map_err(|_| "starter task publishedWorkflow is invalid".to_owned())?;
        }

        let definition_source = serde_json::to_string(&self.task_definition)
            .map_err(|error| format!("starter task TaskDefinition is invalid: {error}"))?;
        let mut files = BTreeMap::new();
        files.insert(
            self.package_path.as_str().to_owned(),
            definition_source.clone(),
        );
        BrowserTaskSubmission {
            package: BrowserPackageLocator {
                source: "inline".to_owned(),
                revision: None,
                path: self.package_path.clone(),
                files: Some(files.clone()),
            },
            envelope_digest: None,
            inputs: self.inputs.clone(),
            diagnostics: DiagnosticsRequest {
                execution_log: self.execution_log,
            },
        }
        .validate()
        .map_err(|error| format!("starter task setting is invalid: {error}"))?;
        crate::tasks::resolve_inline_package_closure(
            &self.package_path,
            &self.task_definition,
            definition_source.as_bytes(),
            &files,
        )
        .map_err(|error| format!("starter task package is invalid: {error}"))?;
        Ok(())
    }
}

fn built_in_starter_task(
    agents: &[ExecutionBindingAdvertisement],
) -> Result<StarterTaskSetting, String> {
    let agent_ref = agents
        .first()
        .map(|agent| agent.agent_ref.as_str())
        .unwrap_or(BUILT_IN_AGENT_WITHOUT_CATALOG);
    Ok(StarterTaskSetting {
        task_definition: DirectTaskDefinition {
            schema_version: DIRECT_TASK_DEFINITION_SCHEMA.to_owned(),
            name: steward_types::direct_package::Slug::parse("browser-task")?,
            version: 1,
            runtime: RuntimeSelection {
                agent_ref: AgentRef::parse(agent_ref)?,
                model: None,
            },
            prompt: None,
            prompt_text: Some(BUILT_IN_STARTER_PROMPT.to_owned()),
            skills: Vec::new(),
            workspace: Vec::new(),
            outputs: vec![DeclaredOutput {
                path: RelativePath::parse("out/hello.txt")?,
                kind: OutputKind::File,
                required: true,
            }],
            requires: None,
        },
        inputs: Value::Object(Map::new()),
        execution_log: ExecutionLogMode::Off,
        package_path: RelativePath::parse(BUILT_IN_PACKAGE_PATH)?,
        title: Some("Hello world".to_owned()),
        description: Some("Run a governed coding agent and collect its exact output.".to_owned()),
        git: Some(StarterTaskGitExample {
            repository: "https://github.com/example-org/agentic-ops.git".to_owned(),
            revision: "git:ref:main".to_owned(),
            path: RelativePath::parse("catalog/hello/task-definition.json")?,
        }),
        published_workflow: None,
    })
}

fn validate_package_path(value: &str) -> Result<(), String> {
    let Some(task_name) = value
        .strip_prefix(".steward/tasks/")
        .and_then(|value| value.strip_suffix("/task-definition.json"))
    else {
        return Err(
            "starter task packagePath must match .steward/tasks/**/task-definition.json".to_owned(),
        );
    };
    if task_name.is_empty() {
        return Err(
            "starter task packagePath must match .steward/tasks/**/task-definition.json".to_owned(),
        );
    }
    Ok(())
}

fn validate_presentation(kind: &str, value: Option<&str>, max_bytes: usize) -> Result<(), String> {
    if value.is_some_and(|value| {
        value.is_empty()
            || value.len() > max_bytes
            || value.trim() != value
            || value.chars().any(char::is_control)
    }) {
        return Err(format!(
            "starter task {kind} must be non-empty, trimmed, and at most {max_bytes} bytes"
        ));
    }
    Ok(())
}

pub fn protected_router(setting: StarterTaskSetting, browser_auth: BrowserAuthService) -> Router {
    protect_browser_routes(
        Router::new()
            .route("/app/api/v1/onboarding/starter-task", get(get_starter_task))
            .with_state(setting),
        browser_auth,
    )
}

#[utoipa::path(
    get,
    operation_id = "getStarterTask",
    path = "/app/api/v1/onboarding/starter-task",
    responses(
        (status = 200, body = StarterTaskResponse),
        (status = 401, description = "Browser session is absent or invalid")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn get_starter_task(State(setting): State<StarterTaskSetting>) -> Response {
    (
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(StarterTaskResponse {
            api_version: STARTER_TASK_API_VERSION,
            starter_task: setting,
        }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use axum::extract::State;
    use axum::http::{StatusCode, header};
    use axum::response::IntoResponse;
    use serde_json::{Value, json};

    use super::{BUILT_IN_STARTER_PROMPT, StarterTaskSetting, get_starter_task};
    use crate::ExecutionBindingAdvertisement;

    fn agents() -> Vec<ExecutionBindingAdvertisement> {
        vec![ExecutionBindingAdvertisement {
            agent_ref: "codex@0.140.0".to_owned(),
            display_name: Some("Codex 0.140.0".to_owned()),
        }]
    }

    fn configured() -> Value {
        json!({
            "taskDefinition": {
                "schemaVersion": "steward.task-definition/v2",
                "name": "hello-world",
                "version": 2,
                "runtime": {"agentRef": "codex@0.140.0"},
                "promptText": "Write hello world to out/hello.txt.",
                "outputs": [{"path": "out/hello.txt", "kind": "file", "required": true}]
            },
            "inputs": {"greeting": "hello"},
            "executionLog": "full",
            "packagePath": ".steward/tasks/hello-world/task-definition.json",
            "title": "Hello from this deployment",
            "description": "A deployment-owned starter task.",
            "git": {
                "repository": "https://github.com/example-org/agentic-ops.git",
                "revision": "git:ref:main",
                "path": "catalog/hello/task-definition.json"
            },
            "publishedWorkflow": "repo-summary@2"
        })
    }

    #[test]
    fn configured_starter_task_is_validated_and_preserved() -> Result<(), String> {
        let setting =
            StarterTaskSetting::from_optional_json(Some(&configured().to_string()), &agents())?;
        assert_eq!(setting.task_definition.name.as_str(), "hello-world");
        assert_eq!(setting.inputs["greeting"], "hello");
        assert_eq!(
            setting.package_path.as_str(),
            ".steward/tasks/hello-world/task-definition.json"
        );
        Ok(())
    }

    #[test]
    fn invalid_starter_task_boundaries_fail_with_specific_errors() -> Result<(), String> {
        let cases = [
            (
                "/taskDefinition/runtime/agentRef",
                json!("unknown@1.0.0"),
                "is not advertised by the execution catalog",
            ),
            (
                "/taskDefinition/promptText",
                json!("x".repeat(32 * 1024 + 1)),
                "promptText must be non-empty and at most 32 KiB",
            ),
            (
                "/taskDefinition/outputs/0/path",
                json!("elsewhere/hello.txt"),
                "declared outputs must remain beneath the out directory",
            ),
            (
                "/packagePath",
                json!("catalog/hello/task-definition.json"),
                "packagePath must match .steward/tasks/**/task-definition.json",
            ),
        ];
        for (pointer, replacement, expected) in cases {
            let mut value = configured();
            *value
                .pointer_mut(pointer)
                .ok_or_else(|| format!("missing test pointer {pointer}"))? = replacement;
            let error =
                match StarterTaskSetting::from_optional_json(Some(&value.to_string()), &agents()) {
                    Err(error) => error,
                    Ok(_) => return Err(format!("invalid starter setting {pointer} was accepted")),
                };
            assert!(error.contains(expected), "unexpected error: {error}");
        }
        Ok(())
    }

    #[test]
    fn invalid_inputs_fail_at_the_existing_browser_limit() -> Result<(), String> {
        let mut value = configured();
        value["inputs"] = json!({"value": "x".repeat(16 * 1024)});
        let error =
            match StarterTaskSetting::from_optional_json(Some(&value.to_string()), &agents()) {
                Err(error) => error,
                Ok(_) => return Err("oversized starter task inputs were accepted".to_owned()),
            };
        assert!(error.contains("browser Task inputs exceed the size limit"));
        Ok(())
    }

    #[tokio::test]
    async fn unset_setting_serves_corrected_built_in_default_without_caching() -> Result<(), String>
    {
        let setting = StarterTaskSetting::from_optional_json(None, &agents())?;
        assert_eq!(
            setting.task_definition.prompt_text.as_deref(),
            Some(BUILT_IN_STARTER_PROMPT)
        );
        assert_eq!(
            setting.task_definition.outputs[0].path.as_str(),
            "out/hello.txt"
        );
        let response = get_starter_task(State(setting)).await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        Ok(())
    }

    #[test]
    fn unset_setting_keeps_a_staged_browser_installation_available_without_agents()
    -> Result<(), String> {
        let setting = StarterTaskSetting::from_optional_json(None, &[])?;
        assert_eq!(
            setting.task_definition.runtime.agent_ref.as_str(),
            "codex@0.140.0"
        );
        Ok(())
    }
}
