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
const STATUS_PATH: &str = "/oauth/github/status";
const START_PATH: &str = "/oauth/github/start";
const DISCONNECT_PATH: &str = "/oauth/github/disconnect";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GithubBridgeOperation {
    Status,
    Start,
    Disconnect,
}

impl GithubBridgeOperation {
    pub fn parse(value: &str) -> Result<Self, PortError> {
        match value {
            "github.status" => Ok(Self::Status),
            "github.start" => Ok(Self::Start),
            "github.disconnect" => Ok(Self::Disconnect),
            _ => Err(rejected("Connections bridge operation is not allowlisted")),
        }
    }

    fn method_and_path(self) -> (Method, &'static str) {
        match self {
            Self::Status => (Method::GET, STATUS_PATH),
            Self::Start => (Method::POST, START_PATH),
            Self::Disconnect => (Method::POST, DISCONNECT_PATH),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GithubBridgeRequest {
    Empty,
    Start { redirect_after: String },
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
        }
    }

    fn body(&self) -> Option<Value> {
        match self {
            Self::Empty => None,
            Self::Start { redirect_after } => Some(json!({"redirectAfter": redirect_after})),
        }
    }
}

#[derive(Clone)]
pub struct GithubMcpGateway {
    client: Client,
    origin: Url,
}

impl GithubMcpGateway {
    pub fn new(origin: &str) -> Result<Self, PortError> {
        let origin = validate_origin(origin)?;
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|_| unavailable("build MCP-GW client"))?;
        Ok(Self { client, origin })
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
            )
        ) {
            return Err(rejected(
                "Connections bridge request does not match its allowlisted operation",
            ));
        }
        let (method, path) = operation.method_and_path();
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
        parse_response(operation, status, &body)
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
    operation: GithubBridgeOperation,
    status: StatusCode,
    body: &[u8],
) -> Result<Value, PortError> {
    match operation {
        GithubBridgeOperation::Status => {
            require_status(status, StatusCode::OK, "read GitHub connection status")?;
            let object = json_object(body, "GitHub status response")?;
            validate_status_response(&object)?;
            Ok(Value::Object(object))
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
        GithubBridgeOperation, GithubBridgeRequest, GithubMcpGateway, parse_response,
        pre_dispatch_provider_failure,
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
    fn bridge_responses_accept_the_disconnected_mcp_gw_contract_and_reject_malformed_values() {
        assert!(
            parse_response(
                GithubBridgeOperation::Status,
                StatusCode::OK,
                br#"{"connected":false}"#,
            )
            .is_ok(),
            "MCP-GW reports an absent or revoked account as only connected=false"
        );
        assert!(
            parse_response(
                GithubBridgeOperation::Status,
                StatusCode::OK,
                br#"{"connected":"false"}"#,
            )
            .is_err(),
            "a status with a non-boolean connection state cannot become a persisted response"
        );
        assert!(
            parse_response(
                GithubBridgeOperation::Status,
                StatusCode::OK,
                br#"{"connected":true}"#,
            )
            .is_err(),
            "a connected status without its verified account identity must fail closed"
        );
        assert!(
            parse_response(
                GithubBridgeOperation::Start,
                StatusCode::OK,
                br#"{"authorizationUrl":"http://github.example.test/authorize"}"#,
            )
            .is_err(),
            "a non-HTTPS authorization URL must never reach the browser"
        );
        assert!(
            parse_response(GithubBridgeOperation::Disconnect, StatusCode::OK, b"").is_err(),
            "disconnect is only complete on the exact MCP-GW no-content response"
        );
    }

    #[test]
    fn provider_control_http_failures_are_reduced_to_non_secret_runtime_categories() {
        assert_eq!(
            parse_response(
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
        let gateway = GithubMcpGateway::new(&format!("http://{address}"))
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
        let gateway = GithubMcpGateway::new(&format!("http://{address}"))
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
