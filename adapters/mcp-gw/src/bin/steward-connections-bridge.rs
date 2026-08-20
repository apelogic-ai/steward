use std::env;
use std::fs;
use std::path::PathBuf;

use steward_adapter_mcp_gw::{GithubBridgeOperation, GithubBridgeRequest, GithubMcpGateway};

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

fn request_bytes() -> Result<Vec<u8>, String> {
    let metadata = fs::metadata(REQUEST_FILE)
        .map_err(|_| "bridge request.json input is unavailable".to_owned())?;
    if !metadata.is_file() || metadata.len() > MAX_REQUEST_BYTES {
        return Err("bridge request.json input is invalid".to_owned());
    }
    fs::read(REQUEST_FILE).map_err(|_| "bridge request.json input is unreadable".to_owned())
}

fn response_path() -> Result<PathBuf, String> {
    let output_directory = PathBuf::from(required_environment("STEWARD_OUTPUT_DIR")?);
    if !output_directory.is_absolute() || !output_directory.is_dir() {
        return Err("STEWARD_OUTPUT_DIR must be an existing absolute directory".to_owned());
    }
    Ok(output_directory.join(RESPONSE_FILE))
}

async fn execute(arguments: &[String]) -> Result<(), String> {
    let invocation = parse_invocation(arguments)?;
    let request = GithubBridgeRequest::parse(invocation.operation, &request_bytes()?)
        .map_err(|_| "bridge request.json violates the operation contract".to_owned())?;
    let origin = required_environment("STEWARD_MCP_GW_ORIGIN")?;
    let gateway = GithubMcpGateway::new(&origin)
        .map_err(|_| "STEWARD_MCP_GW_ORIGIN is not an exact HTTP(S) origin".to_owned())?;
    let response = gateway
        .execute(invocation.operation, request)
        .await
        .map_err(|_| "bridge operation did not receive a valid MCP-GW response".to_owned())?;
    let response = serde_json::to_vec(&response)
        .map_err(|_| "bridge response could not be serialized".to_owned())?;
    fs::write(response_path()?, response)
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
    use super::{GithubBridgeOperation, Invocation, parse_invocation};

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
}
