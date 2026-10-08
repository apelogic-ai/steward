//! MCP-GW adapter for the one-shot, provider-attached Connections bridge.
//!
//! The bridge sends OpenShell's documented bearer placeholder only. The sandbox
//! supervisor replaces it at the governed egress boundary; this adapter never
//! accepts or reads a credential, provider origin, or caller identity.

use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderValue};
use reqwest::{Client, Method, StatusCode, Url};
use serde_json::{Map, Value, json};
use steward_ports::PortError;

pub const IMPLEMENTED_PORTS: [&str; 0] = [];
const OPEN_SHELL_BEARER_PLACEHOLDER: &str = "openshell-token-grant-placeholder";
const MAX_RESPONSE_BYTES: usize = 32 * 1024;
// GitHub MCP tool results are full GitHub API objects; a page of workflow runs is
// several hundred KiB, so tool calls get their own bound.
const MAX_MCP_TOOL_RESPONSE_BYTES: usize = 1024 * 1024;
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
// A server may negotiate down to a version whose tool-call wire is the same.
const SUPPORTED_MCP_PROTOCOL_VERSIONS: [&str; 2] = [MCP_PROTOCOL_VERSION, "2025-03-26"];
const MCP_PROTOCOL_VERSION_HEADER: &str = "MCP-Protocol-Version";
const MCP_SESSION_ID_HEADER: &str = "Mcp-Session-Id";
const MCP_INITIALIZE_REQUEST_ID: &str = "steward-mcp-initialize";
const MCP_CLIENT_NAME: &str = "steward-connections-bridge";
const MAX_MCP_SESSION_ID_BYTES: usize = 4096;
const MCP_SESSION_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
const MCP_SESSION_FAILURE: &str = "MCP-GW session could not be established";
const RERUN_REQUEST_ID: &str = "steward-github-rerun";
const MAX_WORKFLOW_BYTES: usize = 256 * 1024;
// `https://github.com/` plus a 39-byte owner and a 100-byte name is 159 bytes; run
// and job URLs add `/actions/runs/<id>/job/<id>`.
const MAX_GITHUB_URL_BYTES: usize = 255;
// The pinned server's effective default page before it honored `perPage`; it keeps
// the normalized run status within its bridge result bound.
const MAX_RUN_STATUS_JOBS: usize = 30;
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
                let path = workflow_path_field(&object, "path")?;
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

fn workflow_file_name(path: &str) -> Result<&str, PortError> {
    path.strip_prefix(".github/workflows/")
        .filter(|_| valid_workflow_path(path))
        .ok_or_else(|| rejected("GitHub workflow path is invalid"))
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

fn published_files_field(
    object: &Map<String, Value>,
) -> Result<Vec<GithubPublishedFile>, PortError> {
    let files = object
        .get("files")
        .and_then(Value::as_array)
        .filter(|files| files.len() == 2)
        .ok_or_else(|| rejected("GitHub publication must contain exactly two generated files"))?;
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
    if parsed[0].path == parsed[1].path
        || parsed
            .iter()
            .filter(|file| valid_workflow_path(&file.path))
            .count()
            != 1
        || parsed
            .iter()
            .filter(|file| valid_package_path(&file.path))
            .count()
            != 1
    {
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
        let body = bounded_body_or_status(
            status,
            StatusCode::OK,
            read_bounded(response, MAX_RESPONSE_BYTES).await?,
        )?;
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
        if let (GithubBridgeOperation::Rerun, GithubBridgeRequest::Rerun { .. }) =
            (operation, &request)
        {
            let message = request.body().ok_or_else(|| {
                rejected("Connections bridge request does not match its allowlisted operation")
            })?;
            let mut session = McpSession::new(self);
            let result = match session.send(&message, MAX_RESPONSE_BYTES).await {
                Ok((status, body)) => parse_response(self.contract, operation, status, &body),
                Err(error) => Err(error),
            };
            session.close().await;
            return result;
        }
        if !matches!(
            (operation, &request),
            (
                GithubBridgeOperation::Status | GithubBridgeOperation::Disconnect,
                GithubBridgeRequest::Empty
            ) | (
                GithubBridgeOperation::Start,
                GithubBridgeRequest::Start { .. }
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
                read_bounded(response, MAX_RESPONSE_BYTES).await?,
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
        let mut session = McpSession::new(self);
        let result = Self::run_automation(&mut session, operation, request).await;
        session.close().await;
        result
    }

    async fn run_automation(
        mcp: &mut McpSession<'_>,
        operation: GithubBridgeOperation,
        request: GithubBridgeRequest,
    ) -> Result<Value, PortError> {
        match (operation, request) {
            (
                GithubBridgeOperation::Repositories,
                GithubBridgeRequest::Repositories {
                    query,
                    page,
                    per_page,
                },
            ) => {
                let me = mcp
                    .call_tool("steward-github-me", "get_me", json!({}))
                    .await?;
                // The default listing searches only the user's own repositories, so the
                // small minimal output suffices: its items omit the owner, whose stable ID
                // is the profile's. An explicit query can match repositories of other
                // owners, such as organizations, whose stable IDs only the full output
                // carries (about 5 KiB per item, within the tool response bound).
                let minimal_output = query.is_empty();
                let query = if minimal_output {
                    format!("user:{}", github_profile_login(&me)?)
                } else {
                    query
                };
                let repositories = mcp
                    .call_tool(
                        "steward-github-repositories",
                        "search_repositories",
                        json!({
                            "query": query,
                            "page": page,
                            "perPage": per_page,
                            "minimal_output": minimal_output,
                        }),
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
                let file = mcp
                    .call_tool(
                        "steward-github-workflow",
                        "get_file_contents",
                        json!({"owner": owner, "repo": repo, "path": path, "ref": git_ref}),
                    )
                    .await?;
                normalize_workflow(&file, &path, &expected_content)
            }
            (
                GithubBridgeOperation::RunStatus,
                GithubBridgeRequest::RunStatus {
                    owner,
                    repo,
                    run_id,
                },
            ) => {
                let run = mcp
                    .call_tool(
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
                let jobs = mcp
                    .call_tool(
                        "steward-github-jobs",
                        "actions_list",
                        json!({
                            "method": "list_workflow_jobs",
                            "owner": owner,
                            "repo": repo,
                            "resource_id": run_id.to_string(),
                            // v1.6.0 advertises `per_page` but reads `perPage`
                            // (`OptionalPaginationParams`); `per_page` is ignored.
                            "perPage": MAX_RUN_STATUS_JOBS,
                        }),
                    )
                    .await?;
                let failure_log = if run_conclusion(&run).is_some_and(|value| value != "success") {
                    mcp.call_tool(
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
                let workflow = mcp
                    .call_tool(
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
                // The tools resolve a non-numeric workflow ID with go-github's
                // `...ByFileName` helpers, which put it into the URL path unescaped,
                // so they need the file name, not the repository path.
                let workflow_file = workflow_file_name(&workflow_id)?;
                let before = mcp
                    .call_tool(
                        "steward-github-runs-before",
                        "actions_list",
                        json!({
                            "method": "list_workflow_runs",
                            "owner": owner,
                            "repo": repo,
                            "resource_id": workflow_file,
                            "workflow_runs_filter": {"branch": git_ref},
                            "perPage": 10,
                        }),
                    )
                    .await?;
                let previous_run_id = maximum_run_id(&before);
                let dispatched = mcp
                    .call_tool(
                        "steward-github-dispatch",
                        "actions_run_trigger",
                        json!({
                            "method": "run_workflow",
                            "owner": owner,
                            "repo": repo,
                            "workflow_id": workflow_file,
                            "ref": git_ref,
                            "inputs": inputs,
                        }),
                    )
                    .await?;
                // A tool error here is GitHub refusing this dispatch, not an MCP-GW
                // outage: report it as a definite rejection, never as retryable.
                require_write_success(&dispatched, "dispatch GitHub workflow")
                    .map_err(|_| rejected("GitHub rejected the workflow dispatch"))?;
                let deadline = Instant::now() + Duration::from_secs(20);
                loop {
                    let runs = mcp
                        .call_tool(
                            "steward-github-runs-after",
                            "actions_list",
                            json!({
                                "method": "list_workflow_runs",
                                "owner": owner,
                                "repo": repo,
                                "resource_id": workflow_file,
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
                    let branch_commit = mcp
                        .call_tool(
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
                        let pull_requests = mcp
                            .call_tool(
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

                        let base_commit = mcp
                            .call_tool(
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
                                let current = mcp
                                    .call_tool(
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
                    let created = mcp
                        .call_tool(
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
                    let pushed = mcp
                        .call_tool(
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
                let pull_request = mcp
                    .call_tool(
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

    /// One bounded MCP Streamable HTTP POST, retried only across the OpenShell
    /// provider-readiness race, exactly as tool calls were before sessions.
    async fn post_mcp(
        &self,
        message: &Value,
        session_id: Option<&str>,
        protocol_version: &str,
        expected: StatusCode,
        limit: usize,
    ) -> Result<McpReply, PortError> {
        let target = endpoint(&self.origin, MCP_PATH)?;
        let started = Instant::now();
        loop {
            let mut http = self
                .client
                .post(target.clone())
                .header(
                    AUTHORIZATION,
                    format!("Bearer {OPEN_SHELL_BEARER_PLACEHOLDER}"),
                )
                .header(MCP_PROTOCOL_VERSION_HEADER, protocol_version)
                .header(ACCEPT, "application/json, text/event-stream")
                .json(message);
            if let Some(session_id) = session_id {
                http = http.header(MCP_SESSION_ID_HEADER, session_id);
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
            let issued_session = response.headers().get(MCP_SESSION_ID_HEADER).cloned();
            let body =
                bounded_body_or_status(status, expected, read_bounded(response, limit).await?)?;
            if pre_dispatch_provider_failure(status, &body)
                && started.elapsed() < PROVIDER_TRANSPORT_READY_TIMEOUT
            {
                tokio::time::sleep(PROVIDER_TRANSPORT_RETRY_INTERVAL).await;
                continue;
            }
            return Ok(McpReply {
                status,
                session_id: issued_session,
                body,
            });
        }
    }
}

/// The bridge's MCP Streamable HTTP client state for one operation run.
///
/// A gateway such as the MCP-GW agentgateway issues an `Mcp-Session-Id` from
/// `initialize` and rejects every later request without it. A direct GitHub
/// wrapper issues none, so the bridge then sends none. Every tool call of one
/// operation reuses the one session, which is closed best-effort at the end.
struct McpSession<'a> {
    gateway: &'a GithubMcpGateway,
    established: Option<EstablishedMcpSession>,
    reinitialized: bool,
}

#[derive(Clone)]
struct EstablishedMcpSession {
    id: Option<String>,
    protocol_version: &'static str,
}

struct McpReply {
    status: StatusCode,
    session_id: Option<HeaderValue>,
    body: Vec<u8>,
}

impl<'a> McpSession<'a> {
    fn new(gateway: &'a GithubMcpGateway) -> Self {
        Self {
            gateway,
            established: None,
            reinitialized: false,
        }
    }

    async fn establish(&mut self) -> Result<EstablishedMcpSession, PortError> {
        if let Some(session) = &self.established {
            return Ok(session.clone());
        }
        let initialize = json!({
            "jsonrpc": "2.0",
            "id": MCP_INITIALIZE_REQUEST_ID,
            "method": "initialize",
            "params": {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": MCP_CLIENT_NAME,
                    "version": env!("CARGO_PKG_VERSION"),
                },
            },
        });
        // An initialize result can carry server instructions, so it shares the tool
        // result bound rather than the lifecycle bound.
        let reply = self
            .gateway
            .post_mcp(
                &initialize,
                None,
                MCP_PROTOCOL_VERSION,
                StatusCode::OK,
                MAX_MCP_TOOL_RESPONSE_BYTES,
            )
            .await?;
        if reply.status != StatusCode::OK {
            return Err(session_establishment_failure(
                reply.status,
                &reply.body,
                "initialize",
            ));
        }
        let id = reply
            .session_id
            .as_ref()
            .map(valid_mcp_session_id)
            .transpose()?;
        // Record an issued session before validating the rest of the handshake, so
        // that any later failure still closes it.
        self.established = Some(EstablishedMcpSession {
            id: id.clone(),
            protocol_version: MCP_PROTOCOL_VERSION,
        });
        let protocol_version = negotiated_protocol_version(&reply.body)?;
        let session = EstablishedMcpSession {
            id,
            protocol_version,
        };
        self.established = Some(session.clone());
        let initialized = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {},
        });
        let reply = self
            .gateway
            .post_mcp(
                &initialized,
                session.id.as_deref(),
                protocol_version,
                StatusCode::ACCEPTED,
                MAX_RESPONSE_BYTES,
            )
            .await?;
        if !reply.status.is_success() {
            return Err(session_establishment_failure(
                reply.status,
                &reply.body,
                "initialized notification",
            ));
        }
        Ok(session)
    }

    /// Sends one JSON-RPC request in the session and returns its bounded reply.
    async fn send(
        &mut self,
        message: &Value,
        limit: usize,
    ) -> Result<(StatusCode, Vec<u8>), PortError> {
        loop {
            let session = self.establish().await?;
            let reply = self
                .gateway
                .post_mcp(
                    message,
                    session.id.as_deref(),
                    session.protocol_version,
                    StatusCode::OK,
                    limit,
                )
                .await?;
            if session.id.is_some()
                && reply.status == StatusCode::NOT_FOUND
                && mcp_session_rejection(&reply.body)
            {
                // Streamable HTTP answers 404 to a session the server has ended, before
                // it dispatches the request, so resending in a new session cannot
                // repeat a mutation. Re-initialize once per operation, never in a loop.
                self.established = None;
                if self.reinitialized {
                    return Err(mcp_session_failure("session expired"));
                }
                self.reinitialized = true;
                continue;
            }
            if reply.status == StatusCode::BAD_REQUEST && mcp_session_rejection(&reply.body) {
                return Err(mcp_session_failure("session rejected"));
            }
            return Ok((reply.status, reply.body));
        }
    }

    async fn call_tool(
        &mut self,
        request_id: &str,
        tool: &str,
        arguments: Value,
    ) -> Result<Value, PortError> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "tools/call",
            "params": {"name": tool, "arguments": arguments},
        });
        let (status, body) = self.send(&request, MAX_MCP_TOOL_RESPONSE_BYTES).await?;
        require_status(status, StatusCode::OK, &body)?;
        mcp_tool_payload(&body, request_id)
    }

    /// Ends an issued session. Best effort: the operation's outcome is already
    /// decided, and an idle session also expires on the gateway.
    async fn close(self) {
        let Some(EstablishedMcpSession {
            id: Some(id),
            protocol_version,
        }) = self.established
        else {
            return;
        };
        let Ok(target) = endpoint(&self.gateway.origin, MCP_PATH) else {
            return;
        };
        let _ = self
            .gateway
            .client
            .delete(target)
            .header(
                AUTHORIZATION,
                format!("Bearer {OPEN_SHELL_BEARER_PLACEHOLDER}"),
            )
            .header(MCP_SESSION_ID_HEADER, id)
            .header(MCP_PROTOCOL_VERSION_HEADER, protocol_version)
            .timeout(MCP_SESSION_CLOSE_TIMEOUT)
            .send()
            .await;
    }
}

/// Rejects a session establishment step. Credential and authority rejections keep
/// their own categories; anything else means no usable session exists.
/// Rejects a handshake step. Only a gateway's explicit session rejection is a
/// session failure; every other status keeps the mapping a tool call would get,
/// so an outage still reports `MCP-GW returned HTTP <status>` with its detail.
fn session_establishment_failure(status: StatusCode, body: &[u8], step: &str) -> PortError {
    if status == StatusCode::BAD_REQUEST && mcp_session_rejection(body) {
        return mcp_session_failure(&format!("{step} was rejected"));
    }
    match require_status(status, StatusCode::OK, body) {
        Err(error) => error,
        Ok(()) => mcp_session_failure(&format!("{step} returned HTTP {}", status.as_u16())),
    }
}

/// The fixed, non-secret session failure. `detail` is always a bridge-authored
/// constant phrase, never gateway-supplied text.
fn mcp_session_failure(detail: &str) -> PortError {
    failed(&format!("{MCP_SESSION_FAILURE} ({detail})"))
}

/// A gateway's rejection of a missing, invalid or ended session, such as
/// agentgateway's "session header is required for non-initialize requests" (400)
/// or "session not found" (404).
fn mcp_session_rejection(body: &[u8]) -> bool {
    let body = String::from_utf8_lossy(body).to_ascii_lowercase();
    [
        "session header",
        "session id",
        "session not found",
        "mcp-session-id",
    ]
    .iter()
    .any(|phrase| body.contains(phrase))
}

fn negotiated_protocol_version(body: &[u8]) -> Result<&'static str, PortError> {
    let object = mcp_json_object(body, "MCP initialize response")
        .map_err(|_| mcp_session_failure("invalid initialize response"))?;
    let result = object
        .get("result")
        .and_then(Value::as_object)
        .filter(|_| {
            object.get("jsonrpc").and_then(Value::as_str) == Some("2.0")
                && object.get("id").and_then(Value::as_str) == Some(MCP_INITIALIZE_REQUEST_ID)
                && !object.contains_key("error")
        })
        .ok_or_else(|| mcp_session_failure("invalid initialize response"))?;
    let version = result
        .get("protocolVersion")
        .and_then(Value::as_str)
        .ok_or_else(|| mcp_session_failure("invalid initialize response"))?;
    SUPPORTED_MCP_PROTOCOL_VERSIONS
        .iter()
        .find(|supported| **supported == version)
        .copied()
        .ok_or_else(|| mcp_session_failure("unsupported protocol version"))
}

/// MCP session IDs are visible ASCII. The value is echoed only to the gateway that
/// issued it and never enters a diagnostic.
fn valid_mcp_session_id(value: &HeaderValue) -> Result<String, PortError> {
    let bytes = value.as_bytes();
    if bytes.is_empty()
        || bytes.len() > MAX_MCP_SESSION_ID_BYTES
        || !bytes.iter().all(|byte| (0x21..=0x7e).contains(byte))
    {
        return Err(mcp_session_failure("invalid session ID"));
    }
    value
        .to_str()
        .map(str::to_owned)
        .map_err(|_| mcp_session_failure("invalid session ID"))
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

fn payload_items<'a>(payload: &'a Value, fields: &[&str]) -> Option<&'a [Value]> {
    let items = fields
        .iter()
        .find_map(|field| {
            payload.get(*field).and_then(|value| {
                value
                    .as_array()
                    .or_else(|| value.get(*field).and_then(Value::as_array))
            })
        })
        .or_else(|| payload.as_array());
    if let Some(items) = items {
        return Some(items.as_slice());
    }
    // The GitHub MCP server serializes go-github results, whose list fields are
    // `omitempty`: an empty result is `{"total_count":0}`, possibly nested as
    // `{"jobs":{"total_count":0}}`.
    let empty_listing = |value: &Value| {
        value.as_object().is_some_and(|object| {
            object.get("total_count").and_then(Value::as_u64) == Some(0)
                && fields.iter().all(|field| !object.contains_key(*field))
        })
    };
    (empty_listing(payload)
        || fields
            .iter()
            .any(|field| payload.get(*field).is_some_and(empty_listing)))
    .then_some(&[])
}

fn normalize_repositories(
    me: &Value,
    payload: &Value,
    page: u32,
    per_page: u32,
) -> Result<Value, PortError> {
    let login = github_profile_login(me)?;
    let profile_id = numeric_provider_id(me.get("id"));
    let items = payload_items(payload, &["items", "repositories"])
        .filter(|items| items.len() <= usize::try_from(per_page).unwrap_or(100))
        .ok_or_else(|| rejected("GitHub repository search response is invalid"))?;
    let mut repositories = Vec::with_capacity(items.len());
    for item in items {
        // Minimal search results omit the owner object. An item owned by the
        // authenticated user takes its owner ID from the profile; any other item
        // without a stable owner ID cannot be admitted and is omitted. An item that
        // carries an owner ID, or fails for any other reason, is never dropped.
        let owner_id_present = ["owner_id", "ownerId"]
            .iter()
            .any(|field| item.get(*field).is_some())
            || item
                .get("owner")
                .is_some_and(|owner| owner.get("id").is_some());
        let owned_by_profile =
            item.get("owner").is_none() && minimal_repository_owner(item) == Some(login);
        if !owner_id_present && !owned_by_profile {
            continue;
        }
        let profile_owner_id = if owner_id_present {
            None
        } else {
            Some(
                profile_id
                    .as_deref()
                    .ok_or_else(|| rejected("GitHub profile response omitted its stable ID"))?,
            )
        };
        repositories.push(normalize_repository(item, profile_owner_id)?);
    }
    Ok(json!({
        "login": login,
        "repositories": repositories,
        "page": page,
        "hasNextPage": items.len() == usize::try_from(per_page).unwrap_or(100),
    }))
}

fn minimal_repository_owner(repository: &Value) -> Option<&str> {
    repository
        .get("full_name")
        .and_then(Value::as_str)
        .and_then(|full_name| full_name.split_once('/').map(|(owner, _)| owner))
}

fn repository_owner_id(repository: &Value) -> Option<String> {
    repository
        .get("owner")
        .and_then(|owner| numeric_provider_id(owner.get("id")))
        .or_else(|| numeric_provider_id(repository.get("owner_id")))
        .or_else(|| numeric_provider_id(repository.get("ownerId")))
}

fn github_profile_login(me: &Value) -> Result<&str, PortError> {
    me.get("login")
        .and_then(Value::as_str)
        .filter(|login| valid_repository_component(login, 39))
        .ok_or_else(|| rejected("GitHub profile response omitted its login"))
}

/// `profile_owner_id` is the authenticated user's stable ID, supplied only for a
/// minimal search item that has no owner object and whose full name names that user.
fn normalize_repository(
    repository: &Value,
    profile_owner_id: Option<&str>,
) -> Result<Value, PortError> {
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
    let owner_id = repository_owner_id(repository)
        .or_else(|| profile_owner_id.map(str::to_owned))
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
        .filter(|url| url.len() <= MAX_GITHUB_URL_BYTES && valid_github_url(url))
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
        .and_then(github_phase)
        .ok_or_else(|| rejected("GitHub run response omitted its status"))?;
    let conclusion = run.get("conclusion").cloned().unwrap_or(Value::Null);
    if !matches!(conclusion, Value::Null | Value::String(_)) {
        return Err(rejected("GitHub run response has an invalid conclusion"));
    }
    let url = run
        .get("html_url")
        .or_else(|| run.get("url"))
        .and_then(Value::as_str)
        .filter(|url| url.len() <= MAX_GITHUB_URL_BYTES && valid_github_url(url))
        .ok_or_else(|| rejected("GitHub run response omitted its URL"))?;
    let jobs = payload_items(jobs, &["jobs"])
        .filter(|jobs| jobs.len() <= MAX_RUN_STATUS_JOBS)
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

/// GitHub run and job statuses reduced to Steward's phases. `requested`, `waiting`
/// and `pending` are not yet running, so they are reported as `queued`.
fn github_phase(status: &str) -> Option<&'static str> {
    match status {
        "queued" | "requested" | "waiting" | "pending" => Some("queued"),
        "in_progress" => Some("in_progress"),
        "completed" => Some("completed"),
        _ => None,
    }
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
        .and_then(github_phase)
        .ok_or_else(|| rejected("GitHub job response omitted its status"))?;
    let conclusion = job.get("conclusion").cloned().unwrap_or(Value::Null);
    let url = job
        .get("html_url")
        .or_else(|| job.get("url"))
        .and_then(Value::as_str)
        .filter(|url| url.len() <= MAX_GITHUB_URL_BYTES && valid_github_url(url))
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
    payload_items(payload, &["workflow_runs", "runs", "items"])
        .into_iter()
        .flatten()
        .filter_map(|run| run.get("id").and_then(Value::as_u64))
        .max()
        .unwrap_or(0)
}

fn newest_dispatched_run(payload: &Value, previous_run_id: u64) -> Option<(u64, String)> {
    payload_items(payload, &["workflow_runs", "runs", "items"])?
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
    let mut actual = files
        .iter()
        .filter_map(|file| {
            file.get("filename")
                .or_else(|| file.get("path"))
                .and_then(Value::as_str)
        })
        .collect::<Vec<_>>();
    let mut expected = expected_files
        .iter()
        .map(|file| file.path.as_str())
        .collect::<Vec<_>>();
    actual.sort_unstable();
    expected.sort_unstable();
    message == Some(publication_commit_message(branch).as_str()) && actual == expected
}

fn exact_open_pull_request(
    payload: &Value,
    branch: &str,
    base_branch: &str,
) -> Result<Option<Value>, PortError> {
    let pull_requests = payload_items(payload, &["pull_requests", "items"])
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

async fn read_bounded(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Option<Vec<u8>>, PortError> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Ok(None);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| unavailable("read MCP-GW response"))?
    {
        if body.len().saturating_add(chunk.len()) > limit {
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
        MAX_MCP_TOOL_RESPONSE_BYTES, MAX_RESPONSE_BYTES, commit_sha, compatible_workflow,
        github_bridge_failure_diagnostic, maximum_run_id, mcp_tool_payload, normalize_pull_request,
        normalize_repositories, normalize_repository, normalize_run_status, parse_response,
        payload_items, pre_dispatch_provider_failure, workflow_content, workflow_file_name,
    };
    use reqwest::StatusCode;
    use serde_json::Value;
    use steward_ports::PortError;

    fn write_mcp_result(
        mut stream: TcpStream,
        request_id: &str,
        result: serde_json::Value,
    ) -> Result<(), String> {
        let response =
            serde_json::json!({"jsonrpc": "2.0", "id": request_id, "result": result}).to_string();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
            response.len()
        )
        .map_err(|error| format!("write MCP result: {error}"))
    }

    /// Streams a tool result of exactly `body_bytes` bytes with chunked transfer
    /// encoding, so the reader cannot rely on a Content-Length.
    fn write_chunked_mcp_payload(
        mut stream: TcpStream,
        request_id: &str,
        body_bytes: usize,
    ) -> Result<(), String> {
        let envelope = |padding: &str| {
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {
                    "structuredContent": {"login": "alice", "id": 1000001, "bio": padding},
                    "isError": false
                }
            })
            .to_string()
        };
        let padding = body_bytes
            .checked_sub(envelope("").len())
            .ok_or("chunked body is smaller than its envelope")?;
        let body = envelope(&"x".repeat(padding));
        assert_eq!(body.len(), body_bytes);
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
        )
        .map_err(|error| format!("write chunked MCP head: {error}"))?;
        for chunk in body.as_bytes().chunks(64 * 1024) {
            write!(stream, "{:x}\r\n", chunk.len())
                .and_then(|()| stream.write_all(chunk))
                .and_then(|()| stream.write_all(b"\r\n"))
                .map_err(|error| format!("write MCP chunk: {error}"))?;
        }
        stream
            .write_all(b"0\r\n\r\n")
            .map_err(|error| format!("finish chunked MCP body: {error}"))
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

    /// Frames one JSON-RPC result as the single event of a Streamable HTTP SSE reply.
    fn write_mcp_result_sse(
        mut stream: TcpStream,
        request_id: &str,
        result: serde_json::Value,
    ) -> Result<(), String> {
        let message =
            serde_json::json!({"jsonrpc": "2.0", "id": request_id, "result": result}).to_string();
        let body = format!("event: message\ndata: {message}\n\n");
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .map_err(|error| format!("write MCP SSE result: {error}"))
    }

    fn write_http_status(mut stream: TcpStream, status: &str, body: &str) -> Result<(), String> {
        write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .map_err(|error| format!("write HTTP {status}: {error}"))
    }

    /// The MCP transport a fixture server presents to the bridge.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum SessionMode {
        /// A direct GitHub wrapper: it answers `initialize` without issuing a session.
        Stateless,
        /// A gateway such as agentgateway: it issues an `Mcp-Session-Id` and rejects
        /// every non-initialize request that does not carry the issued value.
        Enforced,
    }

    const SESSION_MODES: [SessionMode; 2] = [SessionMode::Enforced, SessionMode::Stateless];
    const MISSING_SESSION_REJECTION: &str =
        "mcp: session header is required for non-initialize requests";

    struct HttpRequest {
        stream: TcpStream,
        request_line: String,
        headers: Vec<(String, String)>,
        body: Option<serde_json::Value>,
    }

    impl HttpRequest {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(header, _)| header.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        }
    }

    /// Accepts the next request, failing instead of hanging when the client sends none.
    fn accept_within(listener: &TcpListener, deadline: Duration) -> Result<TcpStream, String> {
        listener
            .set_nonblocking(true)
            .map_err(|error| format!("poll MCP fixture: {error}"))?;
        let started = std::time::Instant::now();
        let accepted = loop {
            match listener.accept() {
                Ok((stream, _)) => break Ok(stream),
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && started.elapsed() < deadline =>
                {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => break Err(format!("accept MCP request: {error}")),
            }
        };
        listener
            .set_nonblocking(false)
            .map_err(|error| format!("restore MCP fixture: {error}"))?;
        let stream = accepted?;
        stream
            .set_nonblocking(false)
            .map_err(|error| format!("make MCP request blocking: {error}"))?;
        Ok(stream)
    }

    fn read_http_request(listener: &TcpListener) -> Result<HttpRequest, String> {
        read_http_request_within(listener, Duration::from_secs(5))
    }

    fn read_http_request_within(
        listener: &TcpListener,
        deadline: Duration,
    ) -> Result<HttpRequest, String> {
        let mut stream = accept_within(listener, deadline)?;
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .map_err(|error| format!("bound MCP fixture read: {error}"))?;
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        let header_end = loop {
            let read = stream
                .read(&mut buffer)
                .map_err(|error| format!("read MCP request: {error}"))?;
            if read == 0 {
                return Err("MCP request ended before its headers".to_owned());
            }
            request.extend_from_slice(&buffer[..read]);
            if let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                break header_end + 4;
            }
        };
        let head = String::from_utf8_lossy(&request[..header_end]).into_owned();
        let mut lines = head.split("\r\n");
        let request_line = lines.next().unwrap_or_default().to_owned();
        let headers = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
            .collect::<Vec<_>>();
        let content_length = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .map(|(_, value)| value.parse::<usize>())
            .transpose()
            .map_err(|error| format!("parse MCP request content-length: {error}"))?
            .unwrap_or(0);
        while request.len() < header_end + content_length {
            let read = stream
                .read(&mut buffer)
                .map_err(|error| format!("read MCP request body: {error}"))?;
            if read == 0 {
                return Err("MCP request body was truncated".to_owned());
            }
            request.extend_from_slice(&buffer[..read]);
        }
        let body = (content_length > 0)
            .then(|| serde_json::from_slice(&request[header_end..header_end + content_length]))
            .transpose()
            .map_err(|error| format!("decode MCP request: {error}"))?;
        Ok(HttpRequest {
            stream,
            request_line,
            headers,
            body,
        })
    }

    /// A fake MCP Streamable HTTP server. In `Enforced` mode it behaves like the
    /// MCP-GW agentgateway: a non-initialize request without the issued session is
    /// rejected with HTTP 400, and an unknown session with HTTP 404.
    struct McpFixture {
        listener: TcpListener,
        mode: SessionMode,
        session: Option<String>,
        initialized: bool,
        sessions_issued: usize,
        /// The number of upcoming tool calls answered as an expired session.
        expirations: usize,
        accept_deadline: Duration,
    }

    impl McpFixture {
        fn bind(mode: SessionMode) -> Result<(Self, std::net::SocketAddr), String> {
            let listener = TcpListener::bind("127.0.0.1:0")
                .map_err(|error| format!("bind MCP fixture: {error}"))?;
            let address = listener
                .local_addr()
                .map_err(|error| format!("read MCP fixture address: {error}"))?;
            Ok((
                Self {
                    listener,
                    mode,
                    session: None,
                    initialized: false,
                    sessions_issued: 0,
                    expirations: 0,
                    accept_deadline: Duration::from_secs(5),
                },
                address,
            ))
        }

        fn origin(address: std::net::SocketAddr) -> String {
            format!("http://{address}")
        }

        /// Serves the session handshake and returns the next `tools/call` request.
        fn tool_call(&mut self) -> Result<(TcpStream, serde_json::Value), String> {
            loop {
                let request = read_http_request_within(&self.listener, self.accept_deadline)?;
                if request.request_line != "POST /mcp HTTP/1.1" {
                    return Err(format!(
                        "{:?} fixture expected an MCP POST, got {:?}",
                        self.mode, request.request_line
                    ));
                }
                assert_eq!(
                    request.header("authorization"),
                    Some("Bearer openshell-token-grant-placeholder"),
                    "every MCP request carries only the supervisor placeholder"
                );
                assert!(
                    request.header("accept").is_some_and(|accept| {
                        accept.contains("application/json") && accept.contains("text/event-stream")
                    }),
                    "Streamable HTTP clients accept both JSON and SSE replies"
                );
                assert_eq!(
                    request.header("mcp-protocol-version"),
                    Some("2025-06-18"),
                    "every MCP request carries the negotiated protocol version"
                );
                let body = request
                    .body
                    .clone()
                    .ok_or_else(|| "MCP POST omitted its JSON-RPC body".to_owned())?;
                let method = body["method"].as_str().unwrap_or_default().to_owned();
                if method == "initialize" {
                    assert_eq!(
                        request.header("mcp-session-id"),
                        None,
                        "initialize never carries a session"
                    );
                    assert_eq!(body["params"]["protocolVersion"], "2025-06-18");
                    assert_eq!(
                        body["params"]["clientInfo"]["name"],
                        "steward-connections-bridge"
                    );
                    let request_id = body["id"]
                        .as_str()
                        .ok_or_else(|| "initialize omitted its string ID".to_owned())?;
                    let result = serde_json::json!({
                        "protocolVersion": "2025-06-18",
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "mcp-fixture", "version": "1.0.0"}
                    });
                    self.initialized = false;
                    match self.mode {
                        SessionMode::Stateless => {
                            write_mcp_result(request.stream, request_id, result)?;
                        }
                        SessionMode::Enforced => {
                            self.sessions_issued += 1;
                            let session = format!("fixture-session-{}", self.sessions_issued);
                            self.session = Some(session.clone());
                            let message = serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": request_id,
                                "result": result
                            })
                            .to_string();
                            let reply = format!("event: message\ndata: {message}\n\n");
                            let mut stream = request.stream;
                            write!(
                                stream,
                                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nMcp-Session-Id: {session}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                                reply.len()
                            )
                            .map_err(|error| format!("write MCP initialize: {error}"))?;
                        }
                    }
                    continue;
                }
                let session = request.header("mcp-session-id").map(str::to_owned);
                match self.mode {
                    SessionMode::Enforced => match session {
                        None => {
                            write_http_status(
                                request.stream,
                                "400 Bad Request",
                                MISSING_SESSION_REJECTION,
                            )?;
                            return Err(format!("{method} was sent without an MCP session"));
                        }
                        Some(session) if Some(&session) != self.session.as_ref() => {
                            write_http_status(
                                request.stream,
                                "404 Not Found",
                                "session not found",
                            )?;
                            return Err(format!("{method} was sent with an unissued MCP session"));
                        }
                        Some(_) => {}
                    },
                    SessionMode::Stateless => assert_eq!(
                        session, None,
                        "a server that issued no session must not receive one"
                    ),
                }
                match method.as_str() {
                    "notifications/initialized" => {
                        assert!(body.get("id").is_none(), "a notification has no ID");
                        self.initialized = true;
                        write_http_status(request.stream, "202 Accepted", "")?;
                    }
                    "tools/call" => {
                        if !self.initialized {
                            return Err("tools/call preceded notifications/initialized".to_owned());
                        }
                        if self.expirations > 0 {
                            self.expirations -= 1;
                            self.session = None;
                            self.initialized = false;
                            write_http_status(
                                request.stream,
                                "404 Not Found",
                                "session not found",
                            )?;
                            continue;
                        }
                        return Ok((request.stream, body));
                    }
                    other => return Err(format!("unexpected MCP method {other:?}")),
                }
            }
        }

        /// Accepts the bridge's best-effort close of a live session.
        fn finish(mut self) -> Result<(), String> {
            let Some(session) = self.session.take() else {
                return Ok(());
            };
            let request = read_http_request(&self.listener)?;
            assert_eq!(request.request_line, "DELETE /mcp HTTP/1.1");
            assert_eq!(request.header("mcp-session-id"), Some(session.as_str()));
            assert_eq!(
                request.header("authorization"),
                Some("Bearer openshell-token-grant-placeholder")
            );
            write_http_status(request.stream, "200 OK", "")
        }
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
        for mode in SESSION_MODES {
            empty_repository_query_searches_only_the_authenticated_users_repositories_in(mode)
                .await
                .map_err(|error| format!("{mode:?}: {error}"))?;
        }
        Ok(())
    }

    async fn empty_repository_query_searches_only_the_authenticated_users_repositories_in(
        mode: SessionMode,
    ) -> Result<(), String> {
        let (mut fixture, address) = McpFixture::bind(mode)?;
        let server = thread::spawn(move || -> Result<(), String> {
            let (stream, me_request) = fixture.tool_call()?;
            assert_eq!(me_request["params"]["name"], "get_me");
            assert_eq!(me_request["params"]["arguments"], serde_json::json!({}));
            write_mcp_payload(
                stream,
                "steward-github-me",
                serde_json::json!({"login": "alice"}),
            )?;

            let (stream, repositories_request) = fixture.tool_call()?;
            assert_eq!(
                repositories_request["params"]["name"],
                "search_repositories"
            );
            assert_eq!(
                repositories_request["params"]["arguments"],
                serde_json::json!({
                    "query": "user:alice",
                    "page": 1,
                    "perPage": 100,
                    "minimal_output": true
                })
            );
            write_mcp_payload(
                stream,
                "steward-github-repositories",
                serde_json::json!({"items": []}),
            )?;
            fixture.finish()
        });
        let gateway = GithubMcpGateway::new(&McpFixture::origin(address), "0.4.9")
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

    /// Tool results captured from the pinned GitHub MCP server image
    /// (`github-mcp-server` v1.6.0) and sanitized. Hand-written shapes let the
    /// bridge drift from the real server (#311), so every parser is exercised here.
    fn captured_tool_payload(result: &str) -> Result<Value, String> {
        let result: Value =
            serde_json::from_str(result).map_err(|error| format!("parse fixture: {error}"))?;
        let envelope = serde_json::json!({"jsonrpc": "2.0", "id": "fixture", "result": result});
        let body = serde_json::to_vec(&envelope).map_err(|error| format!("encode: {error}"))?;
        mcp_tool_payload(&body, "fixture").map_err(|error| format!("tool payload: {error:?}"))
    }

    macro_rules! captured {
        ($name:literal) => {
            captured_tool_payload(include_str!(concat!(
                "../tests/fixtures/github-mcp-server-v1.6.0/",
                $name,
                ".json"
            )))
        };
    }

    #[test]
    fn captured_minimal_repository_search_takes_owner_identity_from_the_profile()
    -> Result<(), String> {
        let me = captured!("get_me")?;
        let search = captured!("search_repositories_minimal")?;
        assert!(
            search["items"][0].get("owner").is_none(),
            "the pinned server's minimal output omits the owner object"
        );
        let listing = normalize_repositories(&me, &search, 1, 100)
            .map_err(|error| format!("normalize minimal listing: {error:?}"))?;
        assert_eq!(listing["login"], "alice");
        assert_eq!(listing["repositories"][0]["owner"], "alice");
        assert_eq!(listing["repositories"][0]["ownerId"], "1000001");
        assert_eq!(listing["repositories"][0]["name"], "example-repo");
        Ok(())
    }

    #[test]
    fn captured_minimal_items_of_other_owners_are_omitted_not_fatal() -> Result<(), String> {
        let mut me = captured!("get_me")?;
        me["login"] = Value::String("bob".to_owned());
        let search = captured!("search_repositories_minimal")?;
        let listing = normalize_repositories(&me, &search, 1, 100)
            .map_err(|error| format!("normalize foreign listing: {error:?}"))?;
        assert_eq!(listing["repositories"], serde_json::json!([]));
        Ok(())
    }

    #[test]
    fn captured_full_repository_search_keeps_organization_owned_repositories() -> Result<(), String>
    {
        let me = captured!("get_me")?;
        let search = captured!("search_repositories_full")?;
        assert_eq!(search["items"][0]["owner"]["type"], "Organization");
        let listing = normalize_repositories(&me, &search, 1, 100)
            .map_err(|error| format!("normalize full listing: {error:?}"))?;
        assert_eq!(listing["login"], "alice");
        assert_eq!(listing["repositories"][0]["owner"], "example-org");
        assert_eq!(listing["repositories"][0]["ownerId"], "1000002");
        Ok(())
    }

    #[test]
    fn profile_owner_identity_is_never_lent_to_an_item_that_names_another_owner()
    -> Result<(), String> {
        let me = captured!("get_me")?;
        let search = serde_json::json!({"items": [{
            "id": 1296269,
            "name": "example-repo",
            "full_name": "alice/example-repo",
            "owner": {"login": "bob"},
            "html_url": "https://github.com/alice/example-repo",
            "private": false,
            "default_branch": "main"
        }]});
        let listing = normalize_repositories(&me, &search, 1, 100)
            .map_err(|error| format!("normalize conflicting owner: {error:?}"))?;
        assert_eq!(
            listing["repositories"],
            serde_json::json!([]),
            "only a minimal item without any owner object may take the profile ID"
        );
        Ok(())
    }

    #[tokio::test]
    async fn explicit_repository_query_requests_full_output_with_owner_identity()
    -> Result<(), String> {
        for mode in SESSION_MODES {
            explicit_repository_query_requests_full_output_with_owner_identity_in(mode)
                .await
                .map_err(|error| format!("{mode:?}: {error}"))?;
        }
        Ok(())
    }

    async fn explicit_repository_query_requests_full_output_with_owner_identity_in(
        mode: SessionMode,
    ) -> Result<(), String> {
        let me = captured!("get_me")?;
        let search = captured!("search_repositories_full")?;
        let (mut fixture, address) = McpFixture::bind(mode)?;
        let server = thread::spawn(move || -> Result<(), String> {
            let (stream, me_request) = fixture.tool_call()?;
            assert_eq!(me_request["params"]["name"], "get_me");
            write_mcp_payload(stream, "steward-github-me", me)?;
            let (stream, repositories_request) = fixture.tool_call()?;
            assert_eq!(
                repositories_request["params"]["arguments"],
                serde_json::json!({
                    "query": "repo:example-org/example-repo",
                    "page": 1,
                    "perPage": 10,
                    "minimal_output": false
                }),
                "only full output carries the owner identity of repositories the user does not own"
            );
            write_mcp_payload(stream, "steward-github-repositories", search)?;
            fixture.finish()
        });
        let gateway = GithubMcpGateway::new(&McpFixture::origin(address), "0.4.9")
            .map_err(|error| format!("build repository gateway: {error:?}"))?;
        let response = gateway
            .execute(
                GithubBridgeOperation::Repositories,
                GithubBridgeRequest::Repositories {
                    query: "repo:example-org/example-repo".to_owned(),
                    page: 1,
                    per_page: 10,
                },
            )
            .await
            .map_err(|error| format!("execute repository query: {error:?}"))?;
        server
            .join()
            .map_err(|_| "repository fixture panicked".to_owned())??;
        assert_eq!(response["repositories"][0]["owner"], "example-org");
        assert_eq!(response["repositories"][0]["ownerId"], "1000002");
        Ok(())
    }

    #[test]
    fn captured_workflow_run_and_nested_jobs_normalize() -> Result<(), String> {
        let run = captured!("actions_get_workflow_run")?;
        let jobs = captured!("actions_list_workflow_jobs")?;
        assert!(
            jobs["jobs"].is_object(),
            "the pinned server nests jobs as {{\"jobs\":{{\"total_count\":N,\"jobs\":[...]}}}}"
        );
        let run_id = run["id"].as_u64().ok_or("captured run has an ID")?;
        let status = normalize_run_status(&run, &jobs, run_id, None)
            .map_err(|error| format!("normalize run status: {error:?}"))?;
        assert_eq!(
            status["jobs"].as_array().map(Vec::len),
            Some(2),
            "both captured jobs are normalized"
        );
        Ok(())
    }

    /// The tool result as it travels: the page is a JSON string inside the MCP
    /// `content[].text`, so every quote in it is escaped.
    fn runs_page_wire_bytes(run: &Value, runs: usize) -> Result<usize, String> {
        let page = serde_json::json!({
            "total_count": runs,
            "workflow_runs": vec![run.clone(); runs],
        })
        .to_string();
        let envelope = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "steward-github-runs-after",
            "result": {"content": [{"type": "text", "text": page}]},
        });
        serde_json::to_vec(&envelope)
            .map(|body| body.len())
            .map_err(|error| format!("encode runs page: {error}"))
    }

    #[test]
    fn captured_workflow_runs_page_is_read_and_fits_the_tool_bound() -> Result<(), String> {
        let runs = captured!("actions_list_workflow_runs")?;
        assert_eq!(maximum_run_id(&runs), 3000002);
        let run = &runs["workflow_runs"][0];
        assert!(
            runs_page_wire_bytes(run, 10)? < MAX_MCP_TOOL_RESPONSE_BYTES,
            "the 10-run page the bridge requests must fit the tool response bound"
        );
        assert!(
            runs_page_wire_bytes(run, 30)? < MAX_MCP_TOOL_RESPONSE_BYTES,
            "even the server's default 30-run page fits the tool response bound"
        );
        assert!(
            runs_page_wire_bytes(run, 3)? > MAX_RESPONSE_BYTES,
            "the lifecycle bound is too small for tool results, which is why they have their own"
        );
        Ok(())
    }

    #[test]
    fn captured_file_commit_and_pull_request_results_parse() -> Result<(), String> {
        let file = captured!("get_file_contents")?;
        assert!(
            workflow_content(&file).is_some(),
            "resource text is returned as content"
        );
        assert!(file["sha"].as_str().is_some_and(|sha| sha.len() == 40));
        let commit = captured!("get_commit")?;
        assert!(commit_sha(&commit).is_some());
        assert!(
            commit["commit"]["message"].is_string() && commit["files"][0]["filename"].is_string(),
            "publication ownership reads the commit message and changed file names"
        );
        let pulls = captured!("list_pull_requests")?;
        let first = payload_items(&pulls, &["pull_requests", "items"])
            .and_then(|items| items.first())
            .ok_or("captured pull request list has an item")?;
        let branch = first["head"]["ref"].as_str().ok_or("captured head ref")?;
        normalize_pull_request(first, branch)
            .map_err(|error| format!("normalize pull request: {error:?}"))?;
        Ok(())
    }

    #[test]
    fn pinned_repository_search_accepts_flat_owner_identity() -> Result<(), String> {
        let repository = normalize_repository(
            &serde_json::json!({
                "id": 123,
                "name": "example-repo",
                "full_name": "example-org/example-repo",
                "owner_id": 456,
                "default_branch": "main",
                "private": true,
                "html_url": "https://github.com/example-org/example-repo"
            }),
            None,
        )
        .map_err(|error| format!("normalize pinned repository result: {error:?}"))?;
        assert_eq!(repository["owner"], "example-org");
        assert_eq!(repository["ownerId"], "456");
        assert_eq!(repository["repositoryId"], "123");
        Ok(())
    }

    #[test]
    fn empty_go_github_listings_are_empty_lists() -> Result<(), String> {
        let me = captured!("get_me")?;
        let listing = normalize_repositories(
            &me,
            &serde_json::json!({"total_count": 0, "incomplete_results": false}),
            1,
            10,
        )
        .map_err(|error| format!("normalize empty search: {error:?}"))?;
        assert_eq!(listing["repositories"], serde_json::json!([]));
        assert_eq!(listing["hasNextPage"], false);
        assert!(
            normalize_repositories(&me, &serde_json::json!({"total_count": 3}), 1, 10).is_err(),
            "a non-empty result without items is still invalid"
        );

        let run = captured!("actions_get_workflow_run")?;
        let status = normalize_run_status(
            &run,
            &serde_json::json!({"jobs": {"total_count": 0}}),
            3000002,
            None,
        )
        .map_err(|error| format!("normalize run without jobs: {error:?}"))?;
        assert_eq!(status["jobs"], serde_json::json!([]));
        assert_eq!(
            maximum_run_id(&serde_json::json!({"total_count": 0})),
            0,
            "a workflow without runs has no previous run"
        );
        Ok(())
    }

    #[test]
    fn waiting_github_runs_and_jobs_are_queued() -> Result<(), String> {
        for status in ["requested", "waiting", "pending", "queued"] {
            let result = normalize_run_status(
                &serde_json::json!({
                    "run_attempt": 1,
                    "status": status,
                    "conclusion": null,
                    "html_url": "https://github.com/example-org/example-repo/actions/runs/1"
                }),
                &serde_json::json!({"jobs": [{
                    "id": 2,
                    "name": "build",
                    "status": status,
                    "conclusion": null,
                    "html_url": "https://github.com/example-org/example-repo/actions/runs/1/job/2"
                }]}),
                1,
                None,
            )
            .map_err(|error| format!("normalize {status} run: {error:?}"))?;
            assert_eq!(result["phase"], "queued", "{status}");
            assert_eq!(result["jobs"][0]["status"], "queued", "{status}");
        }
        assert!(
            normalize_run_status(
                &serde_json::json!({
                    "run_attempt": 1,
                    "status": "unknown",
                    "html_url": "https://github.com/example-org/example-repo/actions/runs/1"
                }),
                &serde_json::json!({"jobs": []}),
                1,
                None,
            )
            .is_err(),
            "an unknown status is not guessed"
        );
        Ok(())
    }

    #[test]
    fn dispatch_tools_receive_the_workflow_file_name() {
        assert_eq!(
            workflow_file_name(".github/workflows/steward-task.yml").ok(),
            Some("steward-task.yml")
        );
        for invalid in [
            "steward-task.yml",
            ".github/workflows/nested/steward-task.yml",
            ".github/steward-task.yml",
            ".github/workflows/",
        ] {
            assert!(workflow_file_name(invalid).is_err(), "{invalid}");
        }
    }

    #[tokio::test]
    async fn run_status_requests_jobs_with_the_parameter_the_server_reads() -> Result<(), String> {
        for mode in SESSION_MODES {
            run_status_requests_jobs_with_the_parameter_the_server_reads_in(mode)
                .await
                .map_err(|error| format!("{mode:?}: {error}"))?;
        }
        Ok(())
    }

    async fn run_status_requests_jobs_with_the_parameter_the_server_reads_in(
        mode: SessionMode,
    ) -> Result<(), String> {
        let run = captured!("actions_get_workflow_run")?;
        let jobs = captured!("actions_list_workflow_jobs")?;
        let (mut fixture, address) = McpFixture::bind(mode)?;
        let server = thread::spawn(move || -> Result<(), String> {
            let (stream, run_request) = fixture.tool_call()?;
            assert_eq!(run_request["params"]["name"], "actions_get");
            write_mcp_payload(stream, "steward-github-run", run)?;
            let (stream, jobs_request) = fixture.tool_call()?;
            assert_eq!(jobs_request["params"]["name"], "actions_list");
            assert_eq!(
                jobs_request["params"]["arguments"],
                serde_json::json!({
                    "method": "list_workflow_jobs",
                    "owner": "alice",
                    "repo": "example-repo",
                    "resource_id": "3000002",
                    "perPage": 30
                }),
                "the pinned server reads perPage, and 30 jobs keep the run status within its bound"
            );
            write_mcp_payload(stream, "steward-github-jobs", jobs)?;
            fixture.finish()
        });
        let gateway = GithubMcpGateway::new(&McpFixture::origin(address), "0.4.9")
            .map_err(|error| format!("build run status gateway: {error:?}"))?;
        let status = gateway
            .execute(
                GithubBridgeOperation::RunStatus,
                GithubBridgeRequest::RunStatus {
                    owner: "alice".to_owned(),
                    repo: "example-repo".to_owned(),
                    run_id: 3000002,
                },
            )
            .await
            .map_err(|error| format!("execute run status: {error:?}"))?;
        server
            .join()
            .map_err(|_| "run status fixture panicked".to_owned())??;
        assert_eq!(status["phase"], "completed");
        assert_eq!(status["jobs"].as_array().map(Vec::len), Some(2));
        Ok(())
    }

    #[tokio::test]
    async fn dispatch_that_the_server_rejects_is_a_failure() -> Result<(), String> {
        for mode in SESSION_MODES {
            dispatch_that_the_server_rejects_is_a_failure_in(mode)
                .await
                .map_err(|error| format!("{mode:?}: {error}"))?;
        }
        Ok(())
    }

    async fn dispatch_that_the_server_rejects_is_a_failure_in(
        mode: SessionMode,
    ) -> Result<(), String> {
        let workflow = concat!(
            "on:\n",
            "  workflow_dispatch:\n",
            "jobs:\n",
            "  governed:\n",
            "    uses: example-org/steward-run/.github/workflows/steward-task.yml@0123456789012345678901234567890123456789\n"
        );
        let (mut fixture, address) = McpFixture::bind(mode)?;
        let expected_workflow = workflow.to_owned();
        let server = thread::spawn(move || -> Result<(), String> {
            let (stream, _) = fixture.tool_call()?;
            write_mcp_payload(
                stream,
                "steward-github-dispatch-workflow",
                serde_json::json!({"content": expected_workflow}),
            )?;
            let (stream, _) = fixture.tool_call()?;
            write_mcp_payload(
                stream,
                "steward-github-runs-before",
                serde_json::json!({"total_count": 0}),
            )?;
            let (stream, dispatch_request) = fixture.tool_call()?;
            assert_eq!(dispatch_request["params"]["name"], "actions_run_trigger");
            write_mcp_result(
                stream,
                "steward-github-dispatch",
                serde_json::json!({
                    "isError": true,
                    "content": [{"type": "text", "text": "failed to run workflow: 404 Not Found"}]
                }),
            )?;
            fixture.finish()
        });
        let gateway = GithubMcpGateway::new(&McpFixture::origin(address), "0.4.9")
            .map_err(|error| format!("build rejected dispatch gateway: {error:?}"))?;
        let result = gateway
            .execute(
                GithubBridgeOperation::Dispatch,
                GithubBridgeRequest::Dispatch {
                    owner: "example-org".to_owned(),
                    repo: "example-repo".to_owned(),
                    workflow_id: ".github/workflows/steward-browser-task.yml".to_owned(),
                    git_ref: "main".to_owned(),
                    inputs: serde_json::Map::new(),
                    expected_content: workflow.to_owned(),
                },
            )
            .await;
        server
            .join()
            .map_err(|_| "rejected dispatch fixture panicked".to_owned())??;
        assert_eq!(
            result,
            Err(PortError::Rejected {
                reason: "GitHub rejected the workflow dispatch".to_owned(),
            }),
            "a not-found dispatch is a definite rejection, neither queued nor an outage"
        );
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
        for mode in SESSION_MODES {
            dispatch_rechecks_the_exact_workflow_and_uses_official_actions_tool_arguments_in(mode)
                .await
                .map_err(|error| format!("{mode:?}: {error}"))?;
        }
        Ok(())
    }

    async fn dispatch_rechecks_the_exact_workflow_and_uses_official_actions_tool_arguments_in(
        mode: SessionMode,
    ) -> Result<(), String> {
        let workflow = concat!(
            "on:\n",
            "  workflow_dispatch:\n",
            "    inputs:\n",
            "      message:\n",
            "jobs:\n",
            "  governed:\n",
            "    uses: example-org/steward-run/.github/workflows/steward-task.yml@0123456789012345678901234567890123456789\n"
        );
        let (mut fixture, address) = McpFixture::bind(mode)?;
        let expected_workflow = workflow.to_owned();
        let server = thread::spawn(move || -> Result<(), String> {
            let (stream, workflow_request) = fixture.tool_call()?;
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

            let (stream, before_request) = fixture.tool_call()?;
            assert_eq!(before_request["params"]["name"], "actions_list");
            assert_eq!(
                before_request["params"]["arguments"],
                serde_json::json!({
                    "method": "list_workflow_runs",
                    "owner": "example-org",
                    "repo": "example-repo",
                    "resource_id": "steward-browser-task.yml",
                    "workflow_runs_filter": {"branch": "main"},
                    "perPage": 10
                }),
                "the pinned server reads perPage and resolves the workflow by file name"
            );
            write_mcp_payload(
                stream,
                "steward-github-runs-before",
                serde_json::json!({"workflow_runs": [{"id": 100}]}),
            )?;

            let (stream, dispatch_request) = fixture.tool_call()?;
            assert_eq!(dispatch_request["params"]["name"], "actions_run_trigger");
            assert_eq!(
                dispatch_request["params"]["arguments"],
                serde_json::json!({
                    "method": "run_workflow",
                    "owner": "example-org",
                    "repo": "example-repo",
                    "workflow_id": "steward-browser-task.yml",
                    "ref": "main",
                    "inputs": {"message": "hello"}
                })
            );
            write_mcp_payload(stream, "steward-github-dispatch", serde_json::json!({}))?;

            let (stream, after_request) = fixture.tool_call()?;
            assert_eq!(after_request["params"]["name"], "actions_list");
            assert_eq!(
                after_request["params"]["arguments"],
                before_request["params"]["arguments"]
            );
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
            )?;
            fixture.finish()
        });
        let gateway = GithubMcpGateway::new(&McpFixture::origin(address), "0.4.9")
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
        for mode in SESSION_MODES {
            publication_updates_only_its_proven_existing_open_pull_request_in(mode)
                .await
                .map_err(|error| format!("{mode:?}: {error}"))?;
        }
        Ok(())
    }

    async fn publication_updates_only_its_proven_existing_open_pull_request_in(
        mode: SessionMode,
    ) -> Result<(), String> {
        let branch =
            "steward/task-11111111111141118111111111111111-22222222222242228222222222222222";
        let workflow_path = ".github/workflows/steward-browser-task.yml";
        let package_path = ".steward/tasks/hello/task-definition.json";
        let (mut fixture, address) = McpFixture::bind(mode)?;
        let branch_for_server = branch.to_owned();
        let server = thread::spawn(move || -> Result<(), String> {
            let (stream, request) = fixture.tool_call()?;
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

            let (stream, request) = fixture.tool_call()?;
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

            let (stream, request) = fixture.tool_call()?;
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
                let (stream, request) = fixture.tool_call()?;
                assert_eq!(request["params"]["name"], "get_file_contents");
                assert_eq!(request["params"]["arguments"]["path"], path);
                write_mcp_payload(
                    stream,
                    "steward-github-publication-file",
                    serde_json::json!({"content": content}),
                )?;
            }

            let (stream, request) = fixture.tool_call()?;
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
            )?;
            fixture.finish()
        });
        let gateway = GithubMcpGateway::new(&McpFixture::origin(address), "0.4.9")
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
        for mode in SESSION_MODES {
            github_rerun_calls_only_the_exact_actions_tool_in(mode)
                .await
                .map_err(|error| format!("{mode:?}: {error}"))?;
        }
        Ok(())
    }

    async fn github_rerun_calls_only_the_exact_actions_tool_in(
        mode: SessionMode,
    ) -> Result<(), String> {
        let (mut fixture, address) = McpFixture::bind(mode)?;
        let server = thread::spawn(move || -> Result<(), String> {
            let (mut stream, body) = fixture.tool_call()?;
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
            drop(stream);
            fixture.finish()
        });
        let gateway = GithubMcpGateway::new(&McpFixture::origin(address), "0.4.9")
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

    fn run_status_request() -> GithubBridgeRequest {
        GithubBridgeRequest::RunStatus {
            owner: "alice".to_owned(),
            repo: "example-repo".to_owned(),
            run_id: 3000002,
        }
    }

    /// The captured v1.6.0 replies, unmodified, through a gateway that enforces
    /// sessions and answers every MCP request as Streamable HTTP SSE.
    #[tokio::test]
    async fn captured_replies_flow_through_a_session_enforcing_gateway() -> Result<(), String> {
        let raw = |fixture: &str| -> Result<Value, String> {
            serde_json::from_str(fixture).map_err(|error| format!("parse fixture: {error}"))
        };
        let me = raw(include_str!(
            "../tests/fixtures/github-mcp-server-v1.6.0/get_me.json"
        ))?;
        let search = raw(include_str!(
            "../tests/fixtures/github-mcp-server-v1.6.0/search_repositories_minimal.json"
        ))?;
        let run = raw(include_str!(
            "../tests/fixtures/github-mcp-server-v1.6.0/actions_get_workflow_run.json"
        ))?;
        let jobs = raw(include_str!(
            "../tests/fixtures/github-mcp-server-v1.6.0/actions_list_workflow_jobs.json"
        ))?;

        let (mut fixture, address) = McpFixture::bind(SessionMode::Enforced)?;
        let server = thread::spawn(move || -> Result<usize, String> {
            let (stream, request) = fixture.tool_call()?;
            assert_eq!(request["params"]["name"], "get_me");
            write_mcp_result_sse(stream, "steward-github-me", me)?;
            let (stream, request) = fixture.tool_call()?;
            assert_eq!(request["params"]["name"], "search_repositories");
            write_mcp_result_sse(stream, "steward-github-repositories", search)?;
            let sessions = fixture.sessions_issued;
            fixture.finish()?;
            Ok(sessions)
        });
        let gateway = GithubMcpGateway::new(&McpFixture::origin(address), "0.4.9")
            .map_err(|error| format!("build repository gateway: {error:?}"))?;
        let listing = gateway
            .execute(
                GithubBridgeOperation::Repositories,
                GithubBridgeRequest::Repositories {
                    query: String::new(),
                    page: 1,
                    per_page: 100,
                },
            )
            .await
            .map_err(|error| format!("execute repository listing: {error:?}"))?;
        let sessions = server
            .join()
            .map_err(|_| "repository fixture panicked".to_owned())??;
        assert_eq!(sessions, 1, "one operation reuses one session");
        assert_eq!(listing["login"], "alice");
        assert_eq!(listing["repositories"][0]["ownerId"], "1000001");

        let (mut fixture, address) = McpFixture::bind(SessionMode::Enforced)?;
        let server = thread::spawn(move || -> Result<(), String> {
            let (stream, request) = fixture.tool_call()?;
            assert_eq!(request["params"]["name"], "actions_get");
            write_mcp_result_sse(stream, "steward-github-run", run)?;
            let (stream, request) = fixture.tool_call()?;
            assert_eq!(request["params"]["name"], "actions_list");
            write_mcp_result_sse(stream, "steward-github-jobs", jobs)?;
            fixture.finish()
        });
        let gateway = GithubMcpGateway::new(&McpFixture::origin(address), "0.4.9")
            .map_err(|error| format!("build run status gateway: {error:?}"))?;
        let status = gateway
            .execute(GithubBridgeOperation::RunStatus, run_status_request())
            .await
            .map_err(|error| format!("execute run status: {error:?}"))?;
        server
            .join()
            .map_err(|_| "run status fixture panicked".to_owned())??;
        assert_eq!(status["phase"], "completed");
        assert_eq!(status["jobs"].as_array().map(Vec::len), Some(2));
        Ok(())
    }

    #[tokio::test]
    async fn an_expired_session_is_reinitialized_once_and_the_call_resent() -> Result<(), String> {
        let (fixture, address) = McpFixture::bind(SessionMode::Enforced)?;
        let mut fixture = McpFixture {
            expirations: 1,
            ..fixture
        };
        let server = thread::spawn(move || -> Result<usize, String> {
            let (stream, request) = fixture.tool_call()?;
            assert_eq!(
                request["id"], "steward-github-run",
                "the call the expired session refused is resent unchanged"
            );
            write_mcp_payload(
                stream,
                "steward-github-run",
                serde_json::json!({
                    "id": 3000002,
                    "run_attempt": 1,
                    "status": "queued",
                    "html_url": "https://github.com/alice/example-repo/actions/runs/3000002"
                }),
            )?;
            let (stream, request) = fixture.tool_call()?;
            assert_eq!(request["id"], "steward-github-jobs");
            write_mcp_payload(
                stream,
                "steward-github-jobs",
                serde_json::json!({"jobs": []}),
            )?;
            let sessions = fixture.sessions_issued;
            fixture.finish()?;
            Ok(sessions)
        });
        let gateway = GithubMcpGateway::new(&McpFixture::origin(address), "0.4.9")
            .map_err(|error| format!("build expiring gateway: {error:?}"))?;
        let status = gateway
            .execute(GithubBridgeOperation::RunStatus, run_status_request())
            .await
            .map_err(|error| format!("execute run status: {error:?}"))?;
        let sessions = server
            .join()
            .map_err(|_| "expiring fixture panicked".to_owned())??;
        assert_eq!(sessions, 2, "an expired session is replaced exactly once");
        assert_eq!(status["phase"], "queued");
        Ok(())
    }

    #[tokio::test]
    async fn a_session_that_expires_again_is_a_session_failure() -> Result<(), String> {
        let (fixture, address) = McpFixture::bind(SessionMode::Enforced)?;
        let mut fixture = McpFixture {
            expirations: 2,
            accept_deadline: Duration::from_millis(500),
            ..fixture
        };
        let server = thread::spawn(move || -> Result<usize, String> {
            match fixture.tool_call() {
                Ok(_) => Err("the bridge re-initialized more than once".to_owned()),
                // The bridge gives up without another request, not even a close of
                // the session that already ended, so the next accept times out.
                Err(error) if error.starts_with("accept MCP request") => {
                    Ok(fixture.sessions_issued)
                }
                Err(error) => Err(error),
            }
        });
        let gateway = GithubMcpGateway::new(&McpFixture::origin(address), "0.4.9")
            .map_err(|error| format!("build expiring gateway: {error:?}"))?;
        let result = gateway
            .execute(GithubBridgeOperation::RunStatus, run_status_request())
            .await;
        let sessions = server
            .join()
            .map_err(|_| "expiring fixture panicked".to_owned())??;
        assert_eq!(
            sessions, 2,
            "the bridge re-initializes once, never in a loop"
        );
        assert_eq!(
            result,
            Err(PortError::Failed {
                reason: "MCP-GW session could not be established (session expired)".to_owned(),
            })
        );
        Ok(())
    }

    /// A gateway that requires a session but issued none must not surface as a
    /// generic HTTP 400.
    #[tokio::test]
    async fn a_required_but_unissued_session_is_a_session_failure() -> Result<(), String> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|error| format!("bind session fixture: {error}"))?;
        let address = listener
            .local_addr()
            .map_err(|error| format!("read session fixture address: {error}"))?;
        let server = thread::spawn(move || -> Result<(), String> {
            let initialize = read_http_request(&listener)?;
            write_mcp_result(
                initialize.stream,
                "steward-mcp-initialize",
                serde_json::json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "mcp-fixture", "version": "1.0.0"}
                }),
            )?;
            let notification = read_http_request(&listener)?;
            write_http_status(notification.stream, "202 Accepted", "")?;
            let call = read_http_request(&listener)?;
            write_http_status(call.stream, "400 Bad Request", MISSING_SESSION_REJECTION)
        });
        let gateway = GithubMcpGateway::new(&McpFixture::origin(address), "0.4.9")
            .map_err(|error| format!("build session gateway: {error:?}"))?;
        let result = gateway
            .execute(GithubBridgeOperation::RunStatus, run_status_request())
            .await;
        server
            .join()
            .map_err(|_| "session fixture panicked".to_owned())??;
        assert_eq!(
            result,
            Err(PortError::Failed {
                reason: "MCP-GW session could not be established (session rejected)".to_owned(),
            })
        );
        Ok(())
    }

    const INITIALIZE_RESULT: &str = r#"{"jsonrpc":"2.0","id":"steward-mcp-initialize","result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"mcp-fixture","version":"1.0.0"}}}"#;

    /// Serves scripted replies to consecutive requests, then reports every request
    /// line it received, including a close that arrives within a short window.
    async fn scripted_run_status(
        replies: Vec<(&'static str, &'static str, &'static str)>,
    ) -> Result<(Result<Value, PortError>, Vec<String>), String> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|error| format!("bind scripted fixture: {error}"))?;
        let address = listener
            .local_addr()
            .map_err(|error| format!("read scripted fixture address: {error}"))?;
        let server = thread::spawn(move || -> Result<Vec<String>, String> {
            let mut seen = Vec::new();
            for (status, headers, body) in replies {
                let request = read_http_request(&listener)?;
                seen.push(format!(
                    "{} {}",
                    request.request_line,
                    request
                        .body
                        .as_ref()
                        .map_or("", |body| body["method"].as_str().unwrap_or_default())
                ));
                let mut stream = request.stream;
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .map_err(|error| format!("write scripted reply: {error}"))?;
            }
            if let Ok(request) = read_http_request_within(&listener, Duration::from_millis(500)) {
                seen.push(format!(
                    "{} {}",
                    request.request_line,
                    request.header("mcp-session-id").unwrap_or_default()
                ));
                write_http_status(request.stream, "200 OK", "")?;
            }
            Ok(seen)
        });
        let gateway = GithubMcpGateway::new(&McpFixture::origin(address), "0.4.9")
            .map_err(|error| format!("build scripted gateway: {error:?}"))?;
        let result = gateway
            .execute(GithubBridgeOperation::RunStatus, run_status_request())
            .await;
        let seen = server
            .join()
            .map_err(|_| "scripted fixture panicked".to_owned())??;
        Ok((result, seen))
    }

    #[tokio::test]
    async fn handshake_failures_keep_existing_categories_unless_the_session_is_at_fault()
    -> Result<(), String> {
        let session_failure = |detail: &str| PortError::Failed {
            reason: format!("MCP-GW session could not be established ({detail})"),
        };
        for (initialize, expected) in [
            (
                ("503 Service Unavailable", "", ""),
                PortError::Failed {
                    reason: "MCP-GW returned HTTP 503".to_owned(),
                },
            ),
            (
                ("403 Forbidden", "", r#"{"error":"forbidden"}"#),
                PortError::Failed {
                    reason: "MCP-GW rejected runtime authorization".to_owned(),
                },
            ),
            (
                ("400 Bad Request", "", "session ID is required"),
                session_failure("initialize was rejected"),
            ),
            (
                (
                    "200 OK",
                    "",
                    r#"{"jsonrpc":"2.0","id":"steward-mcp-initialize","error":{"code":-32602,"message":"Unsupported protocol version"}}"#,
                ),
                session_failure("invalid initialize response"),
            ),
            (
                (
                    "200 OK",
                    "",
                    r#"{"jsonrpc":"2.0","id":"steward-mcp-initialize","result":{"protocolVersion":"2099-01-01","capabilities":{}}}"#,
                ),
                session_failure("unsupported protocol version"),
            ),
        ] {
            let (result, seen) = scripted_run_status(vec![initialize]).await?;
            assert_eq!(result, Err(expected), "initialize reply {initialize:?}");
            assert_eq!(seen, vec!["POST /mcp HTTP/1.1 initialize".to_owned()]);
        }

        let (result, seen) = scripted_run_status(vec![
            (
                "200 OK",
                "Mcp-Session-Id: fixture-session-1\r\n",
                r#"{"jsonrpc":"2.0","id":"steward-mcp-initialize","result":{"protocolVersion":"2099-01-01"}}"#,
            ),
        ])
        .await?;
        assert_eq!(result, Err(session_failure("unsupported protocol version")));
        assert_eq!(
            seen.last().map(String::as_str),
            Some("DELETE /mcp HTTP/1.1 fixture-session-1"),
            "a session issued by a rejected handshake is still closed"
        );

        let (result, seen) = scripted_run_status(vec![
            (
                "200 OK",
                "Mcp-Session-Id: fixture-session-1\r\n",
                INITIALIZE_RESULT,
            ),
            ("400 Bad Request", "", "invalid session ID header"),
        ])
        .await?;
        assert_eq!(
            result,
            Err(session_failure("initialized notification was rejected"))
        );
        assert_eq!(
            seen.last().map(String::as_str),
            Some("DELETE /mcp HTTP/1.1 fixture-session-1")
        );

        let (result, seen) = scripted_run_status(vec![
            (
                "200 OK",
                "Mcp-Session-Id: fixture-session-1\r\n",
                INITIALIZE_RESULT,
            ),
            ("202 Accepted", "", ""),
            ("400 Bad Request", "", "invalid session ID header"),
        ])
        .await?;
        assert_eq!(result, Err(session_failure("session rejected")));
        assert_eq!(
            seen.last().map(String::as_str),
            Some("DELETE /mcp HTTP/1.1 fixture-session-1")
        );

        // A 404 that does not name the session is the tool route's answer, not an
        // expiry: it is never resent.
        let (result, seen) = scripted_run_status(vec![
            (
                "200 OK",
                "Mcp-Session-Id: fixture-session-1\r\n",
                INITIALIZE_RESULT,
            ),
            ("202 Accepted", "", ""),
            ("404 Not Found", "", r#"{"error":"not found"}"#),
        ])
        .await?;
        assert_eq!(
            result,
            Err(PortError::Failed {
                reason: "MCP-GW returned HTTP 404 (not found)".to_owned(),
            })
        );
        assert_eq!(
            seen,
            vec![
                "POST /mcp HTTP/1.1 initialize".to_owned(),
                "POST /mcp HTTP/1.1 notifications/initialized".to_owned(),
                "POST /mcp HTTP/1.1 tools/call".to_owned(),
                "DELETE /mcp HTTP/1.1 fixture-session-1".to_owned(),
            ]
        );
        Ok(())
    }

    /// A direct wrapper issues no session, so the bridge sends none and has
    /// nothing to close.
    #[tokio::test]
    async fn a_stateless_server_receives_no_session_and_no_close() -> Result<(), String> {
        let (mut fixture, address) = McpFixture::bind(SessionMode::Stateless)?;
        let server = thread::spawn(move || -> Result<McpFixture, String> {
            let (stream, _) = fixture.tool_call()?;
            write_mcp_payload(
                stream,
                "steward-github-run",
                serde_json::json!({
                    "id": 3000002,
                    "run_attempt": 1,
                    "status": "queued",
                    "html_url": "https://github.com/alice/example-repo/actions/runs/3000002"
                }),
            )?;
            let (stream, _) = fixture.tool_call()?;
            write_mcp_payload(
                stream,
                "steward-github-jobs",
                serde_json::json!({"jobs": []}),
            )?;
            Ok(fixture)
        });
        let gateway = GithubMcpGateway::new(&McpFixture::origin(address), "0.4.9")
            .map_err(|error| format!("build stateless gateway: {error:?}"))?;
        gateway
            .execute(GithubBridgeOperation::RunStatus, run_status_request())
            .await
            .map_err(|error| format!("execute run status: {error:?}"))?;
        let fixture = server
            .join()
            .map_err(|_| "stateless fixture panicked".to_owned())??;
        fixture
            .listener
            .set_nonblocking(true)
            .map_err(|error| format!("poll stateless fixture: {error}"))?;
        assert!(
            matches!(
                fixture.listener.accept(),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
            ),
            "a stateless server must not receive a session close"
        );
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

    #[tokio::test]
    async fn tool_results_above_the_lifecycle_bound_are_read() -> Result<(), String> {
        for mode in SESSION_MODES {
            tool_results_above_the_lifecycle_bound_are_read_in(mode)
                .await
                .map_err(|error| format!("{mode:?}: {error}"))?;
        }
        Ok(())
    }

    async fn tool_results_above_the_lifecycle_bound_are_read_in(
        mode: SessionMode,
    ) -> Result<(), String> {
        let (mut fixture, address) = McpFixture::bind(mode)?;
        let server = thread::spawn(move || -> Result<(), String> {
            let (stream, _) = fixture.tool_call()?;
            write_mcp_payload(
                stream,
                "steward-github-me",
                serde_json::json!({
                    "login": "alice",
                    "id": 1000001,
                    "bio": "x".repeat(2 * MAX_RESPONSE_BYTES),
                }),
            )?;
            let (stream, _) = fixture.tool_call()?;
            write_mcp_payload(
                stream,
                "steward-github-repositories",
                serde_json::json!({"items": []}),
            )?;
            fixture.finish()
        });
        let gateway = GithubMcpGateway::new(&McpFixture::origin(address), "0.4.9")
            .map_err(|error| format!("build tool bound gateway: {error:?}"))?;
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
            .map_err(|error| format!("a tool result above 32 KiB must be read: {error:?}"))?;
        server
            .join()
            .map_err(|_| "tool bound fixture panicked".to_owned())??;
        assert_eq!(response["login"], "alice");
        Ok(())
    }

    #[tokio::test]
    async fn tool_results_above_the_tool_bound_are_not_read() -> Result<(), String> {
        for mode in SESSION_MODES {
            tool_results_above_the_tool_bound_are_not_read_in(mode)
                .await
                .map_err(|error| format!("{mode:?}: {error}"))?;
        }
        Ok(())
    }

    async fn tool_results_above_the_tool_bound_are_not_read_in(
        mode: SessionMode,
    ) -> Result<(), String> {
        let (mut fixture, address) = McpFixture::bind(mode)?;
        let server = thread::spawn(move || -> Result<(), String> {
            let (mut stream, _) = fixture.tool_call()?;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                MAX_MCP_TOOL_RESPONSE_BYTES + 1
            )
            .map_err(|error| format!("write oversized tool response: {error}"))?;
            fixture.finish()
        });
        let gateway = GithubMcpGateway::new(&McpFixture::origin(address), "0.4.9")
            .map_err(|error| format!("build tool bound gateway: {error:?}"))?;
        let result = gateway
            .execute(
                GithubBridgeOperation::Repositories,
                GithubBridgeRequest::Repositories {
                    query: String::new(),
                    page: 1,
                    per_page: 100,
                },
            )
            .await;
        server
            .join()
            .map_err(|_| "tool bound fixture panicked".to_owned())??;
        assert_eq!(
            result,
            Err(PortError::Failed {
                reason: "MCP-GW unavailable while attempting to read bounded MCP-GW response"
                    .to_owned(),
            })
        );
        Ok(())
    }

    async fn chunked_profile_of(
        mode: SessionMode,
        body_bytes: usize,
    ) -> Result<Result<Value, PortError>, String> {
        let (mut fixture, address) = McpFixture::bind(mode)?;
        let server = thread::spawn(move || -> Result<(), String> {
            let (stream, _) = fixture.tool_call()?;
            let written = write_chunked_mcp_payload(stream, "steward-github-me", body_bytes);
            if body_bytes > MAX_MCP_TOOL_RESPONSE_BYTES {
                // The client stops reading at the bound and may close the connection
                // before the rest of the body is written.
                return fixture.finish();
            }
            written?;
            let (stream, _) = fixture.tool_call()?;
            write_mcp_payload(
                stream,
                "steward-github-repositories",
                serde_json::json!({"total_count": 0}),
            )?;
            fixture.finish()
        });
        let gateway = GithubMcpGateway::new(&McpFixture::origin(address), "0.4.9")
            .map_err(|error| format!("build chunked gateway: {error:?}"))?;
        let result = gateway
            .execute(
                GithubBridgeOperation::Repositories,
                GithubBridgeRequest::Repositories {
                    query: String::new(),
                    page: 1,
                    per_page: 100,
                },
            )
            .await;
        server
            .join()
            .map_err(|_| "chunked fixture panicked".to_owned())??;
        Ok(result)
    }

    #[tokio::test]
    async fn chunked_tool_results_accumulate_up_to_exactly_the_tool_bound() -> Result<(), String> {
        for mode in SESSION_MODES {
            let at_bound = chunked_profile_of(mode, MAX_MCP_TOOL_RESPONSE_BYTES)
                .await?
                .map_err(|error| {
                    format!("{mode:?}: a result of exactly 1 MiB must be read: {error:?}")
                })?;
            assert_eq!(at_bound["login"], "alice");
            assert_eq!(
                chunked_profile_of(mode, MAX_MCP_TOOL_RESPONSE_BYTES + 1).await?,
                Err(PortError::Failed {
                    reason: "MCP-GW unavailable while attempting to read bounded MCP-GW response"
                        .to_owned(),
                }),
                "{mode:?}"
            );
        }
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
