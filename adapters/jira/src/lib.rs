//! Jira implementation of Steward's outbound-only decision channel.

use serde::{Deserialize, Serialize};
use steward_ports::{
    DecisionChannel, DecisionReference, DecisionRequest, DecisionResolution, PortError,
};

pub const IMPLEMENTED_PORTS: [&str; 1] = ["DecisionChannel"];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JiraConfig {
    pub base_url: String,
    pub project_key: String,
    pub account_email: String,
}

#[derive(Clone)]
pub struct JiraAdapter {
    client: reqwest::Client,
    config: JiraConfig,
    api_token: String,
}

impl JiraAdapter {
    pub fn new(config: JiraConfig, api_token: String) -> Result<Self, PortError> {
        if config.base_url.is_empty()
            || config.project_key.is_empty()
            || config.account_email.is_empty()
            || api_token.is_empty()
        {
            return Err(PortError::Rejected {
                reason: "Jira endpoint, project, account, and API token are required".to_owned(),
            });
        }
        let mut config = config;
        config.base_url = config.base_url.trim_end_matches('/').to_owned();
        Ok(Self {
            client: reqwest::Client::new(),
            config,
            api_token,
        })
    }
}

impl DecisionChannel for JiraAdapter {
    async fn request(&self, request: &DecisionRequest) -> Result<DecisionReference, PortError> {
        let marker = approval_marker(&request.request_id);
        let jql = format!(
            "project = {} AND labels = \"{}\"",
            self.config.project_key, marker
        );
        let response = self
            .authorized(
                self.client
                    .get(format!("{}/rest/api/3/search/jql", self.config.base_url))
                    .query(&[
                        ("jql", jql.as_str()),
                        ("fields", "key"),
                        ("maxResults", "2"),
                    ]),
            )
            .send()
            .await
            .map_err(|error| request_error("search Jira approval requests", error))?;
        if !response.status().is_success() {
            return Err(status_error(
                "search Jira approval requests",
                response.status(),
            ));
        }
        let search = response
            .json::<SearchResponse>()
            .await
            .map_err(|error| request_error("decode Jira search response", error))?;
        match search.issues.as_slice() {
            [] => {}
            [issue] => return self.reference(&issue.key),
            _ => {
                return Err(PortError::Rejected {
                    reason: "multiple Jira issues carry the same Steward approval marker"
                        .to_owned(),
                });
            }
        }

        let description = format!(
            "Steward approval request {}\nRuntime UID: {}\nActor: {}\nMember role: {}\n{}",
            request.request_id,
            request.runtime_uid,
            request.actor,
            request.member_role,
            request.counterexample
        );
        let body = CreateIssue {
            fields: CreateIssueFields {
                project: ProjectRef {
                    key: self.config.project_key.clone(),
                },
                summary: format!("Steward approval for runtime {}", request.runtime_uid),
                description: AdfDocument::text(description),
                issue_type: IssueType {
                    name: "Task".to_owned(),
                },
                labels: vec!["steward-approval".to_owned(), marker],
            },
        };
        let response = self
            .authorized(
                self.client
                    .post(format!("{}/rest/api/3/issue", self.config.base_url))
                    .json(&body),
            )
            .send()
            .await
            .map_err(|error| request_error("create Jira approval request", error))?;
        if !response.status().is_success() {
            return Err(status_error(
                "create Jira approval request",
                response.status(),
            ));
        }
        let issue = response
            .json::<CreatedIssue>()
            .await
            .map_err(|error| request_error("decode Jira create response", error))?;
        self.reference(&issue.key)
    }

    async fn record_resolution(&self, resolution: &DecisionResolution) -> Result<(), PortError> {
        validate_issue_key(&resolution.key)?;
        let comment = format!(
            "Steward resolved approval request {}.\nDecided by: {}\nRationale: {}\nEvidence: {}",
            resolution.request_id,
            resolution.decided_by,
            resolution.rationale,
            resolution.evidence_url
        );
        let response = self
            .authorized(
                self.client
                    .post(format!(
                        "{}/rest/api/3/issue/{}/comment",
                        self.config.base_url, resolution.key
                    ))
                    .json(&CommentRequest {
                        body: AdfDocument::text(comment),
                    }),
            )
            .send()
            .await
            .map_err(|error| request_error("comment on Jira approval request", error))?;
        if !response.status().is_success() {
            return Err(status_error(
                "comment on Jira approval request",
                response.status(),
            ));
        }
        Ok(())
    }
}

impl JiraAdapter {
    fn authorized(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request.basic_auth(&self.config.account_email, Some(&self.api_token))
    }

    fn reference(&self, key: &str) -> Result<DecisionReference, PortError> {
        validate_issue_key(key)?;
        Ok(DecisionReference {
            key: key.to_owned(),
            evidence_url: format!("{}/browse/{key}", self.config.base_url),
        })
    }
}

#[derive(Deserialize)]
struct SearchResponse {
    issues: Vec<JiraIssue>,
}

#[derive(Deserialize)]
struct JiraIssue {
    key: String,
}

#[derive(Deserialize)]
struct CreatedIssue {
    key: String,
}

#[derive(Serialize)]
struct CreateIssue {
    fields: CreateIssueFields,
}

#[derive(Serialize)]
struct CreateIssueFields {
    project: ProjectRef,
    summary: String,
    description: AdfDocument,
    #[serde(rename = "issuetype")]
    issue_type: IssueType,
    labels: Vec<String>,
}

#[derive(Serialize)]
struct ProjectRef {
    key: String,
}

#[derive(Serialize)]
struct IssueType {
    name: String,
}

#[derive(Serialize)]
struct CommentRequest {
    body: AdfDocument,
}

#[derive(Serialize)]
struct AdfDocument {
    version: u8,
    #[serde(rename = "type")]
    kind: &'static str,
    content: Vec<AdfParagraph>,
}

impl AdfDocument {
    fn text(text: String) -> Self {
        Self {
            version: 1,
            kind: "doc",
            content: vec![AdfParagraph {
                kind: "paragraph",
                content: vec![AdfText { kind: "text", text }],
            }],
        }
    }
}

#[derive(Serialize)]
struct AdfParagraph {
    #[serde(rename = "type")]
    kind: &'static str,
    content: Vec<AdfText>,
}

#[derive(Serialize)]
struct AdfText {
    #[serde(rename = "type")]
    kind: &'static str,
    text: String,
}

fn approval_marker(request_id: &str) -> String {
    let suffix = request_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    format!("steward-approval-{suffix}")
}

fn validate_issue_key(key: &str) -> Result<(), PortError> {
    if !key.is_empty()
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        Ok(())
    } else {
        Err(PortError::Rejected {
            reason: "Jira returned an invalid issue key".to_owned(),
        })
    }
}

fn request_error(operation: &str, error: reqwest::Error) -> PortError {
    PortError::Failed {
        reason: format!("{operation} failed: {error}"),
    }
}

fn status_error(operation: &str, status: reqwest::StatusCode) -> PortError {
    PortError::Failed {
        reason: format!("{operation} returned HTTP {status}"),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::thread;

    use steward_ports::{DecisionChannel, DecisionRequest, DecisionResolution};

    use super::{JiraAdapter, JiraConfig};

    struct MockJira {
        base_url: String,
        requests: Arc<Mutex<Vec<String>>>,
        server: thread::JoinHandle<Result<(), String>>,
    }

    impl MockJira {
        fn start(responses: Vec<&str>) -> Result<Self, String> {
            let listener = TcpListener::bind(("localhost", 0))
                .map_err(|error| format!("failed to bind mock Jira: {error}"))?;
            let address = listener
                .local_addr()
                .map_err(|error| format!("failed to read mock Jira address: {error}"))?;
            let requests = Arc::new(Mutex::new(Vec::new()));
            let server_requests = requests.clone();
            let responses = responses.into_iter().map(str::to_owned).collect::<Vec<_>>();
            let server = thread::spawn(move || {
                for response_body in responses {
                    let (mut stream, _) = listener
                        .accept()
                        .map_err(|error| format!("mock Jira accept failed: {error}"))?;
                    let request = read_request(&mut stream)?;
                    server_requests
                        .lock()
                        .map_err(|_| "mock Jira request lock was poisoned".to_owned())?
                        .push(request);
                    write_response(&mut stream, &response_body)?;
                }
                Ok(())
            });
            Ok(Self {
                base_url: format!("http://{address}"),
                requests,
                server,
            })
        }

        fn finish(self) -> Result<Vec<String>, String> {
            self.server
                .join()
                .map_err(|_| "mock Jira server panicked".to_owned())??;
            self.requests
                .lock()
                .map_err(|_| "mock Jira request lock was poisoned".to_owned())
                .map(|requests| requests.clone())
        }
    }

    #[tokio::test]
    async fn files_a_prepopulated_issue_and_returns_its_reference() -> Result<(), String> {
        let jira = MockJira::start(vec![r#"{"issues":[]}"#, r#"{"key":"PROJ-123"}"#])?;
        let adapter = JiraAdapter::new(
            JiraConfig {
                base_url: jira.base_url.clone(),
                project_key: "PROJ".to_owned(),
                account_email: "alice@example.com".to_owned(),
            },
            "test-token".to_owned(),
        )
        .map_err(|error| format!("failed to construct Jira adapter: {error:?}"))?;
        let request = DecisionRequest {
            request_id: "approval-a".to_owned(),
            runtime_uid: "runtime-a".to_owned(),
            actor: "bob@example.org".to_owned(),
            member_role: "engineer".to_owned(),
            counterexample:
                "envelope exceeded: budget.monthlyLimit requested 220.00 USD, ceiling 200.00 USD"
                    .to_owned(),
        };

        let reference = adapter
            .request(&request)
            .await
            .map_err(|error| format!("Jira request failed: {error:?}"))?;
        assert_eq!(reference.key, "PROJ-123");
        assert_eq!(
            reference.evidence_url,
            format!("{}/browse/PROJ-123", jira.base_url)
        );

        let requests = jira.finish()?;
        assert_eq!(requests.len(), 2);
        assert!(
            requests[0].starts_with("GET /rest/api/3/search/jql?"),
            "the adapter must search by its stable approval marker before creating: {}",
            requests[0]
        );
        let create = &requests[1];
        assert!(create.starts_with("POST /rest/api/3/issue "));
        for expected in [
            "approval-a",
            "runtime-a",
            "bob@example.org",
            "engineer",
            "requested 220.00 USD",
        ] {
            assert!(
                create.contains(expected),
                "the pre-populated Jira issue must contain {expected:?}: {create}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn comments_stewards_resolution_back_to_jira() -> Result<(), String> {
        let jira = MockJira::start(vec!["{}"])?;
        let adapter = JiraAdapter::new(
            JiraConfig {
                base_url: jira.base_url.clone(),
                project_key: "PROJ".to_owned(),
                account_email: "alice@example.com".to_owned(),
            },
            "test-token".to_owned(),
        )
        .map_err(|error| format!("failed to construct Jira adapter: {error:?}"))?;
        let resolution = DecisionResolution {
            request_id: "approval-a".to_owned(),
            key: "PROJ-123".to_owned(),
            decided_by: "admin@example.com".to_owned(),
            rationale: "approved for this runtime".to_owned(),
            evidence_url: "https://jira.example.com/browse/PROJ-123".to_owned(),
        };

        adapter
            .record_resolution(&resolution)
            .await
            .map_err(|error| format!("Jira comment failed: {error:?}"))?;
        let requests = jira.finish()?;
        assert_eq!(requests.len(), 1);
        let comment = &requests[0];
        assert!(comment.starts_with("POST /rest/api/3/issue/PROJ-123/comment "));
        for expected in [
            "approval-a",
            "admin@example.com",
            "approved for this runtime",
            "https://jira.example.com/browse/PROJ-123",
        ] {
            assert!(
                comment.contains(expected),
                "the Jira resolution comment must contain {expected:?}: {comment}"
            );
        }
        Ok(())
    }

    fn read_request(stream: &mut TcpStream) -> Result<String, String> {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let read = stream
                .read(&mut buffer)
                .map_err(|error| format!("mock Jira read failed: {error}"))?;
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..read]);
            if request_is_complete(&bytes)? {
                break;
            }
        }
        String::from_utf8(bytes)
            .map_err(|error| format!("mock Jira request was not UTF-8: {error}"))
    }

    fn request_is_complete(bytes: &[u8]) -> Result<bool, String> {
        let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
            return Ok(false);
        };
        let headers = std::str::from_utf8(&bytes[..header_end])
            .map_err(|error| format!("mock Jira headers were not UTF-8: {error}"))?;
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(str::trim)
                    .map(str::to_owned)
            })
            .map(|value| {
                value
                    .parse::<usize>()
                    .map_err(|error| format!("invalid mock Jira content length: {error}"))
            })
            .transpose()?
            .unwrap_or_default();
        Ok(bytes.len() >= header_end + 4 + content_length)
    }

    fn write_response(stream: &mut TcpStream, body: &str) -> Result<(), String> {
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .map_err(|error| format!("mock Jira write failed: {error}"))
    }
}
