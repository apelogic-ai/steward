//! MCP-GW adapter for the one-shot, provider-attached Connections bridge.
//!
//! The bridge sends OpenShell's documented bearer placeholder only. The sandbox
//! supervisor replaces it at the governed egress boundary; this adapter never
//! accepts or reads a credential, provider origin, or caller identity.

use std::time::{Duration, Instant};

use reqwest::header::AUTHORIZATION;
use reqwest::{Client, Method, StatusCode, Url};
use serde_json::{Map, Value, json};
use steward_ports::PortError;

pub const IMPLEMENTED_PORTS: [&str; 0] = [];
const OPEN_SHELL_BEARER_PLACEHOLDER: &str = "openshell-token-grant-placeholder";
const MAX_RESPONSE_BYTES: usize = 32 * 1024;
const PROVIDER_TRANSPORT_READY_TIMEOUT: Duration = Duration::from_secs(12);
const PROVIDER_TRANSPORT_RETRY_INTERVAL: Duration = Duration::from_millis(250);
const DIRECT_STATUS_TIMEOUT: Duration = Duration::from_secs(1);
const LEGACY_STATUS_PATH: &str = "/oauth/github/status";
const LIFECYCLE_STATUS_PATH: &str = "/connections/github/status";
const START_PATH: &str = "/oauth/github/start";
const DISCONNECT_PATH: &str = "/oauth/github/disconnect";
const MCP_PATH: &str = "/mcp";
const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
const RERUN_REQUEST_ID: &str = "steward-github-rerun";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GithubBridgeOperation {
    Status,
    Start,
    Disconnect,
    Rerun,
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
    pub fn parse(value: &str) -> Result<Self, PortError> {
        match value {
            "github.status" => Ok(Self::Status),
            "github.start" => Ok(Self::Start),
            "github.disconnect" => Ok(Self::Disconnect),
            "github.rerun" => Ok(Self::Rerun),
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
            Self::Rerun => (Method::POST, MCP_PATH),
        }
    }
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
        }
    }
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
            .send()
            .await
            .map_err(|_| unavailable("read direct GitHub connection status"))?;
        let status = response.status();
        let body = read_bounded(response).await?;
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
            let body = read_bounded(response).await?;
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
}

fn pre_dispatch_provider_failure(status: StatusCode, body: &[u8]) -> bool {
    if status == StatusCode::UNAUTHORIZED {
        return true;
    }
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
            require_status(status, StatusCode::OK, "read GitHub connection status")?;
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
            require_status(status, StatusCode::OK, "start GitHub connection")?;
            let object = json_object(body, "GitHub start response")?;
            let authorization_url = exact_string_field(&object, "authorizationUrl")?;
            validate_authorization_url(&authorization_url)?;
            Ok(json!({"authorizationUrl": authorization_url}))
        }
        GithubBridgeOperation::Disconnect => {
            require_status(
                status,
                StatusCode::NO_CONTENT,
                "disconnect GitHub connection",
            )?;
            if !body.is_empty() {
                return Err(unavailable("disconnect GitHub connection"));
            }
            Ok(json!({"disconnected": true}))
        }
        GithubBridgeOperation::Rerun => {
            require_status(status, StatusCode::OK, "re-run GitHub workflow")?;
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
    }
}

fn require_status(
    actual: StatusCode,
    expected: StatusCode,
    operation: &str,
) -> Result<(), PortError> {
    if actual == expected {
        Ok(())
    } else if actual == StatusCode::UNAUTHORIZED {
        Err(failed("MCP-GW rejected runtime authentication"))
    } else if actual == StatusCode::FORBIDDEN {
        Err(failed("MCP-GW rejected runtime authorization"))
    } else {
        Err(unavailable(operation))
    }
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
    if object.get("version").and_then(Value::as_str) != Some("1")
        || object.get("provider").and_then(Value::as_str) != Some("github")
    {
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
                .filter(|account| account.len() == 1)
                .ok_or_else(|| rejected("GitHub lifecycle account is invalid"))?;
            account
                .get("displayName")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty() && name.len() <= 320)
                .map(str::to_owned)
                .ok_or_else(|| rejected("GitHub lifecycle account is invalid"))
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
    if let Some(account) = account {
        projection.insert("email".to_owned(), Value::String(account));
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

async fn read_bounded(response: reqwest::Response) -> Result<Vec<u8>, PortError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(unavailable("read bounded MCP-GW response"));
    }
    let body = response
        .bytes()
        .await
        .map_err(|_| unavailable("read MCP-GW response"))?;
    if body.len() > MAX_RESPONSE_BYTES {
        return Err(unavailable("read bounded MCP-GW response"));
    }
    Ok(body.to_vec())
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
        GatewayContract, GithubBridgeOperation, GithubBridgeRequest, GithubMcpGateway,
        GithubStatusCredential, GithubStatusReader, parse_response, pre_dispatch_provider_failure,
    };
    use reqwest::StatusCode;
    use steward_ports::PortError;

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
        assert!(GithubBridgeOperation::parse("github.dispatch").is_err());
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
        assert_eq!(
            parse_response(
                GatewayContract::LegacyV032,
                GithubBridgeOperation::Status,
                StatusCode::BAD_GATEWAY,
                b"ignored",
            ),
            Err(PortError::Failed {
                reason: "MCP-GW unavailable while attempting to read GitHub connection status"
                    .to_owned(),
            })
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
            let body = r#"{"version":"1","provider":"github","phase":"disconnected","connected":false,"requiredScopes":["repo"],"grantedScopes":[],"missingScopes":["repo"],"activeCredentialExpiresAt":null,"renewalCredentialExpiresAt":null,"lastAuthorizedAt":null,"lastRenewedAt":null,"lastValidatedAt":null,"capabilities":{"interactiveAuthorization":true}}"#;
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
                "phase": "disconnected",
                "connected": false,
                "scopesRequired": ["repo"],
                "scopesGranted": [],
                "missingScopes": ["repo"],
                "activeCredentialExpiresAt": null,
                "renewalCredentialExpiresAt": null
            })
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
