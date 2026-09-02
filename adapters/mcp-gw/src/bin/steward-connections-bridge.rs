use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use steward_adapter_mcp_gw::{GithubBridgeOperation, GithubBridgeRequest, GithubMcpGateway};
use steward_ports::PortError;

const REQUEST_FILE: &str = "request.json";
const RESPONSE_FILE: &str = "response.json";
const MAX_REQUEST_BYTES: u64 = 32 * 1024;

#[derive(Debug, Eq, PartialEq)]
struct Invocation {
    operation: GithubBridgeOperation,
}

fn parse_invocation(arguments: &[String]) -> Result<Invocation, String> {
    let [_, operation_flag, operation, input_flag, input] = arguments else {
        return Err(
            "bridge invocation must contain one operation and request.json input".to_owned(),
        );
    };
    if operation_flag != "--operation" || input_flag != "--input" || input != REQUEST_FILE {
        return Err(
            "bridge invocation must contain one allowlisted operation and request.json input"
                .to_owned(),
        );
    }
    Ok(Invocation {
        operation: GithubBridgeOperation::parse(operation)
            .map_err(|_| "bridge operation is not allowlisted".to_owned())?,
    })
}

fn required_environment(name: &str) -> Result<String, String> {
    env::var(name).map_err(|_| format!("{name} is required"))
}

fn request_bytes(path: &Path) -> Result<Vec<u8>, String> {
    let metadata =
        fs::metadata(path).map_err(|_| "bridge request.json input is unavailable".to_owned())?;
    if !metadata.is_file() || metadata.len() > MAX_REQUEST_BYTES {
        return Err("bridge request.json input is invalid".to_owned());
    }
    fs::read(path).map_err(|_| "bridge request.json input is unreadable".to_owned())
}

fn response_path() -> Result<PathBuf, String> {
    let output_directory = PathBuf::from(required_environment("STEWARD_OUTPUT_DIR")?);
    if !output_directory.is_absolute() || !output_directory.is_dir() {
        return Err("STEWARD_OUTPUT_DIR must be an existing absolute directory".to_owned());
    }
    Ok(output_directory.join(RESPONSE_FILE))
}

fn gateway_failure(error: &PortError) -> &'static str {
    match error {
        PortError::Failed { reason } if reason == "MCP-GW rejected runtime authentication" => {
            "bridge MCP-GW rejected runtime authentication"
        }
        PortError::Failed { reason } if reason == "MCP-GW rejected runtime authorization" => {
            "bridge MCP-GW rejected runtime authorization"
        }
        PortError::Failed { reason }
            if reason == "MCP-GW unavailable while attempting to call MCP-GW" =>
        {
            "bridge MCP-GW transport is unavailable"
        }
        PortError::Failed { reason }
            if reason == "MCP-GW unavailable while attempting to read MCP-GW response"
                || reason
                    == "MCP-GW unavailable while attempting to read bounded MCP-GW response" =>
        {
            "bridge MCP-GW response body is unavailable"
        }
        PortError::Failed { reason }
            if matches!(
                reason.as_str(),
                "MCP-GW unavailable while attempting to read GitHub connection status"
                    | "MCP-GW unavailable while attempting to start GitHub connection"
                    | "MCP-GW unavailable while attempting to disconnect GitHub connection"
            ) =>
        {
            "bridge MCP-GW returned an unexpected status"
        }
        PortError::Rejected { .. } => "bridge MCP-GW response violated its bounded contract",
        PortError::Failed { .. } | PortError::Unsupported { .. } => "bridge MCP-GW is unavailable",
        _ => "bridge MCP-GW is unavailable",
    }
}

async fn execute(arguments: &[String]) -> Result<(), String> {
    let origin = required_environment("STEWARD_MCP_GW_ORIGIN")?;
    execute_at(
        arguments,
        PathBuf::from(REQUEST_FILE),
        origin,
        response_path()?,
    )
    .await
}

async fn execute_at(
    arguments: &[String],
    request_path: PathBuf,
    origin: String,
    response_path: PathBuf,
) -> Result<(), String> {
    let invocation = parse_invocation(arguments)?;
    let request = GithubBridgeRequest::parse(invocation.operation, &request_bytes(&request_path)?)
        .map_err(|_| "bridge request.json violates the operation contract".to_owned())?;
    let gateway = GithubMcpGateway::new(&origin)
        .map_err(|_| "STEWARD_MCP_GW_ORIGIN is not an exact HTTP(S) origin".to_owned())?;
    let response = gateway
        .execute(invocation.operation, request)
        .await
        .map_err(|error| gateway_failure(&error).to_owned())?;
    let response = serde_json::to_vec(&response)
        .map_err(|_| "bridge response could not be serialized".to_owned())?;
    fs::write(response_path, response)
        .map_err(|_| "bridge response.json could not be persisted".to_owned())
}

#[tokio::main]
async fn main() {
    if let Err(error) = execute(&env::args().collect::<Vec<_>>()).await {
        eprintln!("steward-connections-bridge: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{ErrorKind, Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::thread;
    use std::time::Duration;

    use super::{GithubBridgeOperation, Invocation, execute_at, gateway_failure, parse_invocation};
    use steward_ports::PortError;

    static TEST_ID: AtomicU64 = AtomicU64::new(0);

    fn test_directory() -> Result<PathBuf, String> {
        let path = std::env::temp_dir().join(format!(
            "steward-connections-bridge-{}-{}",
            std::process::id(),
            TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).map_err(|error| format!("create test directory: {error}"))?;
        Ok(path)
    }

    #[test]
    fn invocation_accepts_only_an_allowlisted_operation_and_exact_request_file() {
        let arguments = vec![
            "steward-connections-bridge".to_owned(),
            "--operation".to_owned(),
            "github.status".to_owned(),
            "--input".to_owned(),
            "request.json".to_owned(),
        ];

        assert_eq!(
            parse_invocation(&arguments),
            Ok(Invocation {
                operation: GithubBridgeOperation::Status,
            }),
            "the bridge must accept exactly its server-authored operation and input ABI"
        );
        for hostile in [
            vec![
                "steward-connections-bridge".to_owned(),
                "--operation".to_owned(),
                "github.status".to_owned(),
                "--input".to_owned(),
                "../request.json".to_owned(),
            ],
            vec![
                "steward-connections-bridge".to_owned(),
                "--operation".to_owned(),
                "github.exec".to_owned(),
                "--input".to_owned(),
                "request.json".to_owned(),
            ],
            vec![
                "steward-connections-bridge".to_owned(),
                "--operation".to_owned(),
                "github.status".to_owned(),
                "--input".to_owned(),
                "request.json".to_owned(),
                "--extra".to_owned(),
            ],
        ] {
            assert!(
                parse_invocation(&hostile).is_err(),
                "the bridge must reject every non-ABI argument shape"
            );
        }
    }

    #[tokio::test]
    async fn one_shot_status_uses_only_the_placeholder_and_persists_one_response()
    -> Result<(), String> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|error| format!("bind MCP-GW fixture: {error}"))?;
        let origin = format!(
            "http://{}",
            listener
                .local_addr()
                .map_err(|error| format!("read MCP-GW fixture address: {error}"))?
        );
        let server = thread::spawn(move || -> Result<String, String> {
            let (mut stream, _) = listener
                .accept()
                .map_err(|error| format!("accept MCP-GW fixture request: {error}"))?;
            let mut bytes = [0_u8; 2048];
            let count = stream
                .read(&mut bytes)
                .map_err(|error| format!("read MCP-GW fixture request: {error}"))?;
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 19\r\nconnection: close\r\n\r\n{\"connected\":false}",
                )
                .map_err(|error| format!("write MCP-GW fixture response: {error}"))?;
            String::from_utf8(bytes[..count].to_vec())
                .map_err(|error| format!("decode MCP-GW fixture request: {error}"))
        });
        let directory = test_directory()?;
        let input = directory.join("request.json");
        let output = directory.join("response.json");
        fs::write(&input, b"{}")
            .map_err(|error| format!("write bridge request fixture: {error}"))?;
        let arguments = vec![
            "steward-connections-bridge".to_owned(),
            "--operation".to_owned(),
            "github.status".to_owned(),
            "--input".to_owned(),
            "request.json".to_owned(),
        ];

        execute_at(&arguments, input, origin, output.clone()).await?;

        let request = server
            .join()
            .map_err(|_| "MCP-GW fixture thread panicked".to_owned())??;
        assert!(
            request.contains("GET /oauth/github/status HTTP/1.1"),
            "the bridge must use only the fixed GitHub status route"
        );
        assert!(
            request.contains("authorization: Bearer openshell-token-grant-placeholder")
                || request.contains("Authorization: Bearer openshell-token-grant-placeholder"),
            "the supervisor placeholder is the only bridge bearer input"
        );
        assert!(
            !request.contains("x-acting-user") && !request.contains("x-canonical-user"),
            "the bridge must not send browser or canonical identity headers"
        );
        assert_eq!(
            fs::read(&output).map_err(|error| format!("read bridge response fixture: {error}"))?,
            br#"{"connected":false}"#,
            "the bridge must persist only the validated MCP-GW response"
        );
        fs::remove_dir_all(directory).map_err(|error| format!("remove bridge fixture: {error}"))?;
        Ok(())
    }

    #[tokio::test]
    async fn runtime_authentication_failure_is_reported_as_only_a_safe_category()
    -> Result<(), String> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|error| format!("bind MCP-GW fixture: {error}"))?;
        let origin = format!(
            "http://{}",
            listener
                .local_addr()
                .map_err(|error| format!("read MCP-GW fixture address: {error}"))?
        );
        listener
            .set_nonblocking(true)
            .map_err(|error| format!("make MCP-GW fixture nonblocking: {error}"))?;
        let finished = Arc::new(AtomicBool::new(false));
        let server_finished = Arc::clone(&finished);
        let server = thread::spawn(move || -> Result<(), String> {
            while !server_finished.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).map_err(|error| {
                            format!("make MCP-GW fixture request blocking: {error}")
                        })?;
                        let mut bytes = [0_u8; 2048];
                        stream
                            .read(&mut bytes)
                            .map_err(|error| format!("read MCP-GW fixture request: {error}"))?;
                        stream
                            .write_all(
                                b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                            )
                            .map_err(|error| {
                                format!("write MCP-GW fixture response: {error}")
                            })?;
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => {
                        return Err(format!("accept MCP-GW fixture request: {error}"));
                    }
                }
            }
            Ok(())
        });
        let directory = test_directory()?;
        let input = directory.join("request.json");
        let output = directory.join("response.json");
        fs::write(&input, b"{}").map_err(|error| format!("write bridge request: {error}"))?;
        let arguments = vec![
            "steward-connections-bridge".to_owned(),
            "--operation".to_owned(),
            "github.status".to_owned(),
            "--input".to_owned(),
            "request.json".to_owned(),
        ];

        let error = match execute_at(&arguments, input, origin, output).await {
            Err(error) => error,
            Ok(()) => {
                return Err("the bridge must reject runtime authentication failure".to_owned());
            }
        };

        finished.store(true, Ordering::Release);
        server
            .join()
            .map_err(|_| "MCP-GW fixture thread panicked".to_owned())??;
        assert_eq!(
            error, "bridge MCP-GW rejected runtime authentication",
            "bridge diagnostics must preserve only the fixed non-secret failure category"
        );
        fs::remove_dir_all(directory).map_err(|error| format!("remove bridge fixture: {error}"))?;
        Ok(())
    }

    #[test]
    fn gateway_failures_preserve_only_actionable_non_secret_categories() {
        assert_eq!(
            gateway_failure(&PortError::Failed {
                reason: "MCP-GW unavailable while attempting to call MCP-GW".to_owned(),
            }),
            "bridge MCP-GW transport is unavailable"
        );
        assert_eq!(
            gateway_failure(&PortError::Failed {
                reason: "MCP-GW unavailable while attempting to read GitHub connection status"
                    .to_owned(),
            }),
            "bridge MCP-GW returned an unexpected status"
        );
        assert_eq!(
            gateway_failure(&PortError::Failed {
                reason: "MCP-GW unavailable while attempting to read MCP-GW response".to_owned(),
            }),
            "bridge MCP-GW response body is unavailable"
        );
    }
}
