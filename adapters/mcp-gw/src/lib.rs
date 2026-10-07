//! MCP-GW adapter for the one-shot, provider-attached Connections bridge.
//!
//! The bridge sends OpenShell's documented bearer placeholder only. The sandbox
//! supervisor replaces it at the governed egress boundary; this adapter never
//! accepts or reads a credential, provider origin, or caller identity.

use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use reqwest::header::{ACCEPT, AUTHORIZATION};
use reqwest::{Client, Method, StatusCode, Url};
use serde_json::{Map, Value, json};
use steward_ports::PortError;

pub const IMPLEMENTED_PORTS: [&str; 0] = [];
const OPEN_SHELL_BEARER_PLACEHOLDER: &str = "openshell-token-grant-placeholder";
const MAX_RESPONSE_BYTES: usize = 32 * 1024;
const PROVIDER_TRANSPORT_READY_TIMEOUT: Duration = Duration::from_secs(12);
const PROVIDER_TRANSPORT_RETRY_INTERVAL: Duration = Duration::from_millis(250);
const DIRECT_STATUS_TIMEOUT: Duration = Duration::from_secs(1);
const CONNECTION_STATUS_V2_MEDIA_TYPE: &str = "application/vnd.apelogic.connection-status.v2+json";
pub const MAX_GATEWAY_FAILURE_DETAIL_BYTES: usize = 200;
const MAX_GATEWAY_FAILURE_CODE_BYTES: usize = 100;
const BRIDGE_GATEWAY_HTTP_PREFIX: &str = "steward-connections-bridge: bridge MCP-GW returned HTTP ";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GithubBridgeFailureDiagnostic {
    pub status: u16,
    pub code: Option<String>,
    pub reason: Option<String>,
}

impl GithubBridgeFailureDiagnostic {
    pub fn to_value(&self) -> Value {
        let mut value = Map::new();
        value.insert("upstreamStatus".to_owned(), Value::from(self.status));
        if let Some(code) = &self.code {
            value.insert("code".to_owned(), Value::String(code.clone()));
        }
        if let Some(reason) = &self.reason {
            value.insert("reason".to_owned(), Value::String(reason.clone()));
        }
        Value::Object(value)
    }

    pub fn from_value(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        if !object
            .keys()
            .all(|key| matches!(key.as_str(), "upstreamStatus" | "code" | "reason"))
        {
            return None;
        }
        let status = u16::try_from(object.get("upstreamStatus")?.as_u64()?).ok()?;
        if !(100..=599).contains(&status) {
            return None;
        }
        let code = match object.get("code") {
            None => None,
            Some(value) => {
                let code = value.as_str()?;
                let sanitized = sanitized_gateway_failure_code(code)?;
                Some((sanitized == code).then_some(sanitized)?)
            }
        };
        let reason = match object.get("reason") {
            None => None,
            Some(value) => {
                let reason = value.as_str()?;
                let sanitized = sanitized_gateway_failure_scalar(reason)?;
                Some((sanitized == reason).then_some(sanitized)?)
            }
        };
        Some(Self {
            status,
            code,
            reason,
        })
    }
}

/// Parse only the fixed, sanitized failure line emitted by the Connections bridge.
/// Arbitrary task stderr is never treated as an operator-facing gateway diagnostic.
pub fn github_bridge_failure_diagnostic(stderr: &[u8]) -> Option<GithubBridgeFailureDiagnostic> {
    let stderr = std::str::from_utf8(stderr)
        .ok()?
        .trim_end_matches(['\r', '\n']);
    let remainder = stderr.strip_prefix(BRIDGE_GATEWAY_HTTP_PREFIX)?;
    let status_end = remainder
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(remainder.len());
    let status = remainder[..status_end].parse::<u16>().ok()?;
    if !(100..=599).contains(&status) {
        return None;
    }
    let mut suffix = &remainder[status_end..];
    let code = if suffix.starts_with(" [code=") {
        let code_end = suffix.find(']')?;
        let code = suffix[7..code_end].to_owned();
        let sanitized = sanitized_gateway_failure_code(&code)?;
        if sanitized != code {
            return None;
        }
        suffix = &suffix[code_end + 1..];
        Some(code)
    } else {
        None
    };
    let reason = if suffix.is_empty() {
        None
    } else {
        let reason = suffix.strip_prefix(" (")?.strip_suffix(')')?;
        let sanitized = sanitized_gateway_failure_scalar(reason)?;
        if sanitized != reason {
            return None;
        }
        Some(sanitized)
    };
    Some(GithubBridgeFailureDiagnostic {
        status,
        code,
        reason,
    })
}
const LEGACY_STATUS_PATH: &str = "/oauth/github/status";
const LIFECYCLE_STATUS_PATH: &str = "/connections/github/status";
const START_PATH: &str = "/oauth/github/start";
const DISCONNECT_PATH: &str = "/oauth/github/disconnect";
const MCP_PATH: &str = "/mcp";
const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
const RERUN_REQUEST_ID: &str = "steward-github-rerun";
const MAX_WORKFLOW_BYTES: usize = 256 * 1024;
const MAX_PUBLISHED_FILE_BYTES: usize = 512 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GithubBridgeOperation {
    Status,
    Start,
    Disconnect,
    Rerun,
    Repositories,
    Workflow,
    RunStatus,
    Dispatch,
    Publish,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GatewayContract {
    LegacyV032,
    LifecycleV049,
}

impl GatewayContract {
    fn parse(value: &str) -> Result<Self, PortError> {
        match value {
            "0.3.2" => Ok(Self::LegacyV032),
            "0.4.9" => Ok(Self::LifecycleV049),
            _ => Err(rejected(
                "Connections bridge gateway version is not allowlisted",
            )),
        }
    }
}

impl GithubBridgeOperation {
    fn expected_status(self) -> StatusCode {
        match self {
            Self::Status
            | Self::Start
            | Self::Rerun
            | Self::Repositories
            | Self::Workflow
            | Self::RunStatus
            | Self::Dispatch
            | Self::Publish => StatusCode::OK,
            Self::Disconnect => StatusCode::NO_CONTENT,
        }
    }

    pub fn parse(value: &str) -> Result<Self, PortError> {
        match value {
            "github.status" => Ok(Self::Status),
            "github.start" => Ok(Self::Start),
            "github.disconnect" => Ok(Self::Disconnect),
            "github.rerun" => Ok(Self::Rerun),
            "github.repositories" => Ok(Self::Repositories),
            "github.workflow" => Ok(Self::Workflow),
            "github.run-status" => Ok(Self::RunStatus),
            "github.dispatch" => Ok(Self::Dispatch),
            "github.publish" => Ok(Self::Publish),
            _ => Err(rejected("Connections bridge operation is not allowlisted")),
        }
    }

    fn method_and_path(self, contract: GatewayContract) -> (Method, &'static str) {
        match self {
            Self::Status => (
                Method::GET,
                match contract {
                    GatewayContract::LegacyV032 => LEGACY_STATUS_PATH,
                    GatewayContract::LifecycleV049 => LIFECYCLE_STATUS_PATH,
                },
            ),
            Self::Start => (Method::POST, START_PATH),
            Self::Disconnect => (Method::POST, DISCONNECT_PATH),
            Self::Rerun
            | Self::Repositories
            | Self::Workflow
            | Self::RunStatus
            | Self::Dispatch
            | Self::Publish => (Method::POST, MCP_PATH),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GithubPublishedFile {
    pub path: String,
    pub content: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GithubBridgeRequest {
    Empty,
    Start {
        redirect_after: String,
    },
    Rerun {
        owner: String,
        repo: String,
        run_id: u64,
    },
    Repositories {
        query: String,
        page: u32,
        per_page: u32,
    },
    Workflow {
        owner: String,
        repo: String,
        path: String,
        git_ref: String,
        expected_content: String,
    },
    RunStatus {
        owner: String,
        repo: String,
        run_id: u64,
    },
    Dispatch {
        owner: String,
        repo: String,
        workflow_id: String,
        git_ref: String,
        inputs: Map<String, Value>,
        expected_content: String,
    },
    Publish {
        owner: String,
        repo: String,
        base_branch: String,
        branch: String,
        title: String,
        body: String,
        files: Vec<GithubPublishedFile>,
        resume_owned_branch: bool,
    },
}

impl GithubBridgeRequest {
    pub fn parse(operation: GithubBridgeOperation, input: &[u8]) -> Result<Self, PortError> {
        let object = serde_json::from_slice::<Value>(input)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .ok_or_else(|| rejected("Connections bridge request must be one JSON object"))?;
        match operation {
            GithubBridgeOperation::Status | GithubBridgeOperation::Disconnect => {
                if object.is_empty() {
                    Ok(Self::Empty)
                } else {
                    Err(rejected(
                        "Connections bridge operation does not accept request fields",
                    ))
                }
            }
            GithubBridgeOperation::Start => {
                let redirect_after = exact_string_field(&object, "redirectAfter")?;
                validate_redirect_after(&redirect_after)?;
                Ok(Self::Start { redirect_after })
            }
            GithubBridgeOperation::Rerun => {
                if object.len() != 3 {
                    return Err(rejected("GitHub rerun request has unexpected fields"));
                }
                let owner = object
                    .get("owner")
                    .and_then(Value::as_str)
                    .filter(|value| valid_repository_component(value, 39))
                    .map(str::to_owned)
                    .ok_or_else(|| rejected("GitHub rerun owner is invalid"))?;
                let repo = object
                    .get("repo")
                    .and_then(Value::as_str)
                    .filter(|value| valid_repository_component(value, 100))
                    .map(str::to_owned)
                    .ok_or_else(|| rejected("GitHub rerun repository is invalid"))?;
                let run_id = object
                    .get("runId")
                    .and_then(Value::as_u64)
                    .filter(|value| *value > 0)
                    .ok_or_else(|| rejected("GitHub rerun run ID is invalid"))?;
                Ok(Self::Rerun {
                    owner,
                    repo,
                    run_id,
                })
            }
            GithubBridgeOperation::Repositories => {
                require_exact_fields(&object, &["query", "page", "perPage"])?;
                let query = object
                    .get("query")
                    .and_then(Value::as_str)
                    .filter(|value| value.len() <= 200)
                    .map(str::to_owned)
                    .ok_or_else(|| rejected("GitHub repository query is invalid"))?;
                if query.bytes().any(|byte| byte.is_ascii_control()) {
                    return Err(rejected("GitHub repository query is invalid"));
                }
                let page = bounded_u32_field(&object, "page", 1, u32::MAX)?;
                let per_page = bounded_u32_field(&object, "perPage", 1, 100)?;
                Ok(Self::Repositories {
                    query,
                    page,
                    per_page,
                })
            }
            GithubBridgeOperation::Workflow => {
                require_exact_fields(
                    &object,
                    &["owner", "repo", "path", "ref", "expectedContent"],
                )?;
                let (owner, repo) = repository_fields(&object)?;
                // The same read also checks a legacy root package file on the base branch
                // before publication, so it never overwrites different existing content.
                let path = object
                    .get("path")
                    .and_then(Value::as_str)
                    .filter(|value| legacy_root_package_path(value))
                    .map_or_else(
                        || workflow_path_field(&object, "path"),
                        |value| Ok(value.to_owned()),
                    )?;
                let git_ref = git_ref_field(&object, "ref")?;
                let expected_content =
                    bounded_string_field(&object, "expectedContent", MAX_WORKFLOW_BYTES)?;
                Ok(Self::Workflow {
                    owner,
                    repo,
                    path,
                    git_ref,
                    expected_content,
                })
            }
            GithubBridgeOperation::RunStatus => {
                require_exact_fields(&object, &["owner", "repo", "runId"])?;
                let (owner, repo) = repository_fields(&object)?;
                let run_id = positive_u64_field(&object, "runId")?;
                Ok(Self::RunStatus {
                    owner,
                    repo,
                    run_id,
                })
            }
            GithubBridgeOperation::Dispatch => {
                require_exact_fields(
                    &object,
                    &[
                        "owner",
                        "repo",
                        "workflowId",
                        "ref",
                        "inputs",
                        "expectedContent",
                    ],
                )?;
                let (owner, repo) = repository_fields(&object)?;
                let workflow_id = workflow_path_field(&object, "workflowId")?;
                let git_ref = git_ref_field(&object, "ref")?;
                let inputs = object
                    .get("inputs")
                    .and_then(Value::as_object)
                    .filter(|inputs| inputs.len() <= 20)
                    .cloned()
                    .ok_or_else(|| rejected("GitHub dispatch inputs are invalid"))?;
                if inputs.iter().any(|(name, value)| {
                    !valid_input_name(name)
                        || value.as_str().is_none_or(|value| value.len() > 8 * 1024)
                }) {
                    return Err(rejected("GitHub dispatch inputs are invalid"));
                }
                let expected_content =
                    bounded_string_field(&object, "expectedContent", MAX_WORKFLOW_BYTES)?;
                Ok(Self::Dispatch {
                    owner,
                    repo,
                    workflow_id,
                    git_ref,
                    inputs,
                    expected_content,
                })
            }
            GithubBridgeOperation::Publish => {
                require_exact_fields(
                    &object,
                    &[
                        "owner",
                        "repo",
                        "baseBranch",
                        "branch",
                        "title",
                        "body",
                        "files",
                        "resumeOwnedBranch",
                    ],
                )?;
                let (owner, repo) = repository_fields(&object)?;
                let base_branch = git_ref_field(&object, "baseBranch")?;
                let branch = git_ref_field(&object, "branch")?;
                if !branch.starts_with("steward/task-") || branch == base_branch {
                    return Err(rejected("GitHub publication branch is invalid"));
                }
                let title = bounded_string_field(&object, "title", 200)?;
                let body = bounded_string_field(&object, "body", 4 * 1024)?;
                let files = published_files_field(&object)?;
                let resume_owned_branch = object
                    .get("resumeOwnedBranch")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| rejected("GitHub publication resume proof is invalid"))?;
                Ok(Self::Publish {
                    owner,
                    repo,
                    base_branch,
                    branch,
                    title,
                    body,
                    files,
                    resume_owned_branch,
                })
            }
        }
    }

    fn body(&self) -> Option<Value> {
        match self {
            Self::Empty => None,
            Self::Start { redirect_after } => Some(json!({"redirectAfter": redirect_after})),
            Self::Rerun {
                owner,
                repo,
                run_id,
            } => Some(json!({
                "jsonrpc": "2.0",
                "id": RERUN_REQUEST_ID,
                "method": "tools/call",
                "params": {
                    "name": "actions_run_trigger",
                    "arguments": {
                        "method": "rerun_workflow_run",
                        "owner": owner,
                        "repo": repo,
                        "run_id": run_id,
                    }
                }
            })),
            Self::Repositories { .. }
            | Self::Workflow { .. }
            | Self::RunStatus { .. }
            | Self::Dispatch { .. }
            | Self::Publish { .. } => None,
        }
    }
}

fn require_exact_fields(object: &Map<String, Value>, fields: &[&str]) -> Result<(), PortError> {
    if object.len() == fields.len() && fields.iter().all(|field| object.contains_key(*field)) {
        Ok(())
    } else {
        Err(rejected("GitHub operation request has unexpected fields"))
    }
}

fn bounded_string_field(
    object: &Map<String, Value>,
    field: &str,
    maximum: usize,
) -> Result<String, PortError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= maximum)
        .map(str::to_owned)
        .ok_or_else(|| rejected("GitHub operation string field is invalid"))
}

fn bounded_u32_field(
    object: &Map<String, Value>,
    field: &str,
    minimum: u32,
    maximum: u32,
) -> Result<u32, PortError> {
    object
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| (*value >= minimum) && (*value <= maximum))
        .ok_or_else(|| rejected("GitHub operation integer field is invalid"))
}

fn positive_u64_field(object: &Map<String, Value>, field: &str) -> Result<u64, PortError> {
    object
        .get(field)
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .ok_or_else(|| rejected("GitHub operation identifier is invalid"))
}

fn repository_fields(object: &Map<String, Value>) -> Result<(String, String), PortError> {
    let owner = object
        .get("owner")
        .and_then(Value::as_str)
        .filter(|value| valid_repository_component(value, 39))
        .map(str::to_owned)
        .ok_or_else(|| rejected("GitHub owner is invalid"))?;
    let repo = object
        .get("repo")
        .and_then(Value::as_str)
        .filter(|value| valid_repository_component(value, 100))
        .map(str::to_owned)
        .ok_or_else(|| rejected("GitHub repository is invalid"))?;
    Ok((owner, repo))
}

fn valid_git_ref(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && !value.starts_with('/')
        && !value.ends_with('/')
        && !value.contains("..")
        && !value.contains("//")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'/' | b'-'))
}

fn git_ref_field(object: &Map<String, Value>, field: &str) -> Result<String, PortError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| valid_git_ref(value))
        .map(str::to_owned)
        .ok_or_else(|| rejected("GitHub ref is invalid"))
}

fn valid_workflow_path(value: &str) -> bool {
    let Some(name) = value.strip_prefix(".github/workflows/") else {
        return false;
    };
    !name.is_empty()
        && !name.contains('/')
        && !name.starts_with('.')
        && matches!(name.rsplit_once('.'), Some((stem, "yml" | "yaml")) if !stem.is_empty())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn workflow_path_field(object: &Map<String, Value>, field: &str) -> Result<String, PortError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| valid_workflow_path(value))
        .map(str::to_owned)
        .ok_or_else(|| rejected("GitHub workflow path is invalid"))
}

fn valid_input_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

fn valid_package_path(value: &str) -> bool {
    let Some(remainder) = value.strip_prefix(".steward/tasks/") else {
        return false;
    };
    !remainder.is_empty()
        && remainder.ends_with("/task-definition.json")
        && !remainder.contains("..")
        && !remainder.contains("//")
        && remainder
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'/' | b'-'))
}

/// Browser releases up to v0.3.8 tested a root Task definition with a path-backed prompt
/// beside it; the package closure digest binds those exact paths.
const LEGACY_ROOT_TASK_DEFINITION_PATH: &str = "task-definition.json";
const LEGACY_ROOT_PROMPT_PATH: &str = "prompt.md";

/// Whether a governed publication may write this package: a `promptText` Task definition
/// under `.steward/tasks/`, or exactly the legacy root Task definition with its root
/// `prompt.md`. Root paths are never accepted for any other package shape.
pub fn valid_publication_package(definition: &str, prompt: Option<&str>) -> bool {
    match prompt {
        None => valid_package_path(definition),
        Some(prompt) => {
            definition == LEGACY_ROOT_TASK_DEFINITION_PATH && prompt == LEGACY_ROOT_PROMPT_PATH
        }
    }
}

/// Repository-root files a legacy publication writes. The base-branch read for these paths
/// lets Steward refuse to overwrite different existing content.
pub fn legacy_root_package_path(value: &str) -> bool {
    matches!(
        value,
        LEGACY_ROOT_TASK_DEFINITION_PATH | LEGACY_ROOT_PROMPT_PATH
    )
}

fn published_files_field(
    object: &Map<String, Value>,
) -> Result<Vec<GithubPublishedFile>, PortError> {
    let files = object
        .get("files")
        .and_then(Value::as_array)
        .filter(|files| (2..=3).contains(&files.len()))
        .ok_or_else(|| rejected("GitHub publication must contain two or three generated files"))?;
    let mut parsed = Vec::with_capacity(files.len());
    for file in files {
        let file = file
            .as_object()
            .ok_or_else(|| rejected("GitHub publication file is invalid"))?;
        require_exact_fields(file, &["path", "content"])?;
        let path = bounded_string_field(file, "path", 512)?;
        let content = bounded_string_field(file, "content", MAX_PUBLISHED_FILE_BYTES)?;
        parsed.push(GithubPublishedFile { path, content });
    }
    let package = parsed
        .iter()
        .map(|file| file.path.as_str())
        .filter(|path| !valid_workflow_path(path))
        .collect::<Vec<_>>();
    let allowed = parsed.len() - package.len() == 1
        && match package.as_slice() {
            [definition] => valid_publication_package(definition, None),
            [first, second] => {
                valid_publication_package(first, Some(second))
                    || valid_publication_package(second, Some(first))
            }
            _ => false,
        };
    if !allowed {
        return Err(rejected(
            "GitHub publication paths are outside the generated workflow and package allowlist",
        ));
    }
    Ok(parsed)
}

#[derive(Clone)]
pub struct GithubMcpGateway {
    client: Client,
    origin: Url,
    contract: GatewayContract,
}

/// A one-request HOP-1 credential used only by the fixed connection-status reader.
/// Deliberately implements neither `Debug` nor `Display`.
pub struct GithubStatusCredential(String);

impl GithubStatusCredential {
    pub fn new(value: String) -> Result<Self, PortError> {
        let segments = value.split('.').collect::<Vec<_>>();
        let compact_jwt = value.len() <= 8 * 1024
            && segments.len() == 3
            && segments.iter().all(|segment| {
                !segment.is_empty()
                    && segment
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            });
        if compact_jwt {
            Ok(Self(value))
        } else {
            Err(rejected("direct status credential must be one compact JWT"))
        }
    }

    fn secret(&self) -> &str {
        &self.0
    }
}

/// MCP-GW metadata-only status reader. It has no mutation or generic request surface.
#[derive(Clone)]
pub struct GithubStatusReader {
    client: Client,
    origin: Url,
    contract: GatewayContract,
}

impl GithubStatusReader {
    pub fn new(origin: &str, version: &str) -> Result<Self, PortError> {
        let origin = validate_origin(origin)?;
        let contract = GatewayContract::parse(version)?;
        if contract != GatewayContract::LifecycleV049 {
            return Err(rejected(
                "direct status requires the MCP-GW lifecycle metadata contract",
            ));
        }
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(DIRECT_STATUS_TIMEOUT)
            .timeout(DIRECT_STATUS_TIMEOUT)
            .build()
            .map_err(|_| unavailable("build direct MCP-GW status client"))?;
        Ok(Self {
            client,
            origin,
            contract,
        })
    }

    pub async fn read(&self, credential: &GithubStatusCredential) -> Result<Value, PortError> {
        let target = endpoint(&self.origin, LIFECYCLE_STATUS_PATH)?;
        let response = self
            .client
            .get(target)
            .header(AUTHORIZATION, format!("Bearer {}", credential.secret()))
            .header(ACCEPT, CONNECTION_STATUS_V2_MEDIA_TYPE)
            .send()
            .await
            .map_err(|_| unavailable("read direct GitHub connection status"))?;
        let status = response.status();
        let body = bounded_body_or_status(status, StatusCode::OK, read_bounded(response).await?)?;
        parse_response(self.contract, GithubBridgeOperation::Status, status, &body)
    }
}

impl GithubMcpGateway {
    pub fn new(origin: &str, version: &str) -> Result<Self, PortError> {
        let origin = validate_origin(origin)?;
        let contract = GatewayContract::parse(version)?;
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|_| unavailable("build MCP-GW client"))?;
        Ok(Self {
            client,
            origin,
            contract,
        })
    }

    pub async fn execute(
        &self,
        operation: GithubBridgeOperation,
        request: GithubBridgeRequest,
    ) -> Result<Value, PortError> {
        if matches!(
            operation,
            GithubBridgeOperation::Repositories
                | GithubBridgeOperation::Workflow
                | GithubBridgeOperation::RunStatus
                | GithubBridgeOperation::Dispatch
                | GithubBridgeOperation::Publish
        ) {
            return self.execute_automation(operation, request).await;
        }
        if !matches!(
            (operation, &request),
            (
                GithubBridgeOperation::Status | GithubBridgeOperation::Disconnect,
                GithubBridgeRequest::Empty
            ) | (
                GithubBridgeOperation::Start,
                GithubBridgeRequest::Start { .. }
            ) | (
                GithubBridgeOperation::Rerun,
                GithubBridgeRequest::Rerun { .. }
            )
        ) {
            return Err(rejected(
                "Connections bridge request does not match its allowlisted operation",
            ));
        }
        let (method, path) = operation.method_and_path(self.contract);
        let target = endpoint(&self.origin, path)?;
        let body = request.body();
        let started = Instant::now();
        let (status, body) = loop {
            let mut http = self.client.request(method.clone(), target.clone()).header(
                AUTHORIZATION,
                format!("Bearer {OPEN_SHELL_BEARER_PLACEHOLDER}"),
            );
            if operation == GithubBridgeOperation::Status {
                http = http.header(ACCEPT, CONNECTION_STATUS_V2_MEDIA_TYPE);
            }
            if let Some(body) = &body {
                http = http.json(body);
            }
            if operation == GithubBridgeOperation::Rerun {
                http = http
                    .header("Mcp-Protocol-Version", MCP_PROTOCOL_VERSION)
                    .header("Accept", "application/json, text/event-stream");
            }
            let response = match http.send().await {
                Ok(response) => response,
                Err(error)
                    if error.is_connect()
                        && started.elapsed() < PROVIDER_TRANSPORT_READY_TIMEOUT =>
                {
                    tokio::time::sleep(PROVIDER_TRANSPORT_RETRY_INTERVAL).await;
                    continue;
                }
                Err(_) => return Err(unavailable("call MCP-GW")),
            };
            let status = response.status();
            let body = bounded_body_or_status(
                status,
                operation.expected_status(),
                read_bounded(response).await?,
            )?;
            if pre_dispatch_provider_failure(status, &body)
                && started.elapsed() < PROVIDER_TRANSPORT_READY_TIMEOUT
            {
                tokio::time::sleep(PROVIDER_TRANSPORT_RETRY_INTERVAL).await;
                continue;
            }
            break (status, body);
        };
        parse_response(self.contract, operation, status, &body)
    }

    async fn execute_automation(
        &self,
        operation: GithubBridgeOperation,
        request: GithubBridgeRequest,
    ) -> Result<Value, PortError> {
        if self.contract != GatewayContract::LifecycleV049 {
            return Err(rejected(
                "GitHub repository automation requires the lifecycle gateway contract",
            ));
        }
        match (operation, request) {
            (
                GithubBridgeOperation::Repositories,
                GithubBridgeRequest::Repositories {
                    query,
                    page,
                    per_page,
                },
            ) => {
                let me = self
                    .call_mcp("steward-github-me", "get_me", json!({}))
                    .await?;
                let query = if query.is_empty() {
                    format!("user:{}", github_profile_login(&me)?)
                } else {
                    query
                };
                let repositories = self
                    .call_mcp(
                        "steward-github-repositories",
                        "search_repositories",
                        json!({"query": query, "page": page, "perPage": per_page}),
                    )
                    .await?;
                normalize_repositories(&me, &repositories, page, per_page)
            }
            (
                GithubBridgeOperation::Workflow,
                GithubBridgeRequest::Workflow {
                    owner,
                    repo,
                    path,
                    git_ref,
                    expected_content,
                },
            ) => {
                let file = self
                    .call_mcp(
                        "steward-github-workflow",
                        "get_file_contents",
                        json!({"owner": owner, "repo": repo, "path": path, "ref": git_ref}),
                    )
                    .await?;
                if legacy_root_package_path(&path) {
                    normalize_package_file(&file, &path, &expected_content)
                } else {
                    normalize_workflow(&file, &path, &expected_content)
                }
            }
            (
                GithubBridgeOperation::RunStatus,
                GithubBridgeRequest::RunStatus {
                    owner,
                    repo,
                    run_id,
                },
            ) => {
                let run = self
                    .call_mcp(
                        "steward-github-run",
                        "actions_get",
                        json!({
                            "method": "get_workflow_run",
                            "owner": owner,
                            "repo": repo,
                            "resource_id": run_id.to_string(),
                        }),
                    )
                    .await?;
                let jobs = self
                    .call_mcp(
                        "steward-github-jobs",
                        "actions_list",
                        json!({
                            "method": "list_workflow_jobs",
                            "owner": owner,
                            "repo": repo,
                            "resource_id": run_id.to_string(),
                            "perPage": 100,
                        }),
                    )
                    .await?;
                let failure_log = if run_conclusion(&run).is_some_and(|value| value != "success") {
                    self.call_mcp(
                        "steward-github-job-logs",
                        "get_job_logs",
                        json!({
                            "owner": owner,
                            "repo": repo,
                            "run_id": run_id,
                            "failed_only": true,
                            "return_content": true,
                            "tail_lines": 200,
                        }),
                    )
                    .await
                    .ok()
                    .and_then(|payload| bounded_log(&payload))
                } else {
                    None
                };
                normalize_run_status(&run, &jobs, run_id, failure_log)
            }
            (
                GithubBridgeOperation::Dispatch,
                GithubBridgeRequest::Dispatch {
                    owner,
                    repo,
                    workflow_id,
                    git_ref,
                    inputs,
                    expected_content,
                },
            ) => {
                let workflow = self
                    .call_mcp(
                        "steward-github-dispatch-workflow",
                        "get_file_contents",
                        json!({
                            "owner": owner,
                            "repo": repo,
                            "path": workflow_id,
                            "ref": git_ref,
                        }),
                    )
                    .await?;
                let content = workflow_content(&workflow)
                    .ok_or_else(|| rejected("GitHub workflow content is unavailable"))?;
                if content != expected_content
                    || !compatible_workflow(&content)
                    || !dispatch_inputs_declared(&content, &inputs)
                {
                    return Err(rejected(
                        "GitHub workflow does not match the exact Steward-generated caller",
                    ));
                }
                let before = self
                    .call_mcp(
                        "steward-github-runs-before",
                        "actions_list",
                        json!({
                            "method": "list_workflow_runs",
                            "owner": owner,
                            "repo": repo,
                            "resource_id": workflow_id,
                            "workflow_runs_filter": {"branch": git_ref},
                            "perPage": 10,
                        }),
                    )
                    .await?;
                let previous_run_id = maximum_run_id(&before);
                self.call_mcp(
                    "steward-github-dispatch",
                    "actions_run_trigger",
                    json!({
                        "method": "run_workflow",
                        "owner": owner,
                        "repo": repo,
                        "workflow_id": workflow_id,
                        "ref": git_ref,
                        "inputs": inputs,
                    }),
                )
                .await?;
                let deadline = Instant::now() + Duration::from_secs(20);
                loop {
                    let runs = self
                        .call_mcp(
                            "steward-github-runs-after",
                            "actions_list",
                            json!({
                                "method": "list_workflow_runs",
                                "owner": owner,
                                "repo": repo,
                                "resource_id": workflow_id,
                                "workflow_runs_filter": {"branch": git_ref},
                                "perPage": 10,
                            }),
                        )
                        .await?;
                    if let Some((run_id, url)) = newest_dispatched_run(&runs, previous_run_id) {
                        return Ok(json!({"runId": run_id, "url": url}));
                    }
                    if Instant::now() >= deadline {
                        return Err(unavailable("observe dispatched GitHub workflow run"));
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
            (
                GithubBridgeOperation::Publish,
                GithubBridgeRequest::Publish {
                    owner,
                    repo,
                    base_branch,
                    branch,
                    title,
                    body,
                    files,
                    resume_owned_branch,
                },
            ) => {
                let mut existing_pull_request = None;
                let mut branch_exists = false;
                let mut files_are_current = false;
                if resume_owned_branch {
                    let branch_commit = self
                        .call_mcp(
                            "steward-github-publication-branch",
                            "get_commit",
                            json!({
                                "owner": owner,
                                "repo": repo,
                                "sha": branch,
                                "detail": "stats",
                                "perPage": 100,
                            }),
                        )
                        .await?;
                    branch_exists = !mcp_not_found(&branch_commit);
                    if branch_exists {
                        let pull_requests = self
                            .call_mcp(
                                "steward-github-publication-pr",
                                "list_pull_requests",
                                json!({
                                    "owner": owner,
                                    "repo": repo,
                                    "state": "open",
                                    "base": base_branch,
                                    "head": format!("{owner}:{branch}"),
                                    "page": 1,
                                    "perPage": 10,
                                    "fields": ["number", "html_url", "state", "head", "base"],
                                }),
                            )
                            .await?;
                        existing_pull_request =
                            exact_open_pull_request(&pull_requests, &branch, &base_branch)?;

                        let base_commit = self
                            .call_mcp(
                                "steward-github-publication-base",
                                "get_commit",
                                json!({
                                    "owner": owner,
                                    "repo": repo,
                                    "sha": base_branch,
                                    "detail": "none",
                                }),
                            )
                            .await?;
                        let branch_is_unchanged = commit_sha(&branch_commit)
                            .zip(commit_sha(&base_commit))
                            .is_some_and(|(branch_sha, base_sha)| branch_sha == base_sha);
                        if !branch_is_unchanged
                            && !steward_publication_head(&branch_commit, &branch, &files)
                        {
                            return Err(rejected(
                                "GitHub publication branch ownership could not be proven",
                            ));
                        }
                        if !branch_is_unchanged {
                            files_are_current = true;
                            for file in &files {
                                let current = self
                                    .call_mcp(
                                        "steward-github-publication-file",
                                        "get_file_contents",
                                        json!({
                                            "owner": owner,
                                            "repo": repo,
                                            "path": file.path,
                                            "ref": branch,
                                        }),
                                    )
                                    .await?;
                                files_are_current &=
                                    workflow_content(&current).as_deref() == Some(&file.content);
                            }
                            if existing_pull_request.is_none() && !files_are_current {
                                return Err(rejected(
                                    "GitHub publication branch content is not Steward-owned",
                                ));
                            }
                        }
                    }
                }
                if !branch_exists {
                    let created = self
                        .call_mcp(
                            "steward-github-create-branch",
                            "create_branch",
                            json!({
                                "owner": owner,
                                "repo": repo,
                                "branch": branch,
                                "from_branch": base_branch,
                            }),
                        )
                        .await?;
                    require_write_success(&created, "create GitHub publication branch")?;
                }
                if !files_are_current {
                    let pushed = self
                        .call_mcp(
                            "steward-github-push-files",
                            "push_files",
                            json!({
                                "owner": owner,
                                "repo": repo,
                                "branch": branch,
                                "message": publication_commit_message(&branch),
                                "files": files.iter().map(|file| json!({
                                    "path": file.path,
                                    "content": file.content,
                                })).collect::<Vec<_>>(),
                            }),
                        )
                        .await?;
                    require_write_success(&pushed, "push GitHub publication files")?;
                }
                if let Some(pull_request) = existing_pull_request {
                    return normalize_pull_request(&pull_request, &branch);
                }
                let pull_request = self
                    .call_mcp(
                        "steward-github-create-pull-request",
                        "create_pull_request",
                        json!({
                            "owner": owner,
                            "repo": repo,
                            "base": base_branch,
                            "head": branch,
                            "title": title,
                            "body": body,
                            "draft": false,
                        }),
                    )
                    .await?;
                require_write_success(&pull_request, "create GitHub publication pull request")?;
                normalize_pull_request(&pull_request, &branch)
            }
            _ => Err(rejected(
                "Connections bridge request does not match its allowlisted operation",
            )),
        }
    }

    async fn call_mcp(
        &self,
        request_id: &str,
        tool: &str,
        arguments: Value,
    ) -> Result<Value, PortError> {
        let target = endpoint(&self.origin, MCP_PATH)?;
        let request = json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "tools/call",
            "params": {"name": tool, "arguments": arguments},
        });
        let started = Instant::now();
        loop {
            let response = match self
                .client
                .post(target.clone())
                .header(
                    AUTHORIZATION,
                    format!("Bearer {OPEN_SHELL_BEARER_PLACEHOLDER}"),
                )
                .header("Mcp-Protocol-Version", MCP_PROTOCOL_VERSION)
                .header("Accept", "application/json, text/event-stream")
                .json(&request)
                .send()
                .await
            {
                Ok(response) => response,
                Err(error)
                    if error.is_connect()
                        && started.elapsed() < PROVIDER_TRANSPORT_READY_TIMEOUT =>
                {
                    tokio::time::sleep(PROVIDER_TRANSPORT_RETRY_INTERVAL).await;
                    continue;
                }
                Err(_) => return Err(unavailable("call MCP-GW")),
            };
            let status = response.status();
            let body =
                bounded_body_or_status(status, StatusCode::OK, read_bounded(response).await?)?;
            if pre_dispatch_provider_failure(status, &body)
                && started.elapsed() < PROVIDER_TRANSPORT_READY_TIMEOUT
            {
                tokio::time::sleep(PROVIDER_TRANSPORT_RETRY_INTERVAL).await;
                continue;
            }
            require_status(status, StatusCode::OK, &body)?;
            return mcp_tool_payload(&body, request_id);
        }
    }
}

fn pre_dispatch_provider_failure(status: StatusCode, body: &[u8]) -> bool {
    if status == StatusCode::UNAUTHORIZED {
        return true;
    }
    token_grant_failure(status, body)
}

fn token_grant_failure(status: StatusCode, body: &[u8]) -> bool {
    if status != StatusCode::BAD_GATEWAY {
        return false;
    }
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .is_some_and(|object| {
            object.len() == 2
                && object.get("error").and_then(Value::as_str) == Some("token_grant_failed")
                && object.get("detail").and_then(Value::as_str)
                    == Some("dynamic token grant failed")
        })
}

fn parse_response(
    contract: GatewayContract,
    operation: GithubBridgeOperation,
    status: StatusCode,
    body: &[u8],
) -> Result<Value, PortError> {
    match operation {
        GithubBridgeOperation::Status => {
            require_status(status, StatusCode::OK, body)?;
            let object = json_object(body, "GitHub status response")?;
            match contract {
                GatewayContract::LegacyV032 => {
                    validate_status_response(&object)?;
                    Ok(Value::Object(object))
                }
                GatewayContract::LifecycleV049 => normalized_status_response(&object),
            }
        }
        GithubBridgeOperation::Start => {
            require_status(status, StatusCode::OK, body)?;
            let object = json_object(body, "GitHub start response")?;
            let authorization_url = exact_string_field(&object, "authorizationUrl")?;
            validate_authorization_url(&authorization_url)?;
            Ok(json!({"authorizationUrl": authorization_url}))
        }
        GithubBridgeOperation::Disconnect => {
            require_status(status, StatusCode::NO_CONTENT, body)?;
            if !body.is_empty() {
                return Err(rejected(
                    "GitHub disconnect response must have an empty body",
                ));
            }
            Ok(json!({"disconnected": true}))
        }
        GithubBridgeOperation::Rerun => {
            require_status(status, StatusCode::OK, body)?;
            let object = mcp_json_object(body, "GitHub rerun MCP response")?;
            if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
                || object.get("id").and_then(Value::as_str) != Some(RERUN_REQUEST_ID)
                || object.contains_key("error")
            {
                return Err(rejected(
                    "GitHub rerun MCP response has an invalid envelope",
                ));
            }
            let result = object
                .get("result")
                .and_then(Value::as_object)
                .ok_or_else(|| rejected("GitHub rerun MCP response omitted its result"))?;
            if result.get("isError").and_then(Value::as_bool) == Some(true)
                || result
                    .get("structuredContent")
                    .and_then(Value::as_object)
                    .is_some_and(|structured| structured.contains_key("error"))
            {
                return Err(failed("MCP-GW rejected GitHub workflow rerun"));
            }
            Ok(json!({"dispatched": true}))
        }
        GithubBridgeOperation::Repositories
        | GithubBridgeOperation::Workflow
        | GithubBridgeOperation::RunStatus
        | GithubBridgeOperation::Dispatch
        | GithubBridgeOperation::Publish => Err(rejected(
            "GitHub automation responses require the typed multi-call adapter path",
        )),
    }
}

fn mcp_tool_payload(body: &[u8], request_id: &str) -> Result<Value, PortError> {
    let object = mcp_json_object(body, "GitHub MCP response")?;
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || object.get("id").and_then(Value::as_str) != Some(request_id)
        || object.contains_key("error")
    {
        return Err(rejected("GitHub MCP response has an invalid envelope"));
    }
    let result = object
        .get("result")
        .and_then(Value::as_object)
        .ok_or_else(|| rejected("GitHub MCP response omitted its result"))?;
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        let messages = result
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .map(str::to_ascii_lowercase)
            .collect::<Vec<_>>();
        return if messages.iter().any(|text| text.contains("not found")) {
            Ok(json!({"__stewardNotFound": true}))
        } else if messages
            .iter()
            .any(|text| text.contains("already exists") || text.contains("reference exists"))
        {
            Ok(json!({"__stewardAlreadyExists": true}))
        } else {
            Err(failed("GitHub MCP tool call failed"))
        };
    }
    if let Some(payload) = result.get("structuredContent") {
        return Ok(payload.clone());
    }
    let content = result
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| rejected("GitHub MCP response omitted structured output"))?;
    if let Some(resource) = content.iter().find_map(|item| {
        (item.get("type").and_then(Value::as_str) == Some("resource"))
            .then(|| item.get("resource").and_then(Value::as_object))
            .flatten()
    }) {
        let resource_content = match (
            resource.get("text").and_then(Value::as_str),
            resource.get("blob").and_then(Value::as_str),
        ) {
            (Some(text), None) if text.len() <= MAX_WORKFLOW_BYTES => text.to_owned(),
            (None, Some(blob)) => {
                let decoded = BASE64_STANDARD
                    .decode(blob)
                    .map_err(|_| rejected("GitHub MCP resource blob is not valid base64"))?;
                if decoded.len() > MAX_WORKFLOW_BYTES {
                    return Err(rejected("GitHub MCP resource blob is too large"));
                }
                String::from_utf8(decoded)
                    .map_err(|_| rejected("GitHub MCP resource blob is not UTF-8"))?
            }
            _ => return Err(rejected("GitHub MCP resource content is invalid")),
        };
        let sha = content
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .chain(resource.get("uri").and_then(Value::as_str))
            .find_map(github_blob_sha)
            .ok_or_else(|| rejected("GitHub MCP resource omitted its blob SHA"))?;
        return Ok(json!({"content": resource_content, "sha": sha}));
    }
    let text = content
        .iter()
        .find_map(|item| item.get("text").and_then(Value::as_str))
        .ok_or_else(|| rejected("GitHub MCP response omitted structured output"))?;
    serde_json::from_str(text).or_else(|_| Ok(Value::String(text.to_owned())))
}

fn github_blob_sha(value: &str) -> Option<String> {
    value
        .as_bytes()
        .windows(40)
        .enumerate()
        .find_map(|(index, bytes)| {
            let bounded_before = index == 0 || !value.as_bytes()[index - 1].is_ascii_hexdigit();
            let bounded_after =
                index + 40 == value.len() || !value.as_bytes()[index + 40].is_ascii_hexdigit();
            (bounded_before && bounded_after && bytes.iter().all(u8::is_ascii_hexdigit))
                .then(|| String::from_utf8_lossy(bytes).into_owned())
        })
}

fn payload_array<'a>(payload: &'a Value, fields: &[&str]) -> Option<&'a Vec<Value>> {
    fields
        .iter()
        .find_map(|field| payload.get(*field).and_then(Value::as_array))
        .or_else(|| payload.as_array())
}

fn normalize_repositories(
    me: &Value,
    payload: &Value,
    page: u32,
    per_page: u32,
) -> Result<Value, PortError> {
    let login = github_profile_login(me)?;
    let items = payload_array(payload, &["items", "repositories"])
        .filter(|items| items.len() <= usize::try_from(per_page).unwrap_or(100))
        .ok_or_else(|| rejected("GitHub repository search response is invalid"))?;
    let repositories = items
        .iter()
        .map(normalize_repository)
        .collect::<Result<Vec<_>, PortError>>()?;
    Ok(json!({
        "login": login,
        "repositories": repositories,
        "page": page,
        "hasNextPage": items.len() == usize::try_from(per_page).unwrap_or(100),
    }))
}

fn github_profile_login(me: &Value) -> Result<&str, PortError> {
    me.get("login")
        .and_then(Value::as_str)
        .filter(|login| valid_repository_component(login, 39))
        .ok_or_else(|| rejected("GitHub profile response omitted its login"))
}

fn normalize_repository(repository: &Value) -> Result<Value, PortError> {
    let name = repository
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| valid_repository_component(name, 100))
        .ok_or_else(|| rejected("GitHub repository response omitted its name"))?;
    let owner = repository
        .get("owner")
        .and_then(|owner| owner.get("login").or(Some(owner)))
        .and_then(Value::as_str)
        .or_else(|| {
            repository
                .get("full_name")
                .and_then(Value::as_str)
                .and_then(|full_name| full_name.split_once('/').map(|(owner, _)| owner))
        })
        .filter(|owner| valid_repository_component(owner, 39))
        .ok_or_else(|| rejected("GitHub repository response omitted its owner"))?;
    let repository_id = numeric_provider_id(repository.get("id"))
        .ok_or_else(|| rejected("GitHub repository response omitted its stable ID"))?;
    let owner_id = repository
        .get("owner")
        .and_then(|owner| numeric_provider_id(owner.get("id")))
        .or_else(|| numeric_provider_id(repository.get("owner_id")))
        .or_else(|| numeric_provider_id(repository.get("ownerId")))
        .ok_or_else(|| rejected("GitHub repository response omitted its owner stable ID"))?;
    let default_branch = repository
        .get("default_branch")
        .or_else(|| repository.get("defaultBranch"))
        .and_then(Value::as_str)
        .filter(|branch| valid_git_ref(branch))
        .ok_or_else(|| rejected("GitHub repository response omitted its default branch"))?;
    let private = repository
        .get("private")
        .and_then(Value::as_bool)
        .ok_or_else(|| rejected("GitHub repository response omitted its visibility"))?;
    let url = repository
        .get("html_url")
        .or_else(|| repository.get("url"))
        .and_then(Value::as_str)
        .filter(|url| valid_github_url(url))
        .ok_or_else(|| rejected("GitHub repository response omitted its URL"))?;
    Ok(json!({
        "owner": owner,
        "ownerId": owner_id,
        "name": name,
        "repositoryId": repository_id,
        "defaultBranch": default_branch,
        "private": private,
        "url": url,
    }))
}

fn numeric_provider_id(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::Number(value) => value.as_u64().map(|value| value.to_string()),
        Value::String(value)
            if !value.is_empty()
                && value.len() <= 20
                && value.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            Some(value.clone())
        }
        _ => None,
    }
}

fn normalize_workflow(
    payload: &Value,
    path: &str,
    expected_content: &str,
) -> Result<Value, PortError> {
    if payload.get("__stewardNotFound").and_then(Value::as_bool) == Some(true) {
        return Ok(json!({
            "exists": false,
            "compatible": false,
            "path": path,
            "sha": Value::Null,
        }));
    }
    let content = workflow_content(payload)
        .ok_or_else(|| rejected("GitHub workflow response omitted file content"))?;
    let sha = payload
        .get("sha")
        .or_else(|| payload.get("data").and_then(|data| data.get("sha")))
        .and_then(Value::as_str)
        .filter(|sha| sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| rejected("GitHub workflow response omitted its blob SHA"))?;
    Ok(json!({
        "exists": true,
        "compatible": content == expected_content && compatible_workflow(&content),
        "path": path,
        "sha": sha,
    }))
}

/// A package file read reports `compatible` only for byte-identical content; an existing
/// file whose content is unreadable or different is incompatible.
fn normalize_package_file(
    payload: &Value,
    path: &str,
    expected_content: &str,
) -> Result<Value, PortError> {
    if mcp_not_found(payload) {
        return Ok(json!({"exists": false, "compatible": false, "path": path, "sha": Value::Null}));
    }
    let sha = payload
        .get("sha")
        .or_else(|| payload.get("data").and_then(|data| data.get("sha")))
        .and_then(Value::as_str)
        .filter(|sha| sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_hexdigit()));
    Ok(json!({
        "exists": true,
        "compatible": workflow_content(payload).as_deref() == Some(expected_content),
        "path": path,
        "sha": sha,
    }))
}

fn workflow_content(payload: &Value) -> Option<String> {
    payload
        .get("content")
        .or_else(|| payload.get("data").and_then(|data| data.get("content")))
        .and_then(Value::as_str)
        .filter(|content| content.len() <= MAX_WORKFLOW_BYTES)
        .map(str::to_owned)
        .or_else(|| {
            payload
                .as_str()
                .filter(|content| content.len() <= MAX_WORKFLOW_BYTES)
                .map(str::to_owned)
        })
}

fn compatible_workflow(content: &str) -> bool {
    let has_dispatch = content
        .lines()
        .any(|line| line.trim() == "workflow_dispatch:");
    let has_pinned_steward_run = content.lines().any(|line| {
        let line = line.trim();
        let Some(reference) = line.strip_prefix("uses: ") else {
            return false;
        };
        let Some((workflow, revision)) = reference.rsplit_once('@') else {
            return false;
        };
        workflow.contains("/steward-run/.github/workflows/steward-task")
            && revision.len() == 40
            && revision.bytes().all(|byte| byte.is_ascii_hexdigit())
    });
    has_dispatch && has_pinned_steward_run
}

fn dispatch_inputs_declared(content: &str, inputs: &Map<String, Value>) -> bool {
    inputs.keys().all(|input| {
        let declaration = format!("{input}:");
        content
            .lines()
            .any(|line| line.len() - line.trim_start().len() >= 6 && line.trim() == declaration)
    })
}

fn run_conclusion(payload: &Value) -> Option<&str> {
    payload.get("conclusion").and_then(Value::as_str)
}

fn bounded_log(payload: &Value) -> Option<String> {
    let text = payload
        .as_str()
        .or_else(|| payload.get("text").and_then(Value::as_str))?;
    let mut end = text.len().min(8 * 1024);
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    Some(text[..end].to_owned())
}

fn normalize_run_status(
    run: &Value,
    jobs: &Value,
    run_id: u64,
    failure_log: Option<String>,
) -> Result<Value, PortError> {
    let run_attempt = run
        .get("run_attempt")
        .or_else(|| run.get("runAttempt"))
        .and_then(Value::as_u64)
        .filter(|attempt| *attempt > 0)
        .ok_or_else(|| rejected("GitHub run response omitted its attempt"))?;
    let status = run
        .get("status")
        .and_then(Value::as_str)
        .filter(|status| matches!(*status, "queued" | "in_progress" | "completed"))
        .ok_or_else(|| rejected("GitHub run response omitted its status"))?;
    let conclusion = run.get("conclusion").cloned().unwrap_or(Value::Null);
    if !matches!(conclusion, Value::Null | Value::String(_)) {
        return Err(rejected("GitHub run response has an invalid conclusion"));
    }
    let url = run
        .get("html_url")
        .or_else(|| run.get("url"))
        .and_then(Value::as_str)
        .filter(|url| valid_github_url(url))
        .ok_or_else(|| rejected("GitHub run response omitted its URL"))?;
    let jobs = payload_array(jobs, &["jobs"])
        .filter(|jobs| jobs.len() <= 100)
        .ok_or_else(|| rejected("GitHub jobs response is invalid"))?
        .iter()
        .map(normalize_job)
        .collect::<Result<Vec<_>, PortError>>()?;
    let mut result = json!({
        "runId": run_id,
        "runAttempt": run_attempt,
        "phase": status,
        "conclusion": conclusion,
        "url": url,
        "jobs": jobs,
    });
    if let Some(failure_log) = failure_log {
        result["failureLog"] = Value::String(failure_log);
    }
    Ok(result)
}

fn normalize_job(job: &Value) -> Result<Value, PortError> {
    let id = job
        .get("id")
        .and_then(Value::as_u64)
        .filter(|id| *id > 0)
        .ok_or_else(|| rejected("GitHub job response omitted its ID"))?;
    let name = job
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty() && name.len() <= 500)
        .ok_or_else(|| rejected("GitHub job response omitted its name"))?;
    let status = job
        .get("status")
        .and_then(Value::as_str)
        .filter(|status| matches!(*status, "queued" | "in_progress" | "completed"))
        .ok_or_else(|| rejected("GitHub job response omitted its status"))?;
    let conclusion = job.get("conclusion").cloned().unwrap_or(Value::Null);
    let url = job
        .get("html_url")
        .or_else(|| job.get("url"))
        .and_then(Value::as_str)
        .filter(|url| valid_github_url(url))
        .ok_or_else(|| rejected("GitHub job response omitted its URL"))?;
    Ok(json!({
        "id": id,
        "name": name,
        "status": status,
        "conclusion": conclusion,
        "url": url,
    }))
}

fn maximum_run_id(payload: &Value) -> u64 {
    payload_array(payload, &["workflow_runs", "runs", "items"])
        .into_iter()
        .flatten()
        .filter_map(|run| run.get("id").and_then(Value::as_u64))
        .max()
        .unwrap_or(0)
}

fn newest_dispatched_run(payload: &Value, previous_run_id: u64) -> Option<(u64, String)> {
    payload_array(payload, &["workflow_runs", "runs", "items"])?
        .iter()
        .filter(|run| {
            run.get("event")
                .and_then(Value::as_str)
                .is_none_or(|event| event == "workflow_dispatch")
        })
        .filter_map(|run| {
            let id = run.get("id").and_then(Value::as_u64)?;
            let url = run
                .get("html_url")
                .or_else(|| run.get("url"))
                .and_then(Value::as_str)
                .filter(|url| valid_github_url(url))?;
            (id > previous_run_id).then(|| (id, url.to_owned()))
        })
        .max_by_key(|(id, _)| *id)
}

fn normalize_pull_request(payload: &Value, branch: &str) -> Result<Value, PortError> {
    let number = payload
        .get("number")
        .and_then(Value::as_u64)
        .filter(|number| *number > 0)
        .ok_or_else(|| rejected("GitHub pull request response omitted its number"))?;
    let url = payload
        .get("html_url")
        .or_else(|| payload.get("url"))
        .and_then(Value::as_str)
        .filter(|url| valid_github_url(url))
        .ok_or_else(|| rejected("GitHub pull request response omitted its URL"))?;
    Ok(json!({
        "pullRequestUrl": url,
        "pullRequestNumber": number,
        "branch": branch,
    }))
}

fn mcp_not_found(payload: &Value) -> bool {
    payload.get("__stewardNotFound").and_then(Value::as_bool) == Some(true)
}

fn require_write_success(payload: &Value, operation: &str) -> Result<(), PortError> {
    if mcp_not_found(payload)
        || payload
            .get("__stewardAlreadyExists")
            .and_then(Value::as_bool)
            == Some(true)
    {
        Err(failed(&format!("GitHub MCP failed to {operation}")))
    } else {
        Ok(())
    }
}

fn commit_sha(payload: &Value) -> Option<&str> {
    payload
        .get("sha")
        .or_else(|| payload.get("commit").and_then(|commit| commit.get("sha")))
        .and_then(Value::as_str)
        .filter(|sha| sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn publication_commit_message(branch: &str) -> String {
    format!("chore: publish Steward governed task on {branch}")
}

fn steward_publication_head(
    payload: &Value,
    branch: &str,
    expected_files: &[GithubPublishedFile],
) -> bool {
    let message = payload
        .get("commit")
        .and_then(|commit| commit.get("message"))
        .or_else(|| payload.get("message"))
        .and_then(Value::as_str);
    let Some(files) = payload.get("files").and_then(Value::as_array) else {
        return false;
    };
    let actual = files
        .iter()
        .map(|file| {
            file.get("filename")
                .or_else(|| file.get("path"))
                .and_then(Value::as_str)
        })
        .collect::<Option<Vec<_>>>();
    // GitHub lists only files the commit changed: a published file identical to the base
    // branch is absent. Steward's commit owns the branch when it changed at least one file
    // and only publication files; the caller then verifies every file's content.
    message == Some(publication_commit_message(branch).as_str())
        && actual.is_some_and(|actual| {
            !actual.is_empty()
                && actual
                    .iter()
                    .all(|path| expected_files.iter().any(|file| file.path == *path))
        })
}

fn exact_open_pull_request(
    payload: &Value,
    branch: &str,
    base_branch: &str,
) -> Result<Option<Value>, PortError> {
    let pull_requests = payload_array(payload, &["pull_requests", "items"])
        .ok_or_else(|| rejected("GitHub pull request list response is invalid"))?;
    let matches = pull_requests
        .iter()
        .filter(|pull_request| {
            pull_request.get("state").and_then(Value::as_str) == Some("open")
                && pull_request
                    .get("head")
                    .and_then(|head| head.get("ref"))
                    .and_then(Value::as_str)
                    == Some(branch)
                && pull_request
                    .get("base")
                    .and_then(|base| base.get("ref"))
                    .and_then(Value::as_str)
                    == Some(base_branch)
        })
        .cloned()
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Ok(None),
        [pull_request] => Ok(Some(pull_request.clone())),
        _ => Err(rejected(
            "GitHub publication branch has ambiguous pull requests",
        )),
    }
}

fn valid_github_url(value: &str) -> bool {
    Url::parse(value).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str() == Some("github.com")
            && url.username().is_empty()
            && url.password().is_none()
    })
}

fn require_status(actual: StatusCode, expected: StatusCode, body: &[u8]) -> Result<(), PortError> {
    if actual == expected {
        Ok(())
    } else if actual == StatusCode::UNAUTHORIZED {
        Err(failed("MCP-GW rejected runtime authentication"))
    } else if token_grant_failure(actual, body) {
        Err(PortError::CredentialGrantFailed)
    } else if actual == StatusCode::FORBIDDEN {
        let proxy_denial = serde_json::from_slice::<Value>(body)
            .ok()
            .and_then(|value| {
                value
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .is_some_and(|error| matches!(error.as_str(), "policy_denied" | "ssrf_denied"));
        if proxy_denial {
            Err(failed("OpenShell proxy denied the provider request"))
        } else {
            Err(failed("MCP-GW rejected runtime authorization"))
        }
    } else {
        let diagnostic = sanitized_gateway_failure_detail(actual.as_u16(), body);
        let code = diagnostic
            .code
            .map(|code| format!(" [code={code}]"))
            .unwrap_or_default();
        let detail = diagnostic
            .reason
            .map(|detail| format!(" ({detail})"))
            .unwrap_or_default();
        Err(failed(&format!(
            "MCP-GW returned HTTP {}{code}{detail}",
            actual.as_u16()
        )))
    }
}

fn sanitized_gateway_failure_detail(status: u16, body: &[u8]) -> GithubBridgeFailureDiagnostic {
    let object = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| value.as_object().cloned());
    let code = object
        .as_ref()
        .and_then(|object| object.get("code"))
        .and_then(Value::as_str)
        .and_then(sanitized_gateway_failure_code);
    let reason = object
        .as_ref()
        .and_then(|object| object.get("error"))
        .and_then(Value::as_str)
        .and_then(sanitized_gateway_failure_scalar);
    GithubBridgeFailureDiagnostic {
        status,
        code,
        reason,
    }
}

fn sanitized_gateway_failure_code(value: &str) -> Option<String> {
    let valid = !value.is_empty()
        && value.len() <= MAX_GATEWAY_FAILURE_CODE_BYTES
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        });
    valid.then(|| value.to_owned())
}

fn sanitized_gateway_failure_scalar(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b' ' | b'.' | b',' | b':' | b';' | b'\'' | b'(' | b')' | b'_' | b'/' | b'-'
                )
        })
        || contains_sensitive_gateway_failure_material(value)
    {
        return None;
    }
    let truncated = value[..value.len().min(MAX_GATEWAY_FAILURE_DETAIL_BYTES)].trim_end();
    (!truncated.is_empty()).then(|| truncated.to_owned())
}

fn contains_sensitive_gateway_failure_material(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if [
        "authorization",
        "bearer",
        "cookie",
        "token",
        "secret",
        "password",
        "api_key",
        "api-key",
        "apikey",
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        return true;
    }

    let has_compact_high_entropy_word = value
        .split(|character: char| {
            !character.is_ascii_alphanumeric() && character != '_' && character != '-'
        })
        .any(|word| {
            word.len() >= 20
                && word.bytes().any(|byte| byte.is_ascii_alphabetic())
                && word.bytes().any(|byte| byte.is_ascii_digit())
        });
    if has_compact_high_entropy_word {
        return true;
    }

    value
        .split(|character: char| {
            character.is_whitespace() || matches!(character, '"' | '\'' | '(' | ')' | ',' | ';')
        })
        .any(|word| {
            let segments = word.split('.').collect::<Vec<_>>();
            segments.windows(3).any(|window| {
                window.iter().all(|segment| {
                    segment.len() >= 8
                        && segment
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
                })
            })
        })
}

fn validate_status_response(object: &Map<String, Value>) -> Result<(), PortError> {
    if !object.keys().all(|key| {
        matches!(
            key.as_str(),
            "connected" | "email" | "scopesRequired" | "scopesGranted" | "missingScopes"
        )
    }) || !object.get("connected").is_some_and(Value::is_boolean)
        || object.get("email").is_some_and(|value| !value.is_string())
        || object
            .get("scopesRequired")
            .is_some_and(|value| !string_array(Some(value)))
        || object
            .get("scopesGranted")
            .is_some_and(|value| !string_array(Some(value)))
        || object
            .get("missingScopes")
            .is_some_and(|value| !string_array(Some(value)))
    {
        return Err(rejected("GitHub status response has an invalid schema"));
    }
    let connected = object
        .get("connected")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let missing = object
        .get("missingScopes")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty);
    if connected && (!object.contains_key("email") || !missing) {
        return Err(rejected(
            "connected GitHub status must include its account and every required scope",
        ));
    }
    Ok(())
}

fn normalized_status_response(object: &Map<String, Value>) -> Result<Value, PortError> {
    // Validate only the fields Steward consumes. MCP-GW may add lifecycle metadata without
    // changing this contract, and the projection below prevents unconsumed values from entering
    // Steward's governed archive or browser response.
    let version = object
        .get("version")
        .and_then(Value::as_str)
        .filter(|version| matches!(*version, "1" | "2"))
        .ok_or_else(|| rejected("GitHub lifecycle status has an invalid identity or schema"))?;
    if object.get("provider").and_then(Value::as_str) != Some("github") {
        return Err(rejected(
            "GitHub lifecycle status has an invalid identity or schema",
        ));
    }

    let phase = object
        .get("phase")
        .and_then(Value::as_str)
        .filter(|phase| {
            matches!(
                *phase,
                "disconnected"
                    | "authorizing"
                    | "connected"
                    | "renewing"
                    | "reauthorization_required"
                    | "revocation_pending"
                    | "disconnected_with_provider_cleanup_pending"
                    | "unavailable"
            )
        })
        .ok_or_else(|| rejected("GitHub lifecycle phase is invalid"))?;
    let connected = object
        .get("connected")
        .and_then(Value::as_bool)
        .ok_or_else(|| rejected("GitHub lifecycle connected state is invalid"))?;
    let account = object
        .get("account")
        .map(|value| {
            let account = value
                .as_object()
                .ok_or_else(|| rejected("GitHub lifecycle account is invalid"))?;
            let display_name = account
                .get("displayName")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty() && name.len() <= 320)
                .map(str::to_owned)
                .ok_or_else(|| rejected("GitHub lifecycle account is invalid"))?;
            if version == "1" {
                if account.len() != 1 {
                    return Err(rejected("GitHub lifecycle account is invalid"));
                }
                return Ok((display_name, None, None));
            }
            if account.get("provider").and_then(Value::as_str) != Some("github") {
                return Err(rejected("GitHub lifecycle account is invalid"));
            }
            let account_id = account
                .get("id")
                .map(|value| {
                    value
                        .as_str()
                        .filter(|id| {
                            !id.is_empty()
                                && id.len() <= 20
                                && id.bytes().all(|byte| byte.is_ascii_digit())
                                && !id.starts_with('0')
                        })
                        .map(str::to_owned)
                        .ok_or_else(|| rejected("GitHub lifecycle account ID is invalid"))
                })
                .transpose()?;
            let login = account
                .get("login")
                .map(|value| {
                    value
                        .as_str()
                        .filter(|login| !login.is_empty() && login.len() <= 128)
                        .map(str::to_owned)
                        .ok_or_else(|| rejected("GitHub lifecycle account login is invalid"))
                })
                .transpose()?;
            Ok((display_name, account_id, login))
        })
        .transpose()?;
    let scopes_required = required_string_array(object, "requiredScopes")?;
    let scopes_granted = required_string_array(object, "grantedScopes")?;
    let scopes_missing = required_string_array(object, "missingScopes")?;
    if connected != (phase == "connected")
        || (connected
            && (account.is_none()
                || scopes_missing
                    .as_array()
                    .is_some_and(|values| !values.is_empty())))
    {
        return Err(rejected("GitHub lifecycle phase and account disagree"));
    }
    let active_expiry = nullable_utc_timestamp(object, "activeCredentialExpiresAt")?;
    let renewal_expiry = nullable_utc_timestamp(object, "renewalCredentialExpiresAt")?;
    for key in ["lastAuthorizedAt", "lastRenewedAt", "lastValidatedAt"] {
        nullable_utc_timestamp(object, key)?;
    }
    let capabilities = object
        .get("capabilities")
        .and_then(Value::as_object)
        .ok_or_else(|| rejected("GitHub lifecycle capabilities are invalid"))?;
    if capabilities.iter().any(|(key, value)| {
        matches!(
            key.as_str(),
            "interactiveAuthorization"
                | "activeCredentialExpiry"
                | "automaticRenewal"
                | "manualRenewal"
                | "rotatingRenewalCredential"
                | "providerValidation"
                | "providerRevocation"
                | "scopeReporting"
                | "identityVerification"
                | "authorizationRequiresRenewalCredential"
        ) && !value.is_boolean()
    }) || object
        .get("errorCategory")
        .is_some_and(|value| value.as_str().is_none_or(|category| category.len() > 64))
    {
        return Err(rejected(
            "GitHub lifecycle capabilities or error are invalid",
        ));
    }

    let mut projection = Map::new();
    projection.insert("phase".to_owned(), Value::String(phase.to_owned()));
    projection.insert("connected".to_owned(), Value::Bool(connected));
    if let Some((display_name, account_id, login)) = account {
        projection.insert("email".to_owned(), Value::String(display_name));
        if let Some(account_id) = account_id {
            projection.insert("accountId".to_owned(), Value::String(account_id));
        }
        if let Some(login) = login {
            projection.insert("accountLogin".to_owned(), Value::String(login));
        }
    }
    projection.insert("scopesRequired".to_owned(), scopes_required);
    projection.insert("scopesGranted".to_owned(), scopes_granted);
    projection.insert("missingScopes".to_owned(), scopes_missing);
    projection.insert("activeCredentialExpiresAt".to_owned(), active_expiry);
    projection.insert("renewalCredentialExpiresAt".to_owned(), renewal_expiry);
    Ok(Value::Object(projection))
}

fn required_string_array(object: &Map<String, Value>, key: &str) -> Result<Value, PortError> {
    object
        .get(key)
        .filter(|value| string_array(Some(value)))
        .cloned()
        .ok_or_else(|| rejected("GitHub lifecycle scope list is invalid"))
}

fn nullable_utc_timestamp(object: &Map<String, Value>, key: &str) -> Result<Value, PortError> {
    let value = object
        .get(key)
        .ok_or_else(|| rejected("GitHub lifecycle timestamp is missing"))?;
    if value.is_null() || value.as_str().is_some_and(valid_utc_timestamp) {
        Ok(value.clone())
    } else {
        Err(rejected("GitHub lifecycle timestamp is invalid"))
    }
}

fn valid_utc_timestamp(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 24
        || ![4, 7, 10, 13, 16, 19, 23]
            .into_iter()
            .zip([b'-', b'-', b'T', b':', b':', b'.', b'Z'])
            .all(|(index, expected)| bytes[index] == expected)
        || !bytes.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19 | 23) || byte.is_ascii_digit()
        })
    {
        return false;
    }
    let number = |start: usize, end: usize| -> Option<u32> { value[start..end].parse().ok() };
    let Some((year, month, day, hour, minute, second)) = number(0, 4)
        .zip(number(5, 7))
        .zip(number(8, 10))
        .zip(number(11, 13))
        .zip(number(14, 16))
        .zip(number(17, 19))
        .map(|(((((year, month), day), hour), minute), second)| {
            (year, month, day, hour, minute, second)
        })
    else {
        return false;
    };
    let leap_year =
        year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap_year => 29,
        2 => 28,
        _ => return false,
    };
    (1..=max_day).contains(&day) && hour < 24 && minute < 60 && second < 60
}

fn string_array(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_array)
        .is_some_and(|values| values.iter().all(Value::is_string))
}

fn json_object(body: &[u8], description: &str) -> Result<Map<String, Value>, PortError> {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .ok_or_else(|| rejected(&format!("{description} must be one JSON object")))
}

fn mcp_json_object(body: &[u8], description: &str) -> Result<Map<String, Value>, PortError> {
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        return value
            .as_object()
            .cloned()
            .ok_or_else(|| rejected(&format!("{description} must be one JSON object")));
    }
    let text = std::str::from_utf8(body)
        .map_err(|_| rejected(&format!("{description} must be JSON or one SSE event")))?;
    let mut data = None;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(value) = line.strip_prefix("data:") {
            if data.replace(value.trim_start()).is_some() {
                return Err(rejected(&format!(
                    "{description} must contain exactly one SSE data event"
                )));
            }
        } else if line.strip_prefix("event:").map(str::trim) != Some("message") {
            return Err(rejected(&format!(
                "{description} contains an unsupported SSE field"
            )));
        }
    }
    let data = data.ok_or_else(|| rejected(&format!("{description} omitted SSE data")))?;
    json_object(data.as_bytes(), description)
}

fn exact_string_field(object: &Map<String, Value>, expected: &str) -> Result<String, PortError> {
    if object.len() != 1 {
        return Err(rejected("Connections bridge request has unexpected fields"));
    }
    object
        .get(expected)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| rejected("Connections bridge request is missing its exact string field"))
}

fn valid_repository_component(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn validate_origin(value: &str) -> Result<Url, PortError> {
    let origin = Url::parse(value)
        .map_err(|_| rejected("MCP-GW origin must be an absolute HTTP(S) origin"))?;
    if !matches!(origin.scheme(), "http" | "https")
        || origin.host_str().is_none()
        || !origin.username().is_empty()
        || origin.password().is_some()
        || origin.path() != "/"
        || origin.query().is_some()
        || origin.fragment().is_some()
    {
        return Err(rejected("MCP-GW origin must be an exact HTTP(S) origin"));
    }
    Ok(origin)
}

fn endpoint(origin: &Url, path: &str) -> Result<Url, PortError> {
    origin
        .join(path)
        .map_err(|_| rejected("Connections bridge endpoint is invalid"))
}

fn validate_redirect_after(value: &str) -> Result<(), PortError> {
    let redirect = Url::parse(value)
        .map_err(|_| rejected("redirectAfter must be an allowlisted HTTPS Connections page"))?;
    if redirect.scheme() != "https"
        || redirect.host_str().is_none()
        || !redirect.username().is_empty()
        || redirect.password().is_some()
        || redirect.path() != "/connections"
        || redirect.query().is_some()
        || !matches!(redirect.fragment(), None | Some("github-connected"))
    {
        return Err(rejected(
            "redirectAfter must be an allowlisted HTTPS Connections page",
        ));
    }
    Ok(())
}

fn validate_authorization_url(value: &str) -> Result<(), PortError> {
    let url = Url::parse(value).map_err(|_| rejected("GitHub authorization URL must use HTTPS"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(rejected("GitHub authorization URL must use HTTPS"));
    }
    Ok(())
}

async fn read_bounded(mut response: reqwest::Response) -> Result<Option<Vec<u8>>, PortError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Ok(None);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| unavailable("read MCP-GW response"))?
    {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Ok(None);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Some(body))
}

fn bounded_body_or_status(
    status: StatusCode,
    expected: StatusCode,
    body: Option<Vec<u8>>,
) -> Result<Vec<u8>, PortError> {
    if let Some(body) = body {
        return Ok(body);
    }
    require_status(status, expected, &[])?;
    Err(unavailable("read bounded MCP-GW response"))
}

fn rejected(reason: &str) -> PortError {
    PortError::Rejected {
        reason: reason.to_owned(),
    }
}

fn unavailable(operation: &str) -> PortError {
    failed(&format!(
        "MCP-GW unavailable while attempting to {operation}"
    ))
}

fn failed(reason: &str) -> PortError {
    PortError::Failed {
        reason: reason.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;
    use std::time::Duration;

    use super::{
        GatewayContract, GithubBridgeFailureDiagnostic, GithubBridgeOperation, GithubBridgeRequest,
        GithubMcpGateway, GithubPublishedFile, GithubStatusCredential, GithubStatusReader,
        compatible_workflow, github_bridge_failure_diagnostic, mcp_tool_payload,
        normalize_repository, normalize_run_status, parse_response, pre_dispatch_provider_failure,
    };
    use reqwest::StatusCode;
    use steward_ports::PortError;

    fn read_json_request(listener: &TcpListener) -> Result<(TcpStream, serde_json::Value), String> {
        let (mut stream, _) = listener
            .accept()
            .map_err(|error| format!("accept MCP request: {error}"))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .map_err(|error| format!("bound MCP fixture read: {error}"))?;
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        let (header_end, content_length) = loop {
            let read = stream
                .read(&mut buffer)
                .map_err(|error| format!("read MCP request: {error}"))?;
            if read == 0 {
                return Err("MCP request ended before its body".to_owned());
            }
            request.extend_from_slice(&buffer[..read]);
            if let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                let header_end = header_end + 4;
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.split_once(':').and_then(|(name, value)| {
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                    })
                    .ok_or_else(|| "MCP request omitted content-length".to_owned())?;
                break (header_end, content_length);
            }
        };
        while request.len() < header_end + content_length {
            let read = stream
                .read(&mut buffer)
                .map_err(|error| format!("read MCP request body: {error}"))?;
            if read == 0 {
                return Err("MCP request body was truncated".to_owned());
            }
            request.extend_from_slice(&buffer[..read]);
        }
        let body = serde_json::from_slice(&request[header_end..header_end + content_length])
            .map_err(|error| format!("decode MCP request: {error}"))?;
        Ok((stream, body))
    }

    fn write_mcp_payload(
        mut stream: TcpStream,
        request_id: &str,
        payload: serde_json::Value,
    ) -> Result<(), String> {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {"structuredContent": payload, "isError": false}
        })
        .to_string();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
            response.len()
        )
        .map_err(|error| format!("write MCP response: {error}"))
    }

    #[test]
    fn start_request_rejects_non_allowlisted_redirect_and_unknown_fields() {
        let operation = GithubBridgeOperation::Start;
        assert_eq!(
            GithubBridgeRequest::parse(
                operation,
                br#"{"redirectAfter":"https://steward.example.test/connections#github-connected"}"#,
            ),
            Ok(GithubBridgeRequest::Start {
                redirect_after: "https://steward.example.test/connections#github-connected"
                    .to_owned(),
            })
        );
        for input in [
            br#"{}"#.as_slice(),
            br#"{"redirectAfter":"http://steward.example.test/connections"}"#.as_slice(),
            br#"{"redirectAfter":"https://steward.example.test/admin/connections"}"#.as_slice(),
            br#"{"redirectAfter":"https://steward.example.test/connections","runtimeUid":"other"}"#
                .as_slice(),
        ] {
            assert!(
                GithubBridgeRequest::parse(operation, input).is_err(),
                "start must reject a request that cannot be server-authored and allowlisted"
            );
        }
    }

    #[test]
    fn fixed_operations_reject_unknown_names_and_bodies() {
        assert!(GithubBridgeOperation::parse("github.delete").is_err());
        for operation in [
            GithubBridgeOperation::Status,
            GithubBridgeOperation::Disconnect,
        ] {
            assert!(
                GithubBridgeRequest::parse(
                    operation,
                    br#"{"redirectAfter":"https://steward.example.test/connections"}"#
                )
                .is_err(),
                "only github.start may receive one server-authored field"
            );
        }
    }

    #[test]
    fn github_rerun_is_an_explicit_allowlisted_bridge_operation() -> Result<(), String> {
        let operation = GithubBridgeOperation::parse("github.rerun")
            .map_err(|error| format!("parse rerun operation: {error:?}"))?;
        assert_eq!(
            GithubBridgeRequest::parse(
                operation,
                br#"{"owner":"example-org","repo":"example-repo","runId":12345}"#,
            )
            .map_err(|error| format!("parse rerun request: {error:?}"))?,
            GithubBridgeRequest::Rerun {
                owner: "example-org".to_owned(),
                repo: "example-repo".to_owned(),
                run_id: 12345,
            }
        );
        for invalid in [
            br#"{"owner":"example-org","repo":"example-repo"}"#.as_slice(),
            br#"{"owner":"example-org","repo":"example-repo","runId":0}"#.as_slice(),
            br#"{"owner":"example-org","repo":"example-repo","runId":12345,"workflow":"other.yml"}"#.as_slice(),
            br#"{"owner":"../other","repo":"example-repo","runId":12345}"#.as_slice(),
        ] {
            assert!(GithubBridgeRequest::parse(operation, invalid).is_err());
        }
        for operation in [
            "github.repositories",
            "github.workflow",
            "github.run-status",
            "github.dispatch",
            "github.publish",
        ] {
            assert!(
                GithubBridgeOperation::parse(operation).is_ok(),
                "v4 operation {operation} must be explicitly allowlisted"
            );
        }
        Ok(())
    }

    #[test]
    fn repository_request_accepts_an_empty_query_for_the_authenticated_user_default()
    -> Result<(), String> {
        assert_eq!(
            GithubBridgeRequest::parse(
                GithubBridgeOperation::Repositories,
                br#"{"query":"","page":1,"perPage":100}"#,
            )
            .map_err(|error| format!("parse repository request: {error:?}"))?,
            GithubBridgeRequest::Repositories {
                query: String::new(),
                page: 1,
                per_page: 100,
            }
        );
        for invalid in [
            serde_json::json!({"query": "\n", "page": 1, "perPage": 100}),
            serde_json::json!({"query": "x".repeat(201), "page": 1, "perPage": 100}),
        ] {
            assert!(
                GithubBridgeRequest::parse(
                    GithubBridgeOperation::Repositories,
                    &serde_json::to_vec(&invalid)
                        .map_err(|error| format!("encode invalid repository request: {error}"))?,
                )
                .is_err(),
                "the empty default must not widen the bounded query contract"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn empty_repository_query_searches_only_the_authenticated_users_repositories()
    -> Result<(), String> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|error| format!("bind repository fixture: {error}"))?;
        let address = listener
            .local_addr()
            .map_err(|error| format!("read repository fixture address: {error}"))?;
        let server = thread::spawn(move || -> Result<(), String> {
            let (stream, me_request) = read_json_request(&listener)?;
            assert_eq!(me_request["params"]["name"], "get_me");
            assert_eq!(me_request["params"]["arguments"], serde_json::json!({}));
            write_mcp_payload(
                stream,
                "steward-github-me",
                serde_json::json!({"login": "alice"}),
            )?;

            let (stream, repositories_request) = read_json_request(&listener)?;
            assert_eq!(
                repositories_request["params"]["name"],
                "search_repositories"
            );
            assert_eq!(
                repositories_request["params"]["arguments"],
                serde_json::json!({"query": "user:alice", "page": 1, "perPage": 100})
            );
            write_mcp_payload(
                stream,
                "steward-github-repositories",
                serde_json::json!({"items": []}),
            )
        });
        let gateway = GithubMcpGateway::new(&format!("http://{address}"), "0.4.9")
            .map_err(|error| format!("build repository gateway: {error:?}"))?;
        let response = gateway
            .execute(
                GithubBridgeOperation::Repositories,
                GithubBridgeRequest::Repositories {
                    query: String::new(),
                    page: 1,
                    per_page: 100,
                },
            )
            .await
            .map_err(|error| format!("execute repository query: {error:?}"))?;
        server
            .join()
            .map_err(|_| "repository fixture panicked".to_owned())??;
        assert_eq!(response["login"], "alice");
        assert_eq!(response["repositories"], serde_json::json!([]));
        Ok(())
    }

    #[test]
    fn repository_automation_requests_reject_paths_branches_and_workflows_outside_the_contract() {
        assert!(
            GithubBridgeRequest::parse(
                GithubBridgeOperation::Workflow,
                br#"{"owner":"example-org","repo":"example-repo","path":"README.md","ref":"main","expectedContent":"content"}"#,
            )
            .is_err(),
            "workflow reads must stay under .github/workflows"
        );
        assert!(
            GithubBridgeRequest::parse(
                GithubBridgeOperation::Publish,
                br#"{"owner":"example-org","repo":"example-repo","baseBranch":"main","branch":"main","title":"title","body":"body","files":[{"path":".github/workflows/steward-task.yml","content":"workflow"},{"path":".steward/tasks/task/task-definition.json","content":"{}"}]}"#,
            )
            .is_err(),
            "publication must never target the default branch"
        );
        assert!(
            !compatible_workflow(
                "on:\n  workflow_dispatch:\njobs:\n  run:\n    uses: example-org/other/.github/workflows/task.yml@0123456789012345678901234567890123456789\n"
            ),
            "an arbitrary workflow_dispatch caller is not a Steward caller"
        );
    }

    #[test]
    fn publication_accepts_only_the_tested_package_closure_shapes() {
        let parse = |files: serde_json::Value| {
            GithubBridgeRequest::parse(
                GithubBridgeOperation::Publish,
                serde_json::json!({
                    "owner": "example-org",
                    "repo": "example-repo",
                    "baseBranch": "main",
                    "branch": "steward/task-0123",
                    "title": "title",
                    "body": "body",
                    "files": files,
                    "resumeOwnedBranch": false
                })
                .to_string()
                .as_bytes(),
            )
        };
        let file = |path: &str| serde_json::json!({"path": path, "content": "content"});
        let workflow = ".github/workflows/hypershell-task.yml";
        for accepted in [
            vec![workflow, ".steward/tasks/hello/task-definition.json"],
            vec![workflow, "task-definition.json", "prompt.md"],
            vec!["prompt.md", workflow, "task-definition.json"],
        ] {
            let files = accepted.iter().map(|path| file(path)).collect::<Vec<_>>();
            assert!(
                parse(serde_json::Value::Array(files)).is_ok(),
                "{accepted:?} is a tested package shape"
            );
        }
        for rejected in [
            vec![workflow],
            vec![workflow, "task-definition.json"],
            vec![workflow, "prompt.md"],
            vec![workflow, "task-definition.json", "README.md"],
            vec![
                workflow,
                "task-definition.json",
                ".steward/tasks/hello/prompt.md",
            ],
            vec![
                workflow,
                ".steward/tasks/hello/task-definition.json",
                ".steward/tasks/hello/prompt.md",
            ],
            vec![
                workflow,
                ".steward/tasks/hello/task-definition.json",
                "prompt.md",
            ],
            vec![workflow, "prompt.md", "notes/prompt.md"],
            vec![workflow, "task-definition.json", "prompt.md", "prompt.md"],
            vec![
                workflow,
                ".github/workflows/other.yml",
                "task-definition.json",
            ],
            vec![
                workflow,
                workflow,
                ".steward/tasks/hello/task-definition.json",
            ],
            vec![
                workflow,
                "task-definition.json",
                "nested/task-definition.json",
            ],
        ] {
            let files = rejected.iter().map(|path| file(path)).collect::<Vec<_>>();
            assert!(
                parse(serde_json::Value::Array(files)).is_err(),
                "{rejected:?} is outside the publication allowlist"
            );
        }
    }

    #[test]
    fn legacy_root_package_reads_report_only_byte_identical_content_as_compatible()
    -> Result<(), String> {
        for path in ["task-definition.json", "prompt.md"] {
            GithubBridgeRequest::parse(
                GithubBridgeOperation::Workflow,
                serde_json::json!({
                    "owner": "example-org",
                    "repo": "example-repo",
                    "path": path,
                    "ref": "main",
                    "expectedContent": "content"
                })
                .to_string()
                .as_bytes(),
            )
            .map_err(|error| format!("{path} base-branch read must parse: {error:?}"))?;
        }
        assert!(
            GithubBridgeRequest::parse(
                GithubBridgeOperation::Workflow,
                br#"{"owner":"example-org","repo":"example-repo","path":"README.md","ref":"main","expectedContent":"content"}"#,
            )
            .is_err(),
            "other root files stay unreadable through the workflow operation"
        );
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let read = |payload: serde_json::Value| {
            super::normalize_package_file(&payload, "task-definition.json", "tested\n")
                .map_err(|error| format!("normalize package read: {error:?}"))
        };
        let absent = read(serde_json::json!({"__stewardNotFound": true}))?;
        assert_eq!(
            (absent["exists"].clone(), absent["compatible"].clone()),
            (false.into(), false.into())
        );
        let same = read(serde_json::json!({"content": "tested\n", "sha": sha}))?;
        assert_eq!(
            (same["exists"].clone(), same["compatible"].clone()),
            (true.into(), true.into())
        );
        let different = read(serde_json::json!({"content": "{\"family\":\"web\"}", "sha": sha}))?;
        assert_eq!(
            (different["exists"].clone(), different["compatible"].clone()),
            (true.into(), false.into())
        );
        let unreadable = read(serde_json::json!({"sha": sha}))?;
        assert_eq!(
            (
                unreadable["exists"].clone(),
                unreadable["compatible"].clone()
            ),
            (true.into(), false.into())
        );
        Ok(())
    }

    #[test]
    fn publication_ownership_accepts_a_steward_commit_that_omits_unchanged_files() {
        let branch = "steward/task-0123";
        let expected = [
            "task-definition.json",
            "prompt.md",
            ".github/workflows/hypershell-task.yml",
        ]
        .map(|path| GithubPublishedFile {
            path: path.to_owned(),
            content: "content".to_owned(),
        });
        let commit = |files: &[&str]| {
            serde_json::json!({
                "commit": {"message": super::publication_commit_message(branch)},
                "files": files.iter().map(|path| serde_json::json!({"filename": path})).collect::<Vec<_>>(),
            })
        };
        assert!(super::steward_publication_head(
            &commit(&["prompt.md", ".github/workflows/hypershell-task.yml"]),
            branch,
            &expected
        ));
        assert!(super::steward_publication_head(
            &commit(&[
                "task-definition.json",
                "prompt.md",
                ".github/workflows/hypershell-task.yml"
            ]),
            branch,
            &expected
        ));
        assert!(!super::steward_publication_head(
            &commit(&[]),
            branch,
            &expected
        ));
        assert!(!super::steward_publication_head(
            &commit(&["prompt.md", "README.md"]),
            branch,
            &expected
        ));
        let mut foreign = commit(&["prompt.md"]);
        foreign["commit"]["message"] = serde_json::Value::String("chore: other".to_owned());
        assert!(!super::steward_publication_head(
            &foreign, branch, &expected
        ));
    }

    #[test]
    fn pinned_github_mcp_embedded_resource_preserves_file_content_and_blob_sha()
    -> Result<(), String> {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "fixture",
            "result": {
                "isError": false,
                "content": [
                    {
                        "type": "text",
                        "text": format!("successfully downloaded text file (SHA: {sha})")
                    },
                    {
                        "type": "resource",
                        "resource": {
                            "uri": format!("repo://example-org/example-repo/contents/task.yml?sha={sha}"),
                            "text": "name: governed\n"
                        }
                    }
                ]
            }
        });
        let payload = mcp_tool_payload(response.to_string().as_bytes(), "fixture")
            .map_err(|error| format!("parse pinned response: {error:?}"))?;
        assert_eq!(payload["content"], "name: governed\n");
        assert_eq!(payload["sha"], sha);

        let blob = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "blob-fixture",
            "result": {
                "isError": false,
                "content": [
                    {"type": "text", "text": format!("SHA: {sha}")},
                    {
                        "type": "resource",
                        "resource": {
                            "uri": "repo://example-org/example-repo/contents/task.yml",
                            "blob": "bmFtZTogZ292ZXJuZWQK"
                        }
                    }
                ]
            }
        });
        let payload = mcp_tool_payload(blob.to_string().as_bytes(), "blob-fixture")
            .map_err(|error| format!("parse pinned blob response: {error:?}"))?;
        assert_eq!(payload["content"], "name: governed\n");
        assert_eq!(payload["sha"], sha);
        Ok(())
    }

    #[test]
    fn pinned_repository_search_accepts_flat_owner_identity() -> Result<(), String> {
        let repository = normalize_repository(&serde_json::json!({
            "id": 123,
            "name": "example-repo",
            "full_name": "example-org/example-repo",
            "owner_id": 456,
            "default_branch": "main",
            "private": true,
            "html_url": "https://github.com/example-org/example-repo"
        }))
        .map_err(|error| format!("normalize pinned repository result: {error:?}"))?;
        assert_eq!(repository["owner"], "example-org");
        assert_eq!(repository["ownerId"], "456");
        assert_eq!(repository["repositoryId"], "123");
        Ok(())
    }

    #[test]
    fn run_status_preserves_the_signed_github_attempt_for_task_linking() -> Result<(), String> {
        let result = normalize_run_status(
            &serde_json::json!({
                "id": 12345,
                "run_attempt": 3,
                "status": "completed",
                "conclusion": "success",
                "html_url": "https://github.com/example-org/example-repo/actions/runs/12345"
            }),
            &serde_json::json!({"jobs": []}),
            12345,
            None,
        )
        .map_err(|error| format!("normalize run status: {error:?}"))?;
        assert_eq!(result["runAttempt"], 3);
        Ok(())
    }

    #[tokio::test]
    async fn dispatch_rechecks_the_exact_workflow_and_uses_official_actions_tool_arguments()
    -> Result<(), String> {
        let workflow = concat!(
            "on:\n",
            "  workflow_dispatch:\n",
            "    inputs:\n",
            "      message:\n",
            "jobs:\n",
            "  governed:\n",
            "    uses: example-org/steward-run/.github/workflows/steward-task.yml@0123456789012345678901234567890123456789\n"
        );
        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|error| format!("bind dispatch fixture: {error}"))?;
        let address = listener
            .local_addr()
            .map_err(|error| format!("read dispatch fixture address: {error}"))?;
        let expected_workflow = workflow.to_owned();
        let server = thread::spawn(move || -> Result<(), String> {
            let (stream, workflow_request) = read_json_request(&listener)?;
            assert_eq!(workflow_request["params"]["name"], "get_file_contents");
            assert_eq!(
                workflow_request["params"]["arguments"],
                serde_json::json!({
                    "owner": "example-org",
                    "repo": "example-repo",
                    "path": ".github/workflows/steward-browser-task.yml",
                    "ref": "main"
                })
            );
            write_mcp_payload(
                stream,
                "steward-github-dispatch-workflow",
                serde_json::json!({"content": expected_workflow}),
            )?;

            let (stream, before_request) = read_json_request(&listener)?;
            assert_eq!(before_request["params"]["name"], "actions_list");
            assert_eq!(
                before_request["params"]["arguments"],
                serde_json::json!({
                    "method": "list_workflow_runs",
                    "owner": "example-org",
                    "repo": "example-repo",
                    "resource_id": ".github/workflows/steward-browser-task.yml",
                    "workflow_runs_filter": {"branch": "main"},
                    "perPage": 10
                })
            );
            write_mcp_payload(
                stream,
                "steward-github-runs-before",
                serde_json::json!({"workflow_runs": [{"id": 100}]}),
            )?;

            let (stream, dispatch_request) = read_json_request(&listener)?;
            assert_eq!(dispatch_request["params"]["name"], "actions_run_trigger");
            assert_eq!(
                dispatch_request["params"]["arguments"],
                serde_json::json!({
                    "method": "run_workflow",
                    "owner": "example-org",
                    "repo": "example-repo",
                    "workflow_id": ".github/workflows/steward-browser-task.yml",
                    "ref": "main",
                    "inputs": {"message": "hello"}
                })
            );
            write_mcp_payload(stream, "steward-github-dispatch", serde_json::json!({}))?;

            let (stream, after_request) = read_json_request(&listener)?;
            assert_eq!(after_request["params"]["name"], "actions_list");
            write_mcp_payload(
                stream,
                "steward-github-runs-after",
                serde_json::json!({
                    "workflow_runs": [{
                        "id": 101,
                        "event": "workflow_dispatch",
                        "html_url": "https://github.com/example-org/example-repo/actions/runs/101"
                    }]
                }),
            )
        });
        let gateway = GithubMcpGateway::new(&format!("http://{address}"), "0.4.9")
            .map_err(|error| format!("build dispatch gateway: {error:?}"))?;
        let response = gateway
            .execute(
                GithubBridgeOperation::Dispatch,
                GithubBridgeRequest::Dispatch {
                    owner: "example-org".to_owned(),
                    repo: "example-repo".to_owned(),
                    workflow_id: ".github/workflows/steward-browser-task.yml".to_owned(),
                    git_ref: "main".to_owned(),
                    inputs: serde_json::Map::from_iter([(
                        "message".to_owned(),
                        serde_json::json!("hello"),
                    )]),
                    expected_content: workflow.to_owned(),
                },
            )
            .await
            .map_err(|error| format!("execute dispatch: {error:?}"))?;
        server
            .join()
            .map_err(|_| "dispatch fixture panicked".to_owned())??;
        assert_eq!(
            response,
            serde_json::json!({
                "runId": 101,
                "url": "https://github.com/example-org/example-repo/actions/runs/101"
            })
        );
        Ok(())
    }

    #[tokio::test]
    async fn publication_updates_only_its_proven_existing_open_pull_request() -> Result<(), String>
    {
        let branch =
            "steward/task-11111111111141118111111111111111-22222222222242228222222222222222";
        let workflow_path = ".github/workflows/steward-browser-task.yml";
        let package_path = ".steward/tasks/hello/task-definition.json";
        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|error| format!("bind publication fixture: {error}"))?;
        let address = listener
            .local_addr()
            .map_err(|error| format!("read publication fixture address: {error}"))?;
        let branch_for_server = branch.to_owned();
        let server = thread::spawn(move || -> Result<(), String> {
            let (stream, request) = read_json_request(&listener)?;
            assert_eq!(request["params"]["name"], "get_commit");
            assert_eq!(request["params"]["arguments"]["sha"], branch_for_server);
            write_mcp_payload(
                stream,
                "steward-github-publication-branch",
                serde_json::json!({
                    "sha": "1111111111111111111111111111111111111111",
                    "commit": {"message": format!("chore: publish Steward governed task on {branch_for_server}")},
                    "files": [
                        {"filename": workflow_path},
                        {"filename": package_path}
                    ]
                }),
            )?;

            let (stream, request) = read_json_request(&listener)?;
            assert_eq!(request["params"]["name"], "list_pull_requests");
            assert_eq!(
                request["params"]["arguments"]["head"],
                format!("example-org:{branch_for_server}")
            );
            write_mcp_payload(
                stream,
                "steward-github-publication-pr",
                serde_json::json!({"pull_requests": [{
                    "number": 17,
                    "html_url": "https://github.com/example-org/example-repo/pull/17",
                    "state": "open",
                    "head": {"ref": branch_for_server},
                    "base": {"ref": "main"}
                }]}),
            )?;

            let (stream, request) = read_json_request(&listener)?;
            assert_eq!(request["params"]["name"], "get_commit");
            assert_eq!(request["params"]["arguments"]["sha"], "main");
            write_mcp_payload(
                stream,
                "steward-github-publication-base",
                serde_json::json!({"sha": "0000000000000000000000000000000000000000"}),
            )?;

            for (path, content) in [
                (workflow_path, "old workflow"),
                (package_path, "old package"),
            ] {
                let (stream, request) = read_json_request(&listener)?;
                assert_eq!(request["params"]["name"], "get_file_contents");
                assert_eq!(request["params"]["arguments"]["path"], path);
                write_mcp_payload(
                    stream,
                    "steward-github-publication-file",
                    serde_json::json!({"content": content}),
                )?;
            }

            let (stream, request) = read_json_request(&listener)?;
            assert_eq!(request["params"]["name"], "push_files");
            assert_eq!(request["params"]["arguments"]["branch"], branch_for_server);
            assert_eq!(
                request["params"]["arguments"]["files"]
                    .as_array()
                    .map(Vec::len),
                Some(2)
            );
            write_mcp_payload(
                stream,
                "steward-github-push-files",
                serde_json::json!({"commit": {"sha": "2222222222222222222222222222222222222222"}}),
            )
        });
        let gateway = GithubMcpGateway::new(&format!("http://{address}"), "0.4.9")
            .map_err(|error| format!("build publication gateway: {error:?}"))?;
        let response = gateway
            .execute(
                GithubBridgeOperation::Publish,
                GithubBridgeRequest::Publish {
                    owner: "example-org".to_owned(),
                    repo: "example-repo".to_owned(),
                    base_branch: "main".to_owned(),
                    branch: branch.to_owned(),
                    title: "Publish governed task".to_owned(),
                    body: "Exact tested package".to_owned(),
                    files: vec![
                        GithubPublishedFile {
                            path: workflow_path.to_owned(),
                            content: "new workflow".to_owned(),
                        },
                        GithubPublishedFile {
                            path: package_path.to_owned(),
                            content: "new package".to_owned(),
                        },
                    ],
                    resume_owned_branch: true,
                },
            )
            .await
            .map_err(|error| format!("execute publication update: {error:?}"))?;
        server
            .join()
            .map_err(|_| "publication fixture panicked".to_owned())??;
        assert_eq!(response["pullRequestNumber"], 17);
        assert_eq!(response["branch"], branch);
        Ok(())
    }

    #[tokio::test]
    async fn github_rerun_calls_only_the_exact_actions_tool_and_discards_provider_output()
    -> Result<(), String> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|error| format!("bind rerun fixture: {error}"))?;
        let address = listener
            .local_addr()
            .map_err(|error| format!("read rerun fixture address: {error}"))?;
        let server = thread::spawn(move || -> Result<(), String> {
            let (mut stream, _) = listener
                .accept()
                .map_err(|error| format!("accept rerun request: {error}"))?;
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .map_err(|error| format!("bound rerun fixture read: {error}"))?;
            let mut request = [0_u8; 8192];
            let read = stream
                .read(&mut request)
                .map_err(|error| format!("read rerun request: {error}"))?;
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.starts_with("POST /mcp HTTP/1.1\r\n"));
            let lower = request.to_ascii_lowercase();
            assert!(
                lower.contains("\r\nauthorization: bearer openshell-token-grant-placeholder\r\n")
            );
            assert!(lower.contains("\r\nmcp-protocol-version: 2025-06-18\r\n"));
            let body = request
                .split_once("\r\n\r\n")
                .map(|(_, body)| body)
                .ok_or_else(|| "rerun request omitted its body".to_owned())?;
            let body: serde_json::Value = serde_json::from_str(body)
                .map_err(|error| format!("decode rerun request: {error}"))?;
            assert_eq!(
                body,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": "steward-github-rerun",
                    "method": "tools/call",
                    "params": {
                        "name": "actions_run_trigger",
                        "arguments": {
                            "method": "rerun_workflow_run",
                            "owner": "example-org",
                            "repo": "example-repo",
                            "run_id": 12345,
                        }
                    }
                })
            );
            let response = r#"{"jsonrpc":"2.0","id":"steward-github-rerun","result":{"content":[{"type":"text","text":"provider detail"}],"isError":false}}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                response.len()
            )
            .map_err(|error| format!("write rerun response: {error}"))?;
            Ok(())
        });
        let gateway = GithubMcpGateway::new(&format!("http://{address}"), "0.4.9")
            .map_err(|error| format!("build rerun gateway: {error:?}"))?;
        let response = gateway
            .execute(
                GithubBridgeOperation::Rerun,
                GithubBridgeRequest::Rerun {
                    owner: "example-org".to_owned(),
                    repo: "example-repo".to_owned(),
                    run_id: 12345,
                },
            )
            .await
            .map_err(|error| format!("execute rerun: {error:?}"))?;
        server
            .join()
            .map_err(|_| "rerun fixture panicked".to_owned())??;
        assert_eq!(response, serde_json::json!({"dispatched": true}));
        Ok(())
    }

    #[test]
    fn github_rerun_accepts_one_streamable_http_event_and_rejects_ambiguous_events() {
        let payload = r#"{"jsonrpc":"2.0","id":"steward-github-rerun","result":{"content":[],"isError":false}}"#;
        let event = format!("event: message\ndata: {payload}\n\n");
        assert_eq!(
            parse_response(
                GatewayContract::LifecycleV049,
                GithubBridgeOperation::Rerun,
                StatusCode::OK,
                event.as_bytes(),
            ),
            Ok(serde_json::json!({"dispatched": true}))
        );

        let repeated = format!("data: {payload}\n\ndata: {payload}\n\n");
        assert!(
            parse_response(
                GatewayContract::LifecycleV049,
                GithubBridgeOperation::Rerun,
                StatusCode::OK,
                repeated.as_bytes(),
            )
            .is_err(),
            "more than one SSE data event must not be treated as one rerun result"
        );
    }

    #[test]
    fn bridge_responses_accept_the_disconnected_mcp_gw_contract_and_reject_malformed_values() {
        assert!(
            parse_response(
                GatewayContract::LegacyV032,
                GithubBridgeOperation::Status,
                StatusCode::OK,
                br#"{"connected":false}"#,
            )
            .is_ok(),
            "MCP-GW reports an absent or revoked account as only connected=false"
        );
        assert!(
            parse_response(
                GatewayContract::LegacyV032,
                GithubBridgeOperation::Status,
                StatusCode::OK,
                br#"{"connected":"false"}"#,
            )
            .is_err(),
            "a status with a non-boolean connection state cannot become a persisted response"
        );
        assert!(
            parse_response(
                GatewayContract::LegacyV032,
                GithubBridgeOperation::Status,
                StatusCode::OK,
                br#"{"connected":true}"#,
            )
            .is_err(),
            "a connected status without its verified account identity must fail closed"
        );
        assert!(
            parse_response(
                GatewayContract::LegacyV032,
                GithubBridgeOperation::Start,
                StatusCode::OK,
                br#"{"authorizationUrl":"http://github.example.test/authorize"}"#,
            )
            .is_err(),
            "a non-HTTPS authorization URL must never reach the browser"
        );
        assert!(
            parse_response(
                GatewayContract::LegacyV032,
                GithubBridgeOperation::Disconnect,
                StatusCode::OK,
                b"",
            )
            .is_err(),
            "disconnect is only complete on the exact MCP-GW no-content response"
        );
    }

    #[test]
    fn normalized_status_accepts_additive_fields_without_forwarding_them() -> Result<(), String> {
        let status = br#"{"version":"1","provider":"github","phase":"connected","connected":true,"account":{"displayName":"alice@example.com"},"requiredScopes":["repo"],"grantedScopes":["repo"],"missingScopes":[],"activeCredentialExpiresAt":"2026-09-15T12:00:00.000Z","renewalCredentialExpiresAt":"2026-09-16T12:00:00.000Z","lastAuthorizedAt":"2026-09-14T12:00:00.000Z","lastRenewedAt":null,"lastValidatedAt":null,"capabilities":{"interactiveAuthorization":true,"activeCredentialExpiry":true,"automaticRenewal":true,"manualRenewal":true,"rotatingRenewalCredential":true,"providerValidation":true,"providerRevocation":true,"scopeReporting":true,"identityVerification":true}}"#;
        let mut escaped: serde_json::Value = serde_json::from_slice(status)
            .map_err(|error| format!("fixed neutral status fixture is invalid: {error}"))?;
        escaped["activeCredentialPresent"] = serde_json::json!(true);
        escaped["renewalCredentialPresent"] = serde_json::json!(true);
        escaped["statusUpdatedAt"] = serde_json::json!("2026-09-14T12:30:00.000Z");
        escaped["activeCredential"] = serde_json::json!("fake-secret");
        escaped["capabilities"]["statusMetadata"] = serde_json::json!(true);
        assert_eq!(
            parse_response(
                GatewayContract::LifecycleV049,
                GithubBridgeOperation::Status,
                StatusCode::OK,
                escaped.to_string().as_bytes(),
            ),
            Ok(serde_json::json!({
                "phase": "connected",
                "connected": true,
                "email": "alice@example.com",
                "scopesRequired": ["repo"],
                "scopesGranted": ["repo"],
                "missingScopes": [],
                "activeCredentialExpiresAt": "2026-09-15T12:00:00.000Z",
                "renewalCredentialExpiresAt": "2026-09-16T12:00:00.000Z"
            })),
            "additive fields must not break status, and unconsumed values must not enter the governed Task archive"
        );

        assert_eq!(
            parse_response(
                GatewayContract::LifecycleV049,
                GithubBridgeOperation::Status,
                StatusCode::OK,
                status,
            ),
            Ok(serde_json::json!({
                "phase": "connected",
                "connected": true,
                "email": "alice@example.com",
                "scopesRequired": ["repo"],
                "scopesGranted": ["repo"],
                "missingScopes": [],
                "activeCredentialExpiresAt": "2026-09-15T12:00:00.000Z",
                "renewalCredentialExpiresAt": "2026-09-16T12:00:00.000Z"
            }))
        );
        Ok(())
    }

    #[test]
    fn normalized_v2_status_accepts_only_a_canonical_numeric_account_id() -> Result<(), String> {
        let status = |id: Option<&str>| {
            let mut account = serde_json::json!({
                "provider": "github",
                "displayName": "alice@example.com"
            });
            if let Some(id) = id {
                account["id"] = serde_json::json!(id);
                account["login"] = serde_json::json!("mutable-login");
            }
            serde_json::json!({
                "version": "2",
                "provider": "github",
                "phase": "connected",
                "connected": true,
                "account": account,
                "requiredScopes": ["repo"],
                "grantedScopes": ["repo"],
                "missingScopes": [],
                "activeCredentialExpiresAt": null,
                "renewalCredentialExpiresAt": null,
                "lastAuthorizedAt": null,
                "lastRenewedAt": null,
                "lastValidatedAt": null,
                "capabilities": {"interactiveAuthorization": true}
            })
        };

        let projected = parse_response(
            GatewayContract::LifecycleV049,
            GithubBridgeOperation::Status,
            StatusCode::OK,
            status(Some("123456")).to_string().as_bytes(),
        )
        .map_err(|error| format!("valid v2 status was rejected: {error:?}"))?;
        assert_eq!(projected["accountId"], "123456");
        assert_eq!(projected["accountLogin"], "mutable-login");
        let mut identity_without_login = status(Some("123456"));
        identity_without_login["account"]
            .as_object_mut()
            .ok_or_else(|| "fixed v2 account fixture is not an object".to_owned())?
            .remove("login");
        identity_without_login["account"]["futureDisplayField"] = serde_json::json!("ignored");
        let projected_without_login = parse_response(
            GatewayContract::LifecycleV049,
            GithubBridgeOperation::Status,
            StatusCode::OK,
            identity_without_login.to_string().as_bytes(),
        )
        .map_err(|error| format!("numeric ID without mutable login was rejected: {error:?}"))?;
        assert_eq!(projected_without_login["accountId"], "123456");
        assert!(projected_without_login.get("accountLogin").is_none());
        assert!(
            parse_response(
                GatewayContract::LifecycleV049,
                GithubBridgeOperation::Status,
                StatusCode::OK,
                status(None).to_string().as_bytes(),
            )
            .is_ok(),
            "a best-effort MCP-GW backfill failure must leave status readable"
        );
        for invalid in ["0", "012345", "123456789012345678901", "123x"] {
            assert!(
                parse_response(
                    GatewayContract::LifecycleV049,
                    GithubBridgeOperation::Status,
                    StatusCode::OK,
                    status(Some(invalid)).to_string().as_bytes(),
                )
                .is_err(),
                "invalid GitHub account ID {invalid} must not become identity evidence"
            );
        }
        Ok(())
    }

    #[test]
    fn provider_control_http_failures_are_reduced_to_non_secret_runtime_categories() {
        assert_eq!(
            parse_response(
                GatewayContract::LegacyV032,
                GithubBridgeOperation::Status,
                StatusCode::UNAUTHORIZED,
                b"ignored",
            ),
            Err(PortError::Failed {
                reason: "MCP-GW rejected runtime authentication".to_owned(),
            })
        );
        assert_eq!(
            parse_response(
                GatewayContract::LegacyV032,
                GithubBridgeOperation::Status,
                StatusCode::FORBIDDEN,
                b"ignored",
            ),
            Err(PortError::Failed {
                reason: "MCP-GW rejected runtime authorization".to_owned(),
            })
        );
        for body in [
            br#"{"error":"policy_denied"}"#.as_slice(),
            br#"{"error":"ssrf_denied"}"#.as_slice(),
        ] {
            assert_eq!(
                parse_response(
                    GatewayContract::LegacyV032,
                    GithubBridgeOperation::Status,
                    StatusCode::FORBIDDEN,
                    body,
                ),
                Err(PortError::Failed {
                    reason: "OpenShell proxy denied the provider request".to_owned(),
                }),
                "a proxy policy denial must remain distinct from MCP-GW authorization"
            );
        }
        assert_eq!(
            parse_response(
                GatewayContract::LegacyV032,
                GithubBridgeOperation::Status,
                StatusCode::BAD_GATEWAY,
                br#"{"error":"token_grant_failed","detail":"dynamic token grant failed"}"#,
            ),
            Err(PortError::CredentialGrantFailed)
        );
        assert_eq!(
            parse_response(
                GatewayContract::LegacyV032,
                GithubBridgeOperation::Status,
                StatusCode::BAD_GATEWAY,
                b"ignored",
            ),
            Err(PortError::Failed {
                reason: "MCP-GW returned HTTP 502".to_owned(),
            })
        );
        assert_eq!(
            parse_response(
                GatewayContract::LifecycleV049,
                GithubBridgeOperation::Start,
                StatusCode::BAD_REQUEST,
                br#"{"error":"OAuth redirect target is not allowed"}"#,
            ),
            Err(PortError::Failed {
                reason: "MCP-GW returned HTTP 400 (OAuth redirect target is not allowed)"
                    .to_owned(),
            }),
            "a bounded error field must preserve the actionable gateway rejection"
        );
        assert_eq!(
            parse_response(
                GatewayContract::LifecycleV049,
                GithubBridgeOperation::Start,
                StatusCode::BAD_REQUEST,
                br#"{"error":"OAuth redirect target is not allowed","code":"oauth_redirect_target_not_allowed"}"#,
            ),
            Err(PortError::Failed {
                reason: "MCP-GW returned HTTP 400 [code=oauth_redirect_target_not_allowed] (OAuth redirect target is not allowed)"
                    .to_owned(),
            }),
            "the stable provider code and bounded human explanation must both survive"
        );
        assert_eq!(
            parse_response(
                GatewayContract::LifecycleV049,
                GithubBridgeOperation::Start,
                StatusCode::BAD_REQUEST,
                br#"{"error":"see https://provider.example.test/callback?token=obviously-fake-secret"}"#,
            ),
            Err(PortError::Failed {
                reason: "MCP-GW returned HTTP 400".to_owned(),
            }),
            "query-bearing URLs and token material must never enter the diagnostic"
        );
        let oversized = serde_json::json!({"code": "a".repeat(201)}).to_string();
        assert_eq!(
            parse_response(
                GatewayContract::LifecycleV049,
                GithubBridgeOperation::Start,
                StatusCode::BAD_REQUEST,
                oversized.as_bytes(),
            ),
            Err(PortError::Failed {
                reason: "MCP-GW returned HTTP 400".to_owned(),
            }),
            "an oversized machine code must be discarded rather than truncated into another code"
        );
    }

    #[test]
    fn bridge_failure_diagnostic_accepts_only_the_fixed_sanitized_line() {
        let diagnostic =
            github_bridge_failure_diagnostic(
                b"steward-connections-bridge: bridge MCP-GW returned HTTP 400 (OAuth redirect target is not allowed)\n"
            );
        let expected = GithubBridgeFailureDiagnostic {
            status: 400,
            code: None,
            reason: Some("OAuth redirect target is not allowed".to_owned()),
        };
        assert_eq!(diagnostic, Some(expected.clone()));
        assert_eq!(
            diagnostic
                .as_ref()
                .and_then(|diagnostic| GithubBridgeFailureDiagnostic::from_value(
                    &diagnostic.to_value()
                )),
            Some(expected),
            "the persisted projection must round trip only the bounded status and reason"
        );
        assert_eq!(
            github_bridge_failure_diagnostic(
                b"steward-connections-bridge: bridge MCP-GW returned HTTP 503\n"
            ),
            Some(GithubBridgeFailureDiagnostic {
                status: 503,
                code: None,
                reason: None,
            })
        );
        assert_eq!(
            github_bridge_failure_diagnostic(
                b"steward-connections-bridge: bridge MCP-GW returned HTTP 400 [code=oauth_redirect_target_not_allowed] (OAuth redirect target is not allowed)\n"
            ),
            Some(GithubBridgeFailureDiagnostic {
                status: 400,
                code: Some("oauth_redirect_target_not_allowed".to_owned()),
                reason: Some("OAuth redirect target is not allowed".to_owned()),
            }),
            "the fixed bridge line must retain both safe MCP-GW error fields"
        );
        for hostile in [
            b"prefix steward-connections-bridge: bridge MCP-GW returned HTTP 400 (reason)"
                .as_slice(),
            b"steward-connections-bridge: bridge MCP-GW returned HTTP 99 (reason)".as_slice(),
            b"steward-connections-bridge: bridge MCP-GW returned HTTP 400 (https://provider.example.test/callback?token=obviously-fake-secret)".as_slice(),
            b"steward-connections-bridge: bridge MCP-GW returned HTTP 400 (authorization: Bearer obviously-fake-secret)".as_slice(),
            b"steward-connections-bridge: bridge MCP-GW returned HTTP 400 (aaaaaaaa.bbbbbbbb.cccccccc)".as_slice(),
            b"steward-connections-bridge: bridge MCP-GW returned HTTP 400 (/oauth/callback?code=abc&state=xyz)".as_slice(),
            b"steward-connections-bridge: bridge MCP-GW returned HTTP 400 (gh.example/cb?code=abc&state=xyz)".as_slice(),
            b"steward-connections-bridge: bridge MCP-GW returned HTTP 400 (code=abc)".as_slice(),
            b"steward-connections-bridge: bridge MCP-GW returned HTTP 400 (state=xyz)".as_slice(),
            b"steward-connections-bridge: bridge MCP-GW returned HTTP 400 (ghp_1234567890abcdef)".as_slice(),
            b"steward-connections-bridge: bridge MCP-GW returned HTTP 400 (gho_1234567890abcdef)".as_slice(),
            b"steward-connections-bridge: bridge MCP-GW returned HTTP 400 (token abcdefghijklmnop)".as_slice(),
            b"steward-connections-bridge: bridge MCP-GW returned HTTP 400 (Authorization : abc)".as_slice(),
            b"steward-connections-bridge: bridge MCP-GW returned HTTP 400 (Cookie: session=abc)".as_slice(),
            b"steward-connections-bridge: bridge MCP-GW returned HTTP 400 (set-cookie session=abc)".as_slice(),
            b"steward-connections-bridge: bridge MCP-GW returned HTTP 400 (alice@example.com)".as_slice(),
            b"steward-connections-bridge: bridge MCP-GW returned HTTP 400 (prefix.aaaaaaaa.bbbbbbbb.cccccccc)".as_slice(),
            b"steward-connections-bridge: bridge MCP-GW returned HTTP 400 (aaaaaaaa.bbbbbbbb.cccccccc.dddddddd.eeeeeeee)".as_slice(),
            b"steward-connections-bridge: bridge MCP-GW returned HTTP 400 (request Abcdefghijklmnop12345 rejected)".as_slice(),
            "steward-connections-bridge: bridge MCP-GW returned HTTP 400 (unsafe\u{2028}detail)".as_bytes(),
        ] {
            assert_eq!(
                github_bridge_failure_diagnostic(hostile),
                None,
                "untrusted bridge stderr must not become a persisted diagnostic"
            );
        }

        let userinfo = format!(
            "steward-connections-bridge: bridge MCP-GW returned HTTP 400 ({}{}{}:{}{}host.example.test/path)",
            "https", "://", "user", "pw", "@"
        );
        assert_eq!(
            github_bridge_failure_diagnostic(userinfo.as_bytes()),
            None,
            "URL userinfo must never become a persisted diagnostic"
        );

        let trailing = serde_json::json!({"error": format!("{} zzz", "a".repeat(199))}).to_string();
        assert_eq!(
            parse_response(
                GatewayContract::LifecycleV049,
                GithubBridgeOperation::Start,
                StatusCode::BAD_REQUEST,
                trailing.as_bytes(),
            ),
            Err(PortError::Failed {
                reason: format!("MCP-GW returned HTTP 400 ({})", "a".repeat(199)),
            }),
            "truncation must not retain trailing whitespace"
        );
    }

    #[tokio::test]
    async fn direct_status_uses_one_exact_authenticated_get_without_a_runtime_retry()
    -> Result<(), String> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|error| format!("bind direct status fixture: {error}"))?;
        let address = listener
            .local_addr()
            .map_err(|error| format!("read direct status fixture address: {error}"))?;
        let server = thread::spawn(move || -> Result<(), String> {
            let (mut stream, _) = listener
                .accept()
                .map_err(|error| format!("accept direct status request: {error}"))?;
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .map_err(|error| format!("bound direct status fixture read: {error}"))?;
            let mut request = [0_u8; 4096];
            let read = stream
                .read(&mut request)
                .map_err(|error| format!("read direct status request: {error}"))?;
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(
                request.starts_with("GET /connections/github/status HTTP/1.1\r\n"),
                "the direct reader must expose only the lifecycle metadata route"
            );
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("\r\nauthorization: bearer aaa.bbb.ccc\r\n"),
                "the direct reader must present the one-request HOP-1 credential"
            );
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("\r\naccept: application/vnd.apelogic.connection-status.v2+json\r\n"),
                "the direct reader must explicitly request stable GitHub account identity"
            );
            let body = r#"{"version":"2","provider":"github","phase":"connected","connected":true,"account":{"provider":"github","id":"123456","login":"alice","displayName":"alice@example.com"},"requiredScopes":["repo"],"grantedScopes":["repo"],"missingScopes":[],"activeCredentialExpiresAt":null,"renewalCredentialExpiresAt":null,"lastAuthorizedAt":null,"lastRenewedAt":null,"lastValidatedAt":null,"capabilities":{"interactiveAuthorization":true}}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .map_err(|error| format!("write direct status response: {error}"))?;
            Ok(())
        });

        let reader = GithubStatusReader::new(&format!("http://{address}"), "0.4.9")
            .map_err(|error| format!("build direct status reader: {error:?}"))?;
        let credential = GithubStatusCredential::new("aaa.bbb.ccc".to_owned())
            .map_err(|error| format!("build direct status credential: {error:?}"))?;
        let status = reader
            .read(&credential)
            .await
            .map_err(|error| format!("read direct status: {error:?}"))?;

        server
            .join()
            .map_err(|_| "direct status fixture panicked".to_owned())??;
        assert_eq!(
            status,
            serde_json::json!({
                "phase": "connected",
                "connected": true,
                "accountId": "123456",
                "accountLogin": "alice",
                "email": "alice@example.com",
                "scopesRequired": ["repo"],
                "scopesGranted": ["repo"],
                "missingScopes": [],
                "activeCredentialExpiresAt": null,
                "renewalCredentialExpiresAt": null
            })
        );
        Ok(())
    }

    #[tokio::test]
    async fn oversized_gateway_failure_keeps_its_http_status() -> Result<(), String> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|error| format!("bind oversized response fixture: {error}"))?;
        let address = listener
            .local_addr()
            .map_err(|error| format!("read oversized response fixture address: {error}"))?;
        let server = thread::spawn(move || -> Result<(), String> {
            let (mut stream, _) = listener
                .accept()
                .map_err(|error| format!("accept oversized response request: {error}"))?;
            let mut request = [0_u8; 4096];
            let _ = stream
                .read(&mut request)
                .map_err(|error| format!("read oversized response request: {error}"))?;
            let body = "x".repeat(super::MAX_RESPONSE_BYTES + 1);
            write!(
                stream,
                "HTTP/1.1 429 Too Many Requests\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .map_err(|error| format!("write oversized response: {error}"))?;
            Ok(())
        });
        let gateway = GithubMcpGateway::new(&format!("http://{address}"), "0.4.9")
            .map_err(|error| format!("build oversized response gateway: {error:?}"))?;
        let result = gateway
            .execute(GithubBridgeOperation::Status, GithubBridgeRequest::Empty)
            .await;
        server
            .join()
            .map_err(|_| "oversized response fixture panicked".to_owned())??;
        assert_eq!(
            result,
            Err(PortError::Failed {
                reason: "MCP-GW returned HTTP 429".to_owned(),
            }),
            "discarding an oversized body must not discard the upstream status"
        );
        Ok(())
    }

    #[test]
    fn direct_status_credential_requires_exactly_three_compact_jwt_segments() {
        for invalid in ["aaa.bbb", "aaa.bbb.ccc.ddd", "aaa..ccc", "aaa.bbb.ccc="] {
            assert!(
                GithubStatusCredential::new(invalid.to_owned()).is_err(),
                "direct status credential must reject {invalid}"
            );
        }
        assert!(GithubStatusCredential::new("aaa.bbb.ccc".to_owned()).is_ok());
    }

    #[tokio::test]
    async fn provider_control_waits_for_the_openshell_provider_transport_to_become_ready()
    -> Result<(), String> {
        let reservation = TcpListener::bind("127.0.0.1:0")
            .map_err(|error| format!("reserve a loopback MCP-GW fixture address: {error}"))?;
        let address = reservation
            .local_addr()
            .map_err(|error| format!("read the loopback fixture address: {error}"))?;
        drop(reservation);
        let server = thread::spawn(move || -> Result<(), String> {
            thread::sleep(Duration::from_millis(300));
            let listener = TcpListener::bind(address).map_err(|error| {
                format!("bind the delayed MCP-GW fixture at its reserved address: {error}")
            })?;
            let (mut stream, _) = listener
                .accept()
                .map_err(|error| format!("accept the retried request: {error}"))?;
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .map_err(|error| format!("bound the fixture read: {error}"))?;
            let mut request = [0_u8; 2048];
            let read = stream
                .read(&mut request)
                .map_err(|error| format!("read the retried request: {error}"))?;
            assert!(
                String::from_utf8_lossy(&request[..read]).starts_with("GET /oauth/github/status "),
                "the readiness retry must preserve the exact provider-control request"
            );
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 19\r\nConnection: close\r\n\r\n{\"connected\":false}",
                )
                .map_err(|error| format!("write the delayed MCP-GW response: {error}"))?;
            Ok(())
        });
        let gateway = GithubMcpGateway::new(&format!("http://{address}"), "0.3.2")
            .map_err(|error| format!("build the MCP-GW fixture adapter: {error:?}"))?;

        let response = gateway
            .execute(GithubBridgeOperation::Status, GithubBridgeRequest::Empty)
            .await
            .map_err(|error| {
                format!("the bridge must tolerate the bounded provider-readiness race: {error:?}")
            })?;

        server
            .join()
            .map_err(|_| "MCP-GW fixture thread panicked".to_owned())??;
        assert_eq!(response, serde_json::json!({"connected": false}));
        Ok(())
    }

    #[tokio::test]
    async fn provider_control_retries_only_a_pre_dispatch_token_grant_failure() -> Result<(), String>
    {
        let exact = br#"{"error":"token_grant_failed","detail":"dynamic token grant failed"}"#;
        assert!(pre_dispatch_provider_failure(
            StatusCode::BAD_GATEWAY,
            exact
        ));
        assert!(
            !pre_dispatch_provider_failure(StatusCode::BAD_GATEWAY, b""),
            "an indistinguishable generic upstream 502 must not retry a mutation"
        );
        assert!(
            !pre_dispatch_provider_failure(
                StatusCode::BAD_GATEWAY,
                br#"{"error":"upstream_unreachable","detail":"connection failed"}"#,
            ),
            "a different OpenShell 502 must not retry a mutation"
        );
        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|error| format!("bind the OpenShell token-grant fixture: {error}"))?;
        let address = listener
            .local_addr()
            .map_err(|error| format!("read the token-grant fixture address: {error}"))?;
        let server = thread::spawn(move || -> Result<(), String> {
            for response in [
                b"HTTP/1.1 502 Bad Gateway\r\nContent-Type: application/json\r\nContent-Length: 68\r\nConnection: close\r\n\r\n{\"error\":\"token_grant_failed\",\"detail\":\"dynamic token grant failed\"}"
                    .as_slice(),
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 19\r\nConnection: close\r\n\r\n{\"connected\":false}"
                    .as_slice(),
            ] {
                let (mut stream, _) = listener
                    .accept()
                    .map_err(|error| format!("accept provider request: {error}"))?;
                let mut request = [0_u8; 2048];
                stream
                    .read(&mut request)
                    .map_err(|error| format!("read provider request: {error}"))?;
                stream
                    .write_all(response)
                    .map_err(|error| format!("write provider response: {error}"))?;
            }
            Ok(())
        });
        let gateway = GithubMcpGateway::new(&format!("http://{address}"), "0.3.2")
            .map_err(|error| format!("build the MCP-GW fixture adapter: {error:?}"))?;

        let result = gateway
            .execute(GithubBridgeOperation::Status, GithubBridgeRequest::Empty)
            .await;
        if result.is_err() {
            let _ = TcpStream::connect(address);
        }
        server
            .join()
            .map_err(|_| "token-grant fixture thread panicked".to_owned())??;

        assert_eq!(
            result,
            Ok(serde_json::json!({"connected": false})),
            "OpenShell's exact synthetic token-grant 502 produced before dispatch must be retried"
        );
        Ok(())
    }
}
