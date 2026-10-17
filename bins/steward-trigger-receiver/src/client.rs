use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::authentication::v1::{TokenRequest, TokenRequestSpec};
use k8s_openapi::api::core::v1::ServiceAccount;
use kube::Client as KubeClient;
use kube::api::{Api, PostParams};
use reqwest::header::CONTENT_TYPE;
use reqwest::{Certificate, Client, Method, Response, StatusCode, Url};
use serde::{Deserialize, Serialize};
use steward_adapter_openshell::{OpaqueToken, WorkloadExchangeTokenProvider};
use steward_apiserver::browser_admin::CapabilityCatalog;
use steward_store::{EnvelopeRequestRecord, EnvelopeRequestStatus, PgStore};
use steward_types::CanonicalUserId;
use steward_types::direct_package::{EnvelopeDigest, Uuid};
use tokio::time::{sleep, timeout};

use crate::{
    ReceiverFuture, TaskInvocation, TaskLifecycle, TaskLifecycleError, TaskStart, TriggerConfig,
};

const TOKEN_REQUEST_SECONDS: i64 = 600;
const MAX_TASK_REPLY_BYTES: u64 = 64 * 1024;

#[derive(Clone)]
pub struct PgEnvelopeAuthority {
    store: PgStore,
}

impl PgEnvelopeAuthority {
    pub fn new(store: PgStore) -> Self {
        Self { store }
    }
}

impl crate::EnvelopeAuthority for PgEnvelopeAuthority {
    fn active_tools<'a>(
        &'a self,
        owner: &'a CanonicalUserId,
        digest: &'a str,
    ) -> ReceiverFuture<'a, Result<Vec<steward_types::ToolGrant>, crate::EnvelopeError>> {
        Box::pin(async move {
            let digest = EnvelopeDigest::parse(digest.to_owned())
                .map_err(|_| crate::EnvelopeError::Denied)?;
            let store_digest = digest
                .as_str()
                .strip_prefix("steward:")
                .ok_or(crate::EnvelopeError::Denied)?;
            let rows = self
                .store
                .active_provisioned_user_envelopes_by_digest(owner, store_digest)
                .await
                .map_err(|_| crate::EnvelopeError::Unavailable)?;
            active_tools_from_rows(owner, store_digest, rows)
        })
    }
}

fn active_tools_from_rows(
    owner: &CanonicalUserId,
    digest: &str,
    mut rows: Vec<EnvelopeRequestRecord>,
) -> Result<Vec<steward_types::ToolGrant>, crate::EnvelopeError> {
    if rows.len() != 1 {
        return Err(if rows.is_empty() {
            crate::EnvelopeError::Absent
        } else {
            crate::EnvelopeError::Ambiguous
        });
    }
    let row = rows.pop().ok_or(crate::EnvelopeError::Absent)?;
    if row.owner_user_id != *owner {
        return Err(crate::EnvelopeError::Denied);
    }
    if row.status != EnvelopeRequestStatus::Provisioned
        || row.envelope_instance_id.is_none()
        || row.envelope_digest.as_deref() != Some(digest)
    {
        return Err(crate::EnvelopeError::Stale);
    }
    row.approved_envelope
        .map(|envelope| envelope.spec.tools)
        .ok_or(crate::EnvelopeError::Stale)
}

#[derive(Clone)]
pub struct FileCatalog {
    path: PathBuf,
}

impl FileCatalog {
    pub fn new(path: PathBuf) -> Result<Self, String> {
        if path.as_os_str().is_empty() {
            return Err("capability catalog file is required".to_owned());
        }
        Ok(Self { path })
    }
}

impl crate::CapabilityCatalogReader for FileCatalog {
    fn current(&self) -> ReceiverFuture<'_, Result<CapabilityCatalog, String>> {
        Box::pin(async move {
            let path = self.path.clone();
            tokio::task::spawn_blocking(move || {
                let json = std::fs::read_to_string(path)
                    .map_err(|_| "capability catalog is unavailable".to_owned())?;
                CapabilityCatalog::from_json(&json)
                    .map_err(|_| "capability catalog is invalid".to_owned())
            })
            .await
            .map_err(|_| "capability catalog is unavailable".to_owned())?
        })
    }
}

#[derive(Clone)]
pub struct KubernetesTokenRequester {
    client: KubeClient,
    namespace: String,
    service_account: String,
    audience: String,
}

impl KubernetesTokenRequester {
    pub fn new(
        client: KubeClient,
        namespace: String,
        service_account: String,
        audience: String,
    ) -> Result<Self, String> {
        if [
            namespace.as_str(),
            service_account.as_str(),
            audience.as_str(),
        ]
        .iter()
        .any(|value| value.trim().is_empty() || value.trim() != *value)
        {
            return Err("TokenRequest settings are incomplete".to_owned());
        }
        Ok(Self {
            client,
            namespace,
            service_account,
            audience,
        })
    }

    async fn request(&self) -> Result<String, TaskLifecycleError> {
        let api: Api<ServiceAccount> = Api::namespaced(self.client.clone(), &self.namespace);
        let request = TokenRequest {
            metadata: Default::default(),
            spec: TokenRequestSpec {
                audiences: vec![self.audience.clone()],
                expiration_seconds: Some(TOKEN_REQUEST_SECONDS),
                bound_object_ref: None,
            },
            status: None,
        };
        let response = api
            .create_token_request(&self.service_account, &PostParams::default(), &request)
            .await
            .map_err(|error| match error {
                kube::Error::Api(response) if response.code < 500 => TaskLifecycleError::Final,
                _ => TaskLifecycleError::Retriable,
            })?;
        response
            .status
            .map(|status| status.token)
            .filter(|token| !token.trim().is_empty() && token.trim() == token)
            .ok_or(TaskLifecycleError::Final)
    }
}

pub trait TaskTokenProvider: Send + Sync + 'static {
    fn token(&self) -> ReceiverFuture<'_, Result<OpaqueToken, TaskLifecycleError>>;
}

#[derive(Clone)]
pub struct KubernetesWorkloadTokenProvider {
    source: KubernetesTokenRequester,
    endpoint: String,
    server_name: String,
    ca_certificate_pem: Arc<Vec<u8>>,
}

impl KubernetesWorkloadTokenProvider {
    pub fn new(
        source: KubernetesTokenRequester,
        endpoint: String,
        server_name: String,
        ca_certificate_pem: Vec<u8>,
    ) -> Result<Self, String> {
        steward_adapter_openshell::validate_workload_exchange_endpoint(&endpoint, &server_name)
            .map_err(|_| "workload exchange endpoint is invalid".to_owned())?;
        if ca_certificate_pem.is_empty() {
            return Err("workload exchange CA certificate is required".to_owned());
        }
        Ok(Self {
            source,
            endpoint,
            server_name,
            ca_certificate_pem: Arc::new(ca_certificate_pem),
        })
    }
}

impl TaskTokenProvider for KubernetesWorkloadTokenProvider {
    fn token(&self) -> ReceiverFuture<'_, Result<OpaqueToken, TaskLifecycleError>> {
        Box::pin(async move {
            let source = self.source.request().await?;
            let provider = WorkloadExchangeTokenProvider::new(
                &self.endpoint,
                &self.server_name,
                &self.ca_certificate_pem,
                source,
            )
            .map_err(exchange_error)?;
            provider.access_token().await.map_err(exchange_error)
        })
    }
}

fn exchange_error(error: steward_ports::PortError) -> TaskLifecycleError {
    match error {
        steward_ports::PortError::Rejected { .. } => TaskLifecycleError::Final,
        _ => TaskLifecycleError::Retriable,
    }
}

#[derive(Clone, Copy)]
pub struct TaskClientTiming {
    pub deadline: Duration,
    pub initial_poll_delay: Duration,
    pub max_poll_delay: Duration,
}

impl TaskClientTiming {
    pub fn validate(self) -> Result<Self, String> {
        if self.deadline.is_zero()
            || self.initial_poll_delay.is_zero()
            || self.max_poll_delay < self.initial_poll_delay
        {
            return Err("Task polling settings are invalid".to_owned());
        }
        Ok(self)
    }
}

#[derive(Clone)]
pub struct WorkloadExchangeClient {
    http: Client,
    task_origin: Url,
    tokens: Arc<dyn TaskTokenProvider>,
    timing: TaskClientTiming,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TaskReply {
    task_uid: String,
    phase: String,
}

#[derive(Debug, Serialize)]
struct TaskRequest<'a> {
    workflow: &'a str,
    #[serde(rename = "envelopeDigest")]
    envelope_digest: &'a str,
}

impl WorkloadExchangeClient {
    pub fn new<T: TaskTokenProvider>(
        task_origin: String,
        server_name: String,
        ca_certificate_pem: Vec<u8>,
        tokens: T,
        timing: TaskClientTiming,
    ) -> Result<Self, String> {
        let task_origin = validate_task_origin(&task_origin, &server_name)?;
        let certificates = Certificate::from_pem_bundle(&ca_certificate_pem)
            .map_err(|_| "Steward API CA certificate is invalid".to_owned())?;
        if certificates.is_empty() {
            return Err("Steward API CA certificate is empty".to_owned());
        }
        let mut builder = Client::builder()
            .https_only(true)
            .tls_built_in_root_certs(false)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10));
        for certificate in certificates {
            builder = builder.add_root_certificate(certificate);
        }
        Ok(Self {
            http: builder
                .build()
                .map_err(|_| "Steward API HTTPS client is invalid".to_owned())?,
            task_origin,
            tokens: Arc::new(tokens),
            timing: timing.validate()?,
        })
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        configure: impl FnOnce(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
    ) -> Result<Response, TaskLifecycleError> {
        let token = self.tokens.token().await?;
        let url = self
            .task_origin
            .join(path)
            .map_err(|_| TaskLifecycleError::Final)?;
        configure(
            self.http
                .request(method, url)
                .bearer_auth(token.expose_secret()),
        )
        .send()
        .await
        .map_err(|_| TaskLifecycleError::Retriable)
    }

    async fn continue_task(
        self,
        uid: String,
        input_archive: Vec<u8>,
    ) -> Result<(), TaskLifecycleError> {
        let base = format!("v1/tasks/{uid}");
        let work = async {
            require_status(
                self.send(Method::PUT, &format!("{base}/inputs"), |request| {
                    request
                        .header(CONTENT_TYPE, "application/x-tar")
                        .body(input_archive)
                })
                .await?,
                StatusCode::NO_CONTENT,
            )?;
            require_status(
                self.send(Method::POST, &format!("{base}/execute"), |request| request)
                    .await?,
                StatusCode::ACCEPTED,
            )?;
            let mut delay = self.timing.initial_poll_delay;
            loop {
                let response = self.send(Method::GET, &base, |request| request).await?;
                if response.status() != StatusCode::OK {
                    return Err(classify_status(response.status()));
                }
                match read_task_reply(response).await?.phase.as_str() {
                    "succeeded" | "failed" | "cancelled" => return Ok(()),
                    "submitted" | "parked" | "queued" | "running" => {
                        sleep(delay).await;
                        delay = delay.saturating_mul(2).min(self.timing.max_poll_delay);
                    }
                    _ => return Err(TaskLifecycleError::Final),
                }
            }
        };
        match timeout(self.timing.deadline, work).await {
            Ok(result) => result.and(self.finalize(&base).await),
            Err(_) => {
                let cleanup = self.clone();
                tokio::spawn(async move {
                    if let Err(error) = cleanup.finalize(&base).await {
                        eprintln!("timed-out Task finalization ended with a {error:?} failure");
                    }
                });
                Err(TaskLifecycleError::Retriable)
            }
        }
    }

    async fn finalize(&self, base: &str) -> Result<(), TaskLifecycleError> {
        self.send(Method::DELETE, base, |request| request)
            .await
            .and_then(|response| require_status(response, StatusCode::ACCEPTED))
    }
}

impl TaskLifecycle for WorkloadExchangeClient {
    fn start<'a>(
        &'a self,
        trigger: &'a TriggerConfig,
        invocation: TaskInvocation,
    ) -> ReceiverFuture<'a, Result<TaskStart, TaskLifecycleError>> {
        Box::pin(async move {
            if !invocation.validate_for(trigger) {
                return Err(TaskLifecycleError::Final);
            }
            let response = self
                .send(Method::POST, "v1/tasks", |request| {
                    request
                        .header("Idempotency-Key", &invocation.idempotency_key)
                        .json(&TaskRequest {
                            workflow: &invocation.workflow,
                            envelope_digest: &invocation.envelope_digest,
                        })
                })
                .await?;
            let status = response.status();
            if !matches!(status, StatusCode::OK | StatusCode::ACCEPTED) {
                return Err(classify_status(status));
            }
            let reply = read_task_reply(response).await?;
            if status == StatusCode::OK {
                return Ok(TaskStart::Duplicate);
            }
            let this = self.clone();
            Ok(TaskStart::Started(Box::pin(async move {
                this.continue_task(reply.task_uid, invocation.input_archive)
                    .await
            })))
        })
    }
}

fn validate_task_origin(origin: &str, server_name: &str) -> Result<Url, String> {
    let url = Url::parse(origin).map_err(|_| "Steward API origin is invalid".to_owned())?;
    if url.scheme() != "https"
        || url.host_str() != Some(server_name)
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || server_name.trim().is_empty()
    {
        return Err("Steward API origin must be an exact verified HTTPS origin".to_owned());
    }
    Ok(url)
}

fn classify_status(status: StatusCode) -> TaskLifecycleError {
    if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
        TaskLifecycleError::Retriable
    } else {
        TaskLifecycleError::Final
    }
}

fn require_status(response: Response, expected: StatusCode) -> Result<(), TaskLifecycleError> {
    (response.status() == expected)
        .then_some(())
        .ok_or_else(|| classify_status(response.status()))
}

async fn read_task_reply(mut response: Response) -> Result<TaskReply, TaskLifecycleError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_TASK_REPLY_BYTES)
    {
        return Err(TaskLifecycleError::Final);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| TaskLifecycleError::Retriable)?
    {
        if body.len().saturating_add(chunk.len()) > MAX_TASK_REPLY_BYTES as usize {
            return Err(TaskLifecycleError::Final);
        }
        body.extend_from_slice(&chunk);
    }
    let reply: TaskReply = serde_json::from_slice(&body).map_err(|_| TaskLifecycleError::Final)?;
    Uuid::parse(reply.task_uid.clone()).map_err(|_| TaskLifecycleError::Final)?;
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axum::body::Body;
    use axum::http::{Request, Response as HttpResponse};
    use kube::client::Body as KubeBody;
    use steward_admission::{Envelope, EnvelopeSpec};
    use steward_types::{Budget, Duration as StewardDuration, RunnerRequirements};
    use tower::service_fn;

    use super::*;

    const OWNER: &str = "usr_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[tokio::test]
    async fn token_request_uses_supported_expiration_and_exact_audience() -> Result<(), String> {
        let captured = Arc::new(Mutex::new(None));
        let captured_request = captured.clone();
        let client = KubeClient::new(
            service_fn(move |request: Request<KubeBody>| {
                let captured = captured_request.clone();
                async move {
                    let body = request
                        .into_body()
                        .collect_bytes()
                        .await
                        .map_err(std::io::Error::other)?;
                    *captured
                        .lock()
                        .map_err(|_| std::io::Error::other("poisoned"))? = Some(body.to_vec());
                    let mut response = HttpResponse::new(Body::from(
                        br#"{"apiVersion":"authentication.k8s.io/v1","kind":"TokenRequest","metadata":{},"spec":{"audiences":["steward-task"],"expirationSeconds":600},"status":{"expirationTimestamp":"2030-01-01T00:00:00Z","token":"obviously-fake-source-token"}}"#
                            .to_vec(),
                    ));
                    response.headers_mut().insert(
                        "content-type",
                        "application/json".parse().map_err(std::io::Error::other)?,
                    );
                    Ok::<_, std::io::Error>(response)
                }
            }),
            "default",
        );
        let requester = KubernetesTokenRequester::new(
            client,
            "steward-test".to_owned(),
            "receiver".to_owned(),
            "steward-task".to_owned(),
        )?;
        assert_eq!(
            requester.request().await.map_err(|_| "request failed")?,
            "obviously-fake-source-token"
        );
        let body = captured
            .lock()
            .map_err(|_| "poisoned")?
            .clone()
            .ok_or("request missing")?;
        let request: serde_json::Value =
            serde_json::from_slice(&body).map_err(|error| error.to_string())?;
        assert_eq!(
            request["spec"]["audiences"],
            serde_json::json!(["steward-task"])
        );
        assert_eq!(request["spec"]["expirationSeconds"], 600);
        Ok(())
    }

    #[tokio::test]
    async fn envelope_lookup_fails_closed_when_database_is_unreachable() -> Result<(), String> {
        let store = PgStore::connect_lazy("postgres://steward@127.0.0.1:9/steward")
            .map_err(|error| error.to_string())?;
        let authority = PgEnvelopeAuthority::new(store);
        let owner = CanonicalUserId::parse(OWNER)?;
        let digest = format!("steward:sha256:{}", "a".repeat(64));
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            crate::EnvelopeAuthority::active_tools(&authority, &owner, &digest),
        )
        .await;
        assert!(!matches!(result, Ok(Ok(_))));
        Ok(())
    }

    #[test]
    fn envelope_lookup_requires_one_exact_active_record() -> Result<(), String> {
        let owner = CanonicalUserId::parse(OWNER)?;
        let digest = format!("sha256:{}", "a".repeat(64));
        assert_eq!(
            active_tools_from_rows(&owner, &digest, Vec::new()),
            Err(crate::EnvelopeError::Absent)
        );
        let valid = envelope_record(owner.clone(), digest.clone());
        assert_eq!(
            active_tools_from_rows(&owner, &digest, vec![valid.clone(), valid.clone()]),
            Err(crate::EnvelopeError::Ambiguous)
        );
        assert!(active_tools_from_rows(&owner, &digest, vec![valid.clone()]).is_ok());
        let mut wrong_owner = valid.clone();
        wrong_owner.owner_user_id = CanonicalUserId::parse("usr_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")?;
        assert_eq!(
            active_tools_from_rows(&owner, &digest, vec![wrong_owner]),
            Err(crate::EnvelopeError::Denied)
        );
        for stale in [
            {
                let mut row = valid.clone();
                row.envelope_instance_id = None;
                row
            },
            {
                let mut row = valid.clone();
                row.envelope_digest = Some(format!("sha256:{}", "b".repeat(64)));
                row
            },
            {
                let mut row = valid;
                row.approved_envelope = None;
                row
            },
        ] {
            assert_eq!(
                active_tools_from_rows(&owner, &digest, vec![stale]),
                Err(crate::EnvelopeError::Stale)
            );
        }
        Ok(())
    }

    fn envelope_record(owner_user_id: CanonicalUserId, digest: String) -> EnvelopeRequestRecord {
        let envelope = Envelope {
            revision: 1,
            spec: EnvelopeSpec {
                llms: Vec::new(),
                tools: Vec::new(),
                budget: Budget {
                    monthly_limit: "10.00".to_owned(),
                    single_run_limit: Some("1.00".to_owned()),
                    currency: "USD".to_owned(),
                },
                runtime_minutes_limit: None,
                workspace: None,
                ttl: StewardDuration("15m".to_owned()),
                runner: RunnerRequirements::default(),
            },
        };
        EnvelopeRequestRecord {
            id: uuid::Uuid::nil(),
            owner_user_id,
            template_id: None,
            template_revision: None,
            requested_envelope: envelope.clone(),
            template_envelope: None,
            justification: None,
            ceiling_exceedances: Vec::new(),
            approved_envelope: Some(envelope),
            status: EnvelopeRequestStatus::Provisioned,
            approval_id: None,
            envelope_instance_id: Some("env_test".to_owned()),
            envelope_digest: Some(digest),
            reason: None,
            rationale: None,
            evidence_url: None,
            decision_key: None,
            expires_at: None,
            status_actor: OWNER.to_owned(),
            status_template_revision: None,
            created_at: "2026-10-16T00:00:00Z".to_owned(),
            status_at: "2026-10-16T00:00:00Z".to_owned(),
        }
    }

    #[test]
    fn task_origin_refuses_http_credentials_and_server_name_mismatch() {
        for (origin, server_name) in [
            ("http://api.example.test/", "api.example.test"),
            ("https://alice@api.example.test/", "api.example.test"),
            ("https://api.example.test/", "other.example.test"),
            ("https://api.example.test/v1", "api.example.test"),
        ] {
            assert!(validate_task_origin(origin, server_name).is_err());
        }
    }

    #[tokio::test]
    async fn file_catalog_is_read_again_for_each_submit_check() -> Result<(), String> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
            "../../target/trigger-catalog-{}",
            std::process::id()
        ));
        std::fs::write(
            &path,
            r#"{"schemaVersion":"steward.capability-catalog/v2","models":[],"tools":[],"catalogs":[]}"#,
        )
        .map_err(|error| error.to_string())?;
        let reader = FileCatalog::new(path.clone())?;
        assert!(
            crate::CapabilityCatalogReader::current(&reader)
                .await
                .is_ok()
        );
        std::fs::write(&path, "{}").map_err(|error| error.to_string())?;
        assert!(
            crate::CapabilityCatalogReader::current(&reader)
                .await
                .is_err()
        );
        std::fs::remove_file(path).map_err(|error| error.to_string())?;
        Ok(())
    }

    #[derive(Clone)]
    struct Tokens(Arc<Mutex<usize>>);

    impl TaskTokenProvider for Tokens {
        fn token(&self) -> ReceiverFuture<'_, Result<OpaqueToken, TaskLifecycleError>> {
            Box::pin(async move {
                let mut count = self.0.lock().map_err(|_| TaskLifecycleError::Retriable)?;
                *count += 1;
                OpaqueToken::new(format!("obviously-fake-task-token-{count}"))
                    .map_err(exchange_error)
            })
        }
    }

    #[derive(Clone)]
    struct ApiState {
        create_status: StatusCode,
        polls_before_done: usize,
        failure: Option<&'static str>,
        requests: Arc<Mutex<Vec<(Method, String, String)>>>,
        polls: Arc<Mutex<usize>>,
    }

    async fn api_fixture(
        axum::extract::State(state): axum::extract::State<ApiState>,
        request: Request<Body>,
    ) -> HttpResponse<Body> {
        let method = request.method().clone();
        let path = request.uri().path().to_owned();
        let auth = request
            .headers()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let Ok(mut requests) = state.requests.lock() else {
            return HttpResponse::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::empty())
                .unwrap_or_else(|_| HttpResponse::new(Body::empty()));
        };
        requests.push((method.clone(), path.clone(), auth));
        drop(requests);
        let mut response = if method == Method::POST && path == "/v1/tasks" {
            HttpResponse::new(Body::from(
                r#"{"taskUid":"00000000-0000-4000-8000-000000000001","phase":"submitted"}"#,
            ))
        } else if method == Method::GET {
            let Ok(mut polls) = state.polls.lock() else {
                return HttpResponse::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::empty())
                    .unwrap_or_else(|_| HttpResponse::new(Body::empty()));
            };
            *polls += 1;
            let phase = if *polls > state.polls_before_done {
                "succeeded"
            } else {
                "running"
            };
            HttpResponse::new(Body::from(format!(
                r#"{{"taskUid":"00000000-0000-4000-8000-000000000001","phase":"{phase}"}}"#
            )))
        } else {
            HttpResponse::new(Body::empty())
        };
        let injected_failure = match state.failure {
            Some("upload") => method == Method::PUT,
            Some("execute") => method == Method::POST && path.ends_with("/execute"),
            Some("poll") => method == Method::GET,
            Some("finalize") => method == Method::DELETE,
            _ => false,
        };
        *response.status_mut() = if method == Method::POST && path == "/v1/tasks" {
            state.create_status
        } else if injected_failure {
            StatusCode::SERVICE_UNAVAILABLE
        } else if method == Method::PUT {
            StatusCode::NO_CONTENT
        } else if method == Method::GET {
            StatusCode::OK
        } else {
            StatusCode::ACCEPTED
        };
        response
    }

    async fn client_fixture(
        create_status: StatusCode,
        polls_before_done: usize,
        deadline: Duration,
        failure: Option<&'static str>,
    ) -> Result<
        (
            WorkloadExchangeClient,
            ApiState,
            tokio::task::JoinHandle<()>,
        ),
        String,
    > {
        let state = ApiState {
            create_status,
            polls_before_done,
            failure,
            requests: Arc::new(Mutex::new(Vec::new())),
            polls: Arc::new(Mutex::new(0)),
        };
        let app = axum::Router::new()
            .fallback(axum::routing::any(api_fixture))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|error| error.to_string())?;
        let origin = Url::parse(&format!(
            "http://{}/",
            listener.local_addr().map_err(|error| error.to_string())?
        ))
        .map_err(|error| error.to_string())?;
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let client = WorkloadExchangeClient {
            http: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|error| error.to_string())?,
            task_origin: origin,
            tokens: Arc::new(Tokens(Arc::new(Mutex::new(0)))),
            timing: TaskClientTiming {
                deadline,
                initial_poll_delay: Duration::from_millis(5),
                max_poll_delay: Duration::from_millis(10),
            },
        };
        Ok((client, state, handle))
    }

    fn invocation() -> Result<TaskInvocation, String> {
        Ok(TaskInvocation {
            workflow: "triage@1".to_owned(),
            envelope_digest: format!("steward:sha256:{}", "a".repeat(64)),
            idempotency_key: format!("jira-sha256:{}", "b".repeat(64)),
            input_archive: crate::JiraEvent::parse(
                br#"{"webhookEvent":"jira:issue_updated","issue":{"key":"PROJ-123","fields":{"project":{"key":"PROJ"},"issuetype":{"id":"10001"}}},"user":{"accountId":"alice"}}"#,
                128,
            )
            .map_err(|_| "event fixture is invalid")?
            .archive()
            .map_err(|_| "archive fixture is invalid")?,
        })
    }

    fn trigger() -> Result<TriggerConfig, String> {
        serde_json::from_value(serde_json::json!({
            "id": "jira-read", "enabled": true, "source": "jira", "workflow": "triage@1",
            "service": "event-worker", "ownerUserId": OWNER,
            "envelopeDigest": format!("steward:sha256:{}", "a".repeat(64)),
            "signingSecretFile": "/run/secrets/jira", "concurrencyLimit": 1,
            "cooldownSeconds": 1
        }))
        .map_err(|error| error.to_string())
    }

    #[tokio::test]
    async fn duplicate_create_is_a_noop() -> Result<(), String> {
        let (client, state, handle) =
            client_fixture(StatusCode::OK, 0, Duration::from_secs(1), None).await?;
        let result = client
            .start(&trigger()?, invocation()?)
            .await
            .map_err(|error| format!("{error:?}"))?;
        assert!(matches!(result, TaskStart::Duplicate));
        assert_eq!(state.requests.lock().map_err(|_| "poisoned")?.len(), 1);
        handle.abort();
        Ok(())
    }

    #[tokio::test]
    async fn accepted_task_polls_with_fresh_tokens_and_finalizes() -> Result<(), String> {
        let (client, state, handle) =
            client_fixture(StatusCode::ACCEPTED, 1, Duration::from_secs(1), None).await?;
        let TaskStart::Started(completion) = client
            .start(&trigger()?, invocation()?)
            .await
            .map_err(|error| format!("{error:?}"))?
        else {
            return Err("expected started".to_owned());
        };
        completion.await.map_err(|error| format!("{error:?}"))?;
        let requests = state.requests.lock().map_err(|_| "poisoned")?;
        assert_eq!(
            requests
                .iter()
                .filter(|(method, _, _)| *method == Method::GET)
                .count(),
            2
        );
        assert!(
            requests
                .iter()
                .any(|(method, _, _)| *method == Method::DELETE)
        );
        let auth = requests
            .iter()
            .map(|(_, _, auth)| auth)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(auth.len(), requests.len());
        handle.abort();
        Ok(())
    }

    #[tokio::test]
    async fn poll_deadline_still_finalizes() -> Result<(), String> {
        let (client, state, handle) = client_fixture(
            StatusCode::ACCEPTED,
            usize::MAX,
            Duration::from_millis(20),
            None,
        )
        .await?;
        let TaskStart::Started(completion) = client
            .start(&trigger()?, invocation()?)
            .await
            .map_err(|error| format!("{error:?}"))?
        else {
            return Err("expected started".to_owned());
        };
        assert_eq!(completion.await, Err(TaskLifecycleError::Retriable));
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if state
                    .requests
                    .lock()
                    .map(|requests| {
                        requests
                            .iter()
                            .any(|(method, _, _)| *method == Method::DELETE)
                    })
                    .unwrap_or(false)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .map_err(|_| "finalize was not attempted")?;
        handle.abort();
        Ok(())
    }

    #[tokio::test]
    async fn create_failures_are_classified_before_started() -> Result<(), String> {
        for (status, expected) in [
            (
                StatusCode::SERVICE_UNAVAILABLE,
                TaskLifecycleError::Retriable,
            ),
            (StatusCode::FORBIDDEN, TaskLifecycleError::Final),
        ] {
            let (client, _, handle) =
                client_fixture(status, 0, Duration::from_secs(1), None).await?;
            assert_eq!(
                client.start(&trigger()?, invocation()?).await.err(),
                Some(expected)
            );
            handle.abort();
        }
        Ok(())
    }

    #[tokio::test]
    async fn every_post_create_failure_is_returned_and_finalize_is_attempted() -> Result<(), String>
    {
        for failure in ["upload", "execute", "poll", "finalize"] {
            let (client, state, handle) = client_fixture(
                StatusCode::ACCEPTED,
                0,
                Duration::from_secs(1),
                Some(failure),
            )
            .await?;
            let TaskStart::Started(completion) = client
                .start(&trigger()?, invocation()?)
                .await
                .map_err(|error| format!("{error:?}"))?
            else {
                return Err("expected started".to_owned());
            };
            assert_eq!(completion.await, Err(TaskLifecycleError::Retriable));
            assert!(
                state
                    .requests
                    .lock()
                    .map_err(|_| "poisoned")?
                    .iter()
                    .any(|(method, _, _)| *method == Method::DELETE),
                "{failure} must still attempt finalization"
            );
            handle.abort();
        }
        Ok(())
    }
}
