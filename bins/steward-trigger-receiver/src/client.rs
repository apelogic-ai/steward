use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use k8s_openapi::api::authentication::v1::{TokenRequest, TokenRequestSpec};
use k8s_openapi::api::core::v1::ServiceAccount;
use kube::Client as KubeClient;
use kube::api::{Api, PostParams};
use reqwest::header::CONTENT_TYPE;
use reqwest::{Certificate, Client, Method, Response, StatusCode, Url};
use serde::{Deserialize, Serialize};
use steward_adapter_openshell::{
    OpaqueToken, WorkloadExchangeTokenProvider, WorkloadExchangeTransport,
    WorkloadSourceTokenFuture, WorkloadSourceTokenProvider,
};
use steward_apiserver::browser_admin::CapabilityCatalog;
use steward_store::{EnvelopeRequestRecord, EnvelopeRequestStatus, PgStore, StoreError};
use steward_types::CanonicalUserId;
use steward_types::direct_package::{EnvelopeDigest, Uuid};
use tokio::sync::Mutex;
use tokio::time::{sleep, timeout};

use crate::{
    ReceiverFuture, TaskInvocation, TaskLifecycle, TaskLifecycleError, TaskStart, TriggerConfig,
};

pub const MIN_SOURCE_TOKEN_SECONDS: u64 = 600;
pub const MAX_SOURCE_TOKEN_SECONDS: u64 = 86_400;
const SOURCE_TOKEN_REFRESH_DIVISOR: u32 = 10;

#[derive(Clone)]
pub struct PgEnvelopeAuthority {
    store: Arc<dyn EnvelopeStore>,
}

impl PgEnvelopeAuthority {
    pub fn new(store: PgStore) -> Self {
        Self {
            store: Arc::new(store),
        }
    }
}

trait EnvelopeStore: Send + Sync {
    fn active_provisioned_user_envelopes_by_digest<'a>(
        &'a self,
        owner: &'a CanonicalUserId,
        digest: &'a str,
    ) -> ReceiverFuture<'a, Result<Vec<EnvelopeRequestRecord>, StoreError>>;
}

impl EnvelopeStore for PgStore {
    fn active_provisioned_user_envelopes_by_digest<'a>(
        &'a self,
        owner: &'a CanonicalUserId,
        digest: &'a str,
    ) -> ReceiverFuture<'a, Result<Vec<EnvelopeRequestRecord>, StoreError>> {
        Box::pin(async move {
            PgStore::active_provisioned_user_envelopes_by_digest(self, owner, digest).await
        })
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
                .await;
            active_tools_from_store_result(owner, store_digest, rows)
        })
    }
}

fn active_tools_from_store_result(
    owner: &CanonicalUserId,
    digest: &str,
    rows: Result<Vec<EnvelopeRequestRecord>, StoreError>,
) -> Result<Vec<steward_types::ToolGrant>, crate::EnvelopeError> {
    active_tools_from_rows(
        owner,
        digest,
        rows.map_err(|_| crate::EnvelopeError::Unavailable)?,
    )
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
    expiration_seconds: u64,
    cache: Arc<Mutex<Option<CachedSourceToken>>>,
}

#[derive(Clone)]
struct CachedSourceToken {
    token: OpaqueToken,
    refresh_at: SystemTime,
}

impl KubernetesTokenRequester {
    pub fn new(
        client: KubeClient,
        namespace: String,
        service_account: String,
        audience: String,
        expiration_seconds: u64,
    ) -> Result<Self, String> {
        if [
            namespace.as_str(),
            service_account.as_str(),
            audience.as_str(),
        ]
        .iter()
        .any(|value| value.trim().is_empty() || value.trim() != *value)
            || !(MIN_SOURCE_TOKEN_SECONDS..=MAX_SOURCE_TOKEN_SECONDS).contains(&expiration_seconds)
        {
            return Err("TokenRequest settings are incomplete".to_owned());
        }
        Ok(Self {
            client,
            namespace,
            service_account,
            audience,
            expiration_seconds,
            cache: Arc::new(Mutex::new(None)),
        })
    }

    async fn request(&self) -> Result<CachedSourceToken, TaskLifecycleError> {
        let expiration_seconds =
            i64::try_from(self.expiration_seconds).map_err(|_| TaskLifecycleError::Final)?;
        let api: Api<ServiceAccount> = Api::namespaced(self.client.clone(), &self.namespace);
        let request = TokenRequest {
            metadata: Default::default(),
            spec: TokenRequestSpec {
                audiences: vec![self.audience.clone()],
                expiration_seconds: Some(expiration_seconds),
                bound_object_ref: None,
            },
            status: None,
        };
        let response = api
            .create_token_request(&self.service_account, &PostParams::default(), &request)
            .await
            .map_err(|error| match error {
                kube::Error::Api(response) if !transient_status_code(response.code) => {
                    TaskLifecycleError::Final
                }
                _ => TaskLifecycleError::Retriable,
            })?;
        let status = response.status.ok_or(TaskLifecycleError::Final)?;
        let token = OpaqueToken::new(status.token).map_err(|_| TaskLifecycleError::Final)?;
        let expires_at: SystemTime = status.expiration_timestamp.0.into();
        let refresh_at = source_token_refresh_at(expires_at, SystemTime::now())?;
        Ok(CachedSourceToken { token, refresh_at })
    }

    async fn invalidate(&self) {
        *self.cache.lock().await = None;
    }
}

fn source_token_refresh_at(
    expires_at: SystemTime,
    now: SystemTime,
) -> Result<SystemTime, TaskLifecycleError> {
    let remaining = expires_at
        .duration_since(now)
        .map_err(|_| TaskLifecycleError::Retriable)?;
    let refresh_margin =
        remaining.max(Duration::from_secs(MIN_SOURCE_TOKEN_SECONDS)) / SOURCE_TOKEN_REFRESH_DIVISOR;
    expires_at
        .checked_sub(refresh_margin)
        .filter(|refresh_at| *refresh_at > now)
        .ok_or(TaskLifecycleError::Retriable)
}

fn transient_status_code(code: u16) -> bool {
    code >= 500
        || [
            StatusCode::REQUEST_TIMEOUT.as_u16(),
            StatusCode::TOO_EARLY.as_u16(),
            StatusCode::TOO_MANY_REQUESTS.as_u16(),
        ]
        .contains(&code)
}

impl WorkloadSourceTokenProvider for KubernetesTokenRequester {
    fn source_token(&self) -> WorkloadSourceTokenFuture<'_> {
        Box::pin(async move {
            let mut cache = self.cache.lock().await;
            if let Some(cached) = cache
                .as_ref()
                .filter(|cached| SystemTime::now() < cached.refresh_at)
            {
                return Ok(cached.token.clone());
            }
            let source = self.request().await.map_err(|error| match error {
                TaskLifecycleError::Final => steward_ports::PortError::Rejected {
                    reason: "workload source credential was refused".to_owned(),
                },
                TaskLifecycleError::Retriable => steward_ports::PortError::Failed {
                    reason: "workload source credential is unavailable".to_owned(),
                },
            })?;
            let token = source.token.clone();
            *cache = Some(source);
            Ok(token)
        })
    }
}

pub trait TaskTokenProvider: Send + Sync + 'static {
    fn lifecycle(
        &self,
    ) -> ReceiverFuture<'_, Result<Arc<dyn TaskAccessTokenProvider>, TaskLifecycleError>>;
}

pub trait TaskAccessTokenProvider: Send + Sync + 'static {
    fn token(&self) -> ReceiverFuture<'_, Result<OpaqueToken, TaskLifecycleError>>;
}

#[derive(Clone)]
pub struct KubernetesWorkloadTokenProvider {
    source: KubernetesTokenRequester,
    endpoint: String,
    server_name: String,
    ca_certificate_pem: Arc<Vec<u8>>,
    transport: WorkloadExchangeTransport,
}

impl KubernetesWorkloadTokenProvider {
    pub fn new(
        source: KubernetesTokenRequester,
        endpoint: String,
        server_name: String,
        ca_certificate_pem: Vec<u8>,
        transport: WorkloadExchangeTransport,
    ) -> Result<Self, String> {
        let transport = transport
            .validate()
            .map_err(|_| "workload exchange transport settings are invalid".to_owned())?;
        steward_adapter_openshell::validate_workload_exchange_endpoint(&endpoint, &server_name)
            .map_err(|_| "workload exchange endpoint is invalid".to_owned())?;
        let certificates = Certificate::from_pem_bundle(&ca_certificate_pem)
            .map_err(|_| "workload exchange CA certificate is invalid".to_owned())?;
        if certificates.is_empty() {
            return Err("workload exchange CA certificate is empty".to_owned());
        }
        Ok(Self {
            source,
            endpoint,
            server_name,
            ca_certificate_pem: Arc::new(ca_certificate_pem),
            transport,
        })
    }
}

impl TaskTokenProvider for KubernetesWorkloadTokenProvider {
    fn lifecycle(
        &self,
    ) -> ReceiverFuture<'_, Result<Arc<dyn TaskAccessTokenProvider>, TaskLifecycleError>> {
        Box::pin(async move {
            let provider = WorkloadExchangeTokenProvider::new_with_source_provider(
                &self.endpoint,
                &self.server_name,
                &self.ca_certificate_pem,
                self.transport,
                Arc::new(self.source.clone()),
            )
            .map_err(exchange_error)?;
            Ok(Arc::new(KubernetesAccessTokenProvider {
                exchange: provider,
                source: self.source.clone(),
            }) as Arc<dyn TaskAccessTokenProvider>)
        })
    }
}

struct KubernetesAccessTokenProvider {
    exchange: WorkloadExchangeTokenProvider,
    source: KubernetesTokenRequester,
}

impl TaskAccessTokenProvider for KubernetesAccessTokenProvider {
    fn token(&self) -> ReceiverFuture<'_, Result<OpaqueToken, TaskLifecycleError>> {
        Box::pin(async move {
            let result = self.exchange.access_token().await;
            if matches!(result, Err(steward_ports::PortError::Rejected { .. })) {
                self.source.invalidate().await;
            }
            result.map_err(exchange_error)
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
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub max_reply_bytes: usize,
}

impl TaskClientTiming {
    pub fn validate(self) -> Result<Self, String> {
        if self.deadline.is_zero()
            || self.initial_poll_delay.is_zero()
            || self.max_poll_delay < self.initial_poll_delay
            || self.connect_timeout.is_zero()
            || self.request_timeout < self.connect_timeout
            || self.max_reply_bytes == 0
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
    #[serde(rename = "callerReference")]
    #[serde(skip_serializing_if = "Option::is_none")]
    caller_reference: Option<&'a steward_types::CallerReference>,
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
            .connect_timeout(timing.connect_timeout)
            .timeout(timing.request_timeout);
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
        tokens: &dyn TaskAccessTokenProvider,
        method: Method,
        path: &str,
        configure: impl FnOnce(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
    ) -> Result<Response, TaskLifecycleError> {
        let token = tokens.token().await?;
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
        tokens: Arc<dyn TaskAccessTokenProvider>,
    ) -> Result<(), TaskLifecycleError> {
        let base = format!("v1/tasks/{uid}");
        let work = async {
            let mut delay = self.timing.initial_poll_delay;
            loop {
                let reply = match self
                    .send(tokens.as_ref(), Method::GET, &base, |request| request)
                    .await
                {
                    Ok(response) if response.status() == StatusCode::OK => {
                        read_task_reply(response, self.timing.max_reply_bytes).await
                    }
                    Ok(response) => Err(classify_status(response.status())),
                    Err(error) => Err(error),
                };
                match reply {
                    Err(TaskLifecycleError::Retriable) => {
                        sleep(delay).await;
                        delay = delay.saturating_mul(2).min(self.timing.max_poll_delay);
                        continue;
                    }
                    Err(TaskLifecycleError::Final) => return Err(TaskLifecycleError::Final),
                    Ok(reply) => match reply.phase.as_str() {
                        "succeeded" | "failed" | "cancelled" => return Ok(()),
                        "submitted" | "parked" | "queued" | "running" => {
                            sleep(delay).await;
                            delay = delay.saturating_mul(2).min(self.timing.max_poll_delay);
                        }
                        _ => return Err(TaskLifecycleError::Final),
                    },
                }
            }
        };
        match timeout(self.timing.deadline, work).await {
            Ok(result) => {
                let finalized = self.finalize(&base, tokens.as_ref()).await;
                result.and(finalized)
            }
            Err(_) => {
                let cleanup = self.clone();
                tokio::spawn(async move {
                    if let Err(error) = cleanup.finalize(&base, tokens.as_ref()).await {
                        eprintln!("timed-out Task finalization ended with a {error:?} failure");
                    }
                });
                Err(TaskLifecycleError::Retriable)
            }
        }
    }

    async fn finalize(
        &self,
        base: &str,
        tokens: &dyn TaskAccessTokenProvider,
    ) -> Result<(), TaskLifecycleError> {
        self.send(tokens, Method::DELETE, base, |request| request)
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
            let tokens = self.tokens.lifecycle().await?;
            let response = self
                .send(tokens.as_ref(), Method::POST, "v1/tasks", |request| {
                    request
                        .header("Idempotency-Key", &invocation.idempotency_key)
                        .json(&TaskRequest {
                            workflow: &invocation.workflow,
                            envelope_digest: &invocation.envelope_digest,
                            caller_reference: invocation.caller_reference.as_ref(),
                        })
                })
                .await?;
            let status = response.status();
            if !matches!(status, StatusCode::OK | StatusCode::ACCEPTED) {
                return Err(classify_status(status));
            }
            let reply = read_task_reply(response, self.timing.max_reply_bytes).await?;
            if status == StatusCode::OK {
                return Ok(TaskStart::Duplicate);
            }
            let base = format!("v1/tasks/{}", reply.task_uid);
            match reply.phase.as_str() {
                "succeeded" | "failed" | "cancelled" => {
                    self.finalize(&base, tokens.as_ref()).await?;
                    return Ok(TaskStart::Duplicate);
                }
                "submitted" | "parked" => {
                    let prepared = async {
                        require_status(
                            self.send(
                                tokens.as_ref(),
                                Method::PUT,
                                &format!("{base}/inputs"),
                                |request| {
                                    request
                                        .header(CONTENT_TYPE, "application/x-tar")
                                        .body(invocation.input_archive)
                                },
                            )
                            .await?,
                            StatusCode::NO_CONTENT,
                        )?;
                        require_status(
                            self.send(
                                tokens.as_ref(),
                                Method::POST,
                                &format!("{base}/execute"),
                                |request| request,
                            )
                            .await?,
                            StatusCode::ACCEPTED,
                        )
                    }
                    .await;
                    if let Err(error) = prepared {
                        if error == TaskLifecycleError::Final {
                            let _ = self.finalize(&base, tokens.as_ref()).await;
                        }
                        return Err(error);
                    }
                }
                "queued" | "running" => {}
                _ => {
                    let _ = self.finalize(&base, tokens.as_ref()).await;
                    return Err(TaskLifecycleError::Final);
                }
            }
            let this = self.clone();
            Ok(TaskStart::Started(Box::pin(async move {
                this.continue_task(reply.task_uid, tokens).await
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
    if status.is_server_error()
        || matches!(
            status,
            StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS
        )
    {
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

async fn read_task_reply(
    mut response: Response,
    max_reply_bytes: usize,
) -> Result<TaskReply, TaskLifecycleError> {
    if response
        .content_length()
        .is_some_and(|length| length > max_reply_bytes as u64)
    {
        return Err(TaskLifecycleError::Final);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| TaskLifecycleError::Retriable)?
    {
        if body.len().saturating_add(chunk.len()) > max_reply_bytes {
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
    use std::sync::atomic::{AtomicU64, Ordering};

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, Response as HttpResponse};
    use kube::client::Body as KubeBody;
    use rcgen::generate_simple_self_signed;
    use steward_admission::{Envelope, EnvelopeSpec};
    use steward_types::{Budget, Duration as StewardDuration, RunnerRequirements};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_rustls::TlsAcceptor;
    use tokio_rustls::rustls::ServerConfig;
    use tokio_rustls::rustls::pki_types::PrivatePkcs8KeyDer;
    use tower::service_fn;

    use super::*;

    const OWNER: &str = "usr_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    struct TlsFixture {
        origin: String,
        ca_certificate_pem: Vec<u8>,
        requests: Arc<Mutex<Vec<String>>>,
        handle: tokio::task::JoinHandle<()>,
    }

    async fn tls_fixture(responses: Vec<String>) -> Result<TlsFixture, String> {
        let expected_requests = responses.len();
        let mut responses = responses.into_iter();
        tls_fixture_with_responder(expected_requests, move |_| {
            responses.next().unwrap_or_default()
        })
        .await
    }

    async fn tls_fixture_with_responder(
        expected_requests: usize,
        mut respond: impl FnMut(&str) -> String + Send + 'static,
    ) -> Result<TlsFixture, String> {
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
        let certified = generate_simple_self_signed(vec!["localhost".to_owned()])
            .map_err(|error| error.to_string())?;
        let ca_certificate_pem = certified.cert.pem().into_bytes();
        let key = PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der());
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certified.cert.der().clone()], key.into())
            .map_err(|error| error.to_string())?;
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|error| error.to_string())?;
        let address = listener.local_addr().map_err(|error| error.to_string())?;
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let handle = tokio::spawn(async move {
            for _ in 0..expected_requests {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let Ok(mut stream) = acceptor.accept(stream).await else {
                    continue;
                };
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4096];
                loop {
                    let Ok(read) = stream.read(&mut buffer).await else {
                        return;
                    };
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let Ok(request) = String::from_utf8(request) else {
                    return;
                };
                let response = respond(&request);
                if let Ok(mut captured) = captured.lock() {
                    captured.push(request);
                }
                if stream.write_all(response.as_bytes()).await.is_err() {
                    return;
                }
            }
        });
        Ok(TlsFixture {
            origin: format!("https://localhost:{}/", address.port()),
            ca_certificate_pem,
            requests,
            handle,
        })
    }

    fn test_response(status: u16, headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn exchange_transport() -> WorkloadExchangeTransport {
        WorkloadExchangeTransport {
            connect_timeout: Duration::from_millis(50),
            request_timeout: Duration::from_millis(250),
            max_response_bytes: 4091,
        }
    }

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
            600,
        )?;
        assert_eq!(
            requester
                .request()
                .await
                .map_err(|_| "request failed")?
                .token
                .expose_secret(),
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
    async fn token_request_maps_rate_limits_and_server_failures_to_retry() -> Result<(), String> {
        for (status, expected) in [
            (StatusCode::FORBIDDEN, TaskLifecycleError::Final),
            (StatusCode::REQUEST_TIMEOUT, TaskLifecycleError::Retriable),
            (StatusCode::TOO_MANY_REQUESTS, TaskLifecycleError::Retriable),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                TaskLifecycleError::Retriable,
            ),
        ] {
            let client = KubeClient::new(
                service_fn(move |_request: Request<KubeBody>| async move {
                    let mut response = HttpResponse::new(Body::from(format!(
                        r#"{{"apiVersion":"v1","kind":"Status","status":"Failure","message":"request refused","reason":"Failure","code":{}}}"#,
                        status.as_u16()
                    )));
                    *response.status_mut() = status;
                    response.headers_mut().insert(
                        "content-type",
                        "application/json".parse().map_err(std::io::Error::other)?,
                    );
                    Ok::<_, std::io::Error>(response)
                }),
                "default",
            );
            let requester = KubernetesTokenRequester::new(
                client,
                "steward-test".to_owned(),
                "receiver".to_owned(),
                "steward-task".to_owned(),
                600,
            )?;
            assert_eq!(requester.request().await.err(), Some(expected));
        }
        Ok(())
    }

    #[tokio::test]
    async fn workload_provider_reuses_the_source_token_during_access_token_refresh()
    -> Result<(), String> {
        let source_calls = Arc::new(Mutex::new(0_usize));
        let counted = source_calls.clone();
        let kube = KubeClient::new(
            service_fn(move |_request: Request<KubeBody>| {
                let counted = counted.clone();
                async move {
                    let call = {
                        let mut count = counted
                            .lock()
                            .map_err(|_| std::io::Error::other("poisoned"))?;
                        *count += 1;
                        *count
                    };
                    let mut response = HttpResponse::new(Body::from(format!(
                        r#"{{"apiVersion":"authentication.k8s.io/v1","kind":"TokenRequest","metadata":{{}},"spec":{{"audiences":["steward-task"],"expirationSeconds":600}},"status":{{"expirationTimestamp":"2030-01-01T00:00:00Z","token":"obviously-fake-source-token-{call}"}}}}"#
                    )));
                    response.headers_mut().insert(
                        "content-type",
                        "application/json".parse().map_err(std::io::Error::other)?,
                    );
                    Ok::<_, std::io::Error>(response)
                }
            }),
            "default",
        );
        let first_exchange = r#"{"access_token":"obviously-fake-first-access","token_type":"Bearer","expires_in":1}"#;
        let second_exchange = r#"{"access_token":"obviously-fake-second-access","token_type":"Bearer","expires_in":120}"#;
        let exchange = tls_fixture(vec![
            test_response(200, "Cache-Control: no-store\r\n", first_exchange),
            test_response(200, "Cache-Control: no-store\r\n", second_exchange),
        ])
        .await?;
        let provider = KubernetesWorkloadTokenProvider::new(
            KubernetesTokenRequester::new(
                kube,
                "steward-test".to_owned(),
                "receiver".to_owned(),
                "steward-task".to_owned(),
                600,
            )?,
            format!("{}v1/workload/exchange", exchange.origin),
            "localhost".to_owned(),
            exchange.ca_certificate_pem.clone(),
            exchange_transport(),
        )?;
        let lifecycle = provider
            .lifecycle()
            .await
            .map_err(|error| format!("{error:?}"))?;
        assert_eq!(
            lifecycle
                .token()
                .await
                .map_err(|error| format!("{error:?}"))?
                .expose_secret(),
            "obviously-fake-first-access"
        );
        assert_eq!(
            lifecycle
                .token()
                .await
                .map_err(|error| format!("{error:?}"))?
                .expose_secret(),
            "obviously-fake-second-access"
        );
        exchange.handle.await.map_err(|error| error.to_string())?;
        assert_eq!(*source_calls.lock().map_err(|_| "poisoned")?, 1);
        let requests = exchange.requests.lock().map_err(|_| "poisoned")?;
        assert!(requests[0].contains("Bearer obviously-fake-source-token-1"));
        assert!(requests[1].contains("Bearer obviously-fake-source-token-1"));
        Ok(())
    }

    #[tokio::test]
    async fn source_token_refreshes_after_its_refresh_point() -> Result<(), String> {
        let source_calls = Arc::new(Mutex::new(0_usize));
        let counted = source_calls.clone();
        let kube = KubeClient::new(
            service_fn(move |_request: Request<KubeBody>| {
                let counted = counted.clone();
                async move {
                    let call = {
                        let mut count = counted
                            .lock()
                            .map_err(|_| std::io::Error::other("poisoned"))?;
                        *count += 1;
                        *count
                    };
                    let mut response = HttpResponse::new(Body::from(format!(
                        r#"{{"apiVersion":"authentication.k8s.io/v1","kind":"TokenRequest","metadata":{{}},"spec":{{"audiences":["steward-task"],"expirationSeconds":600}},"status":{{"expirationTimestamp":"2030-01-01T00:00:00Z","token":"obviously-fake-source-token-{call}"}}}}"#
                    )));
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
            kube,
            "steward-test".to_owned(),
            "receiver".to_owned(),
            "steward-task".to_owned(),
            600,
        )?;
        assert_eq!(
            requester
                .source_token()
                .await
                .map_err(|error| format!("{error:?}"))?
                .expose_secret(),
            "obviously-fake-source-token-1"
        );
        requester
            .cache
            .lock()
            .await
            .as_mut()
            .ok_or("source token was not cached")?
            .refresh_at = SystemTime::UNIX_EPOCH;
        assert_eq!(
            requester
                .source_token()
                .await
                .map_err(|error| format!("{error:?}"))?
                .expose_secret(),
            "obviously-fake-source-token-2"
        );
        assert_eq!(*source_calls.lock().map_err(|_| "poisoned")?, 2);
        Ok(())
    }

    #[test]
    fn source_token_clock_skew_is_retriable() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        for expires_at in [
            now - Duration::from_secs(1),
            now + Duration::from_secs(
                MIN_SOURCE_TOKEN_SECONDS / u64::from(SOURCE_TOKEN_REFRESH_DIVISOR) - 1,
            ),
        ] {
            assert_eq!(
                source_token_refresh_at(expires_at, now),
                Err(TaskLifecycleError::Retriable)
            );
        }
    }

    #[tokio::test]
    async fn workload_exchange_refuses_a_source_token_with_the_wrong_audience() -> Result<(), String>
    {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let request_bodies = captured.clone();
        let kube = KubeClient::new(
            service_fn(move |request: Request<KubeBody>| {
                let request_bodies = request_bodies.clone();
                async move {
                    let body = request
                        .into_body()
                        .collect_bytes()
                        .await
                        .map_err(std::io::Error::other)?;
                    request_bodies
                        .lock()
                        .map_err(|_| std::io::Error::other("poisoned"))?
                        .push(body.to_vec());
                    let wrong = body
                        .windows(b"other-service".len())
                        .any(|window| window == b"other-service");
                    let (audience, token) = if wrong {
                        ("other-service", "obviously-fake-wrong-audience-token")
                    } else {
                        ("steward-task", "obviously-fake-source-token")
                    };
                    let mut response = HttpResponse::new(Body::from(format!(
                        r#"{{"apiVersion":"authentication.k8s.io/v1","kind":"TokenRequest","metadata":{{}},"spec":{{"audiences":["{audience}"],"expirationSeconds":600}},"status":{{"expirationTimestamp":"2030-01-01T00:00:00Z","token":"{token}"}}}}"#
                    )));
                    response.headers_mut().insert(
                        "content-type",
                        "application/json".parse().map_err(std::io::Error::other)?,
                    );
                    Ok::<_, std::io::Error>(response)
                }
            }),
            "default",
        );
        let exchange = tls_fixture_with_responder(2, |request| {
            if request.contains("Bearer obviously-fake-wrong-audience-token") {
                test_response(401, "", "")
            } else {
                test_response(
                    200,
                    "Cache-Control: no-store\r\n",
                    r#"{"access_token":"obviously-fake-access-token","token_type":"Bearer","expires_in":120}"#,
                )
            }
        })
        .await?;
        let wrong = KubernetesWorkloadTokenProvider::new(
            KubernetesTokenRequester::new(
                kube.clone(),
                "steward-test".to_owned(),
                "receiver".to_owned(),
                "other-service".to_owned(),
                600,
            )?,
            format!("{}v1/workload/exchange", exchange.origin),
            "localhost".to_owned(),
            exchange.ca_certificate_pem.clone(),
            exchange_transport(),
        )?;
        let lifecycle = wrong
            .lifecycle()
            .await
            .map_err(|error| format!("{error:?}"))?;
        assert_eq!(
            lifecycle.token().await.err(),
            Some(TaskLifecycleError::Final)
        );
        let good = KubernetesWorkloadTokenProvider::new(
            KubernetesTokenRequester::new(
                kube,
                "steward-test".to_owned(),
                "receiver".to_owned(),
                "steward-task".to_owned(),
                600,
            )?,
            format!("{}v1/workload/exchange", exchange.origin),
            "localhost".to_owned(),
            exchange.ca_certificate_pem.clone(),
            exchange_transport(),
        )?;
        assert_eq!(
            good.lifecycle()
                .await
                .map_err(|error| format!("{error:?}"))?
                .token()
                .await
                .map_err(|error| format!("{error:?}"))?
                .expose_secret(),
            "obviously-fake-access-token"
        );
        exchange.handle.await.map_err(|error| error.to_string())?;
        let bodies = captured.lock().map_err(|_| "poisoned")?;
        let wrong_request: serde_json::Value =
            serde_json::from_slice(&bodies[0]).map_err(|error| error.to_string())?;
        let good_request: serde_json::Value =
            serde_json::from_slice(&bodies[1]).map_err(|error| error.to_string())?;
        assert_eq!(
            wrong_request["spec"]["audiences"],
            serde_json::json!(["other-service"])
        );
        assert_eq!(
            good_request["spec"]["audiences"],
            serde_json::json!(["steward-task"])
        );
        Ok(())
    }

    #[tokio::test]
    async fn workload_exchange_maps_refusal_and_transient_statuses() -> Result<(), String> {
        for (status, expected) in [
            (401, TaskLifecycleError::Final),
            (408, TaskLifecycleError::Retriable),
            (429, TaskLifecycleError::Retriable),
            (503, TaskLifecycleError::Retriable),
        ] {
            let kube = KubeClient::new(
                service_fn(|_request: Request<KubeBody>| async move {
                    let mut response = HttpResponse::new(Body::from(
                        br#"{"apiVersion":"authentication.k8s.io/v1","kind":"TokenRequest","metadata":{},"spec":{"audiences":["steward-task"],"expirationSeconds":600},"status":{"expirationTimestamp":"2030-01-01T00:00:00Z","token":"obviously-fake-source-token"}}"#
                            .to_vec(),
                    ));
                    response.headers_mut().insert(
                        "content-type",
                        "application/json".parse().map_err(std::io::Error::other)?,
                    );
                    Ok::<_, std::io::Error>(response)
                }),
                "default",
            );
            let exchange = tls_fixture(vec![test_response(status, "", "")]).await?;
            let provider = KubernetesWorkloadTokenProvider::new(
                KubernetesTokenRequester::new(
                    kube,
                    "steward-test".to_owned(),
                    "receiver".to_owned(),
                    "steward-task".to_owned(),
                    600,
                )?,
                format!("{}v1/workload/exchange", exchange.origin),
                "localhost".to_owned(),
                exchange.ca_certificate_pem.clone(),
                exchange_transport(),
            )?;
            let lifecycle = provider
                .lifecycle()
                .await
                .map_err(|error| format!("{error:?}"))?;
            assert_eq!(lifecycle.token().await.err(), Some(expected));
            exchange.handle.await.map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn refused_source_token_is_evicted_before_the_next_delivery() -> Result<(), String> {
        let source_calls = Arc::new(Mutex::new(0_usize));
        let counted = source_calls.clone();
        let kube = KubeClient::new(
            service_fn(move |_request: Request<KubeBody>| {
                let counted = counted.clone();
                async move {
                    let call = {
                        let mut count = counted
                            .lock()
                            .map_err(|_| std::io::Error::other("poisoned"))?;
                        *count += 1;
                        *count
                    };
                    let mut response = HttpResponse::new(Body::from(format!(
                        r#"{{"apiVersion":"authentication.k8s.io/v1","kind":"TokenRequest","metadata":{{}},"spec":{{"audiences":["steward-task"],"expirationSeconds":600}},"status":{{"expirationTimestamp":"2030-01-01T00:00:00Z","token":"obviously-fake-source-token-{call}"}}}}"#
                    )));
                    response.headers_mut().insert(
                        "content-type",
                        "application/json".parse().map_err(std::io::Error::other)?,
                    );
                    Ok::<_, std::io::Error>(response)
                }
            }),
            "default",
        );
        let access =
            r#"{"access_token":"obviously-fake-access","token_type":"Bearer","expires_in":120}"#;
        let exchange = tls_fixture(vec![
            test_response(401, "", ""),
            test_response(200, "Cache-Control: no-store\r\n", access),
        ])
        .await?;
        let provider = KubernetesWorkloadTokenProvider::new(
            KubernetesTokenRequester::new(
                kube,
                "steward-test".to_owned(),
                "receiver".to_owned(),
                "steward-task".to_owned(),
                600,
            )?,
            format!("{}v1/workload/exchange", exchange.origin),
            "localhost".to_owned(),
            exchange.ca_certificate_pem.clone(),
            exchange_transport(),
        )?;
        let first = provider
            .lifecycle()
            .await
            .map_err(|error| format!("{error:?}"))?;
        assert_eq!(first.token().await.err(), Some(TaskLifecycleError::Final));
        let second = provider
            .lifecycle()
            .await
            .map_err(|error| format!("{error:?}"))?;
        assert_eq!(
            second
                .token()
                .await
                .map_err(|error| format!("{error:?}"))?
                .expose_secret(),
            "obviously-fake-access"
        );
        exchange.handle.await.map_err(|error| error.to_string())?;
        assert_eq!(*source_calls.lock().map_err(|_| "poisoned")?, 2);
        Ok(())
    }

    #[tokio::test]
    async fn workload_exchange_rejects_a_malformed_ca_at_startup() -> Result<(), String> {
        let kube = KubeClient::new(
            service_fn(|_request: Request<KubeBody>| async move {
                Ok::<_, std::io::Error>(HttpResponse::new(Body::empty()))
            }),
            "default",
        );
        assert!(
            KubernetesWorkloadTokenProvider::new(
                KubernetesTokenRequester::new(
                    kube,
                    "steward-test".to_owned(),
                    "receiver".to_owned(),
                    "steward-task".to_owned(),
                    600,
                )?,
                "https://identity.example.test/v1/workload/exchange".to_owned(),
                "identity.example.test".to_owned(),
                b"not a certificate".to_vec(),
                exchange_transport(),
            )
            .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn workload_exchange_rejects_invalid_transport_at_startup() -> Result<(), String> {
        let exchange = tls_fixture(Vec::new()).await?;
        let kube = KubeClient::new(
            service_fn(|_request: Request<KubeBody>| async move {
                Ok::<_, std::io::Error>(HttpResponse::new(Body::empty()))
            }),
            "default",
        );
        assert!(
            KubernetesWorkloadTokenProvider::new(
                KubernetesTokenRequester::new(
                    kube,
                    "steward-test".to_owned(),
                    "receiver".to_owned(),
                    "steward-task".to_owned(),
                    600,
                )?,
                format!("{}v1/workload/exchange", exchange.origin),
                "localhost".to_owned(),
                exchange.ca_certificate_pem,
                WorkloadExchangeTransport {
                    max_response_bytes: 0,
                    ..exchange_transport()
                },
            )
            .is_err()
        );
        exchange.handle.abort();
        Ok(())
    }

    #[tokio::test]
    async fn envelope_lookup_fails_closed_when_database_is_unreachable() -> Result<(), String> {
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            PgStore::connect_with_acquire_timeout(
                "postgres://steward@127.0.0.1:9/steward",
                Duration::from_millis(25),
            ),
        )
        .await
        .map_err(|_| "database refusal exceeded the receiver deadline")?;
        assert!(result.is_err(), "an unavailable database must fail closed");
        Ok(())
    }

    struct UnavailableEnvelopeStore;

    impl EnvelopeStore for UnavailableEnvelopeStore {
        fn active_provisioned_user_envelopes_by_digest<'a>(
            &'a self,
            _owner: &'a CanonicalUserId,
            _digest: &'a str,
        ) -> ReceiverFuture<'a, Result<Vec<EnvelopeRequestRecord>, StoreError>> {
            Box::pin(async { Err(StoreError::Database("database unavailable".to_owned())) })
        }
    }

    #[tokio::test]
    async fn envelope_authority_maps_database_failure_to_unavailable() -> Result<(), String> {
        let owner = CanonicalUserId::parse(OWNER)?;
        let authority = PgEnvelopeAuthority {
            store: Arc::new(UnavailableEnvelopeStore),
        };
        assert_eq!(
            crate::EnvelopeAuthority::active_tools(
                &authority,
                &owner,
                &format!("steward:sha256:{}", "a".repeat(64)),
            )
            .await,
            Err(crate::EnvelopeError::Unavailable)
        );
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

    #[test]
    fn chart_render_contract_from_environment() -> Result<(), String> {
        let config_json = std::env::var("STEWARD_CHART_RECEIVER_CONFIG").unwrap_or_else(|_| {
            r#"{"jiraSiteUrl":"https://jira.example.com/","triggers":[{"id":"Jira_read","enabled":true,"source":"jira","workflow":"triage@1","service":"event-worker","ownerUserId":"usr_11111111111111111111111111111111","envelopeDigest":"steward:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","filters":{"event":"jira:issue_updated","project":"PROJ","issueType":"10001"},"ignoredAccountIds":["acct-1"],"concurrencyLimit":2,"cooldownSeconds":60,"signingSecretFile":"/run/trigger-signing/Jira_read"}]}"#.to_owned()
        });
        let limits = crate::ReceiverLimits {
            max_config_bytes: 4 * 1024 * 1024,
            max_body_bytes: 1024 * 1024,
            max_identifier_bytes: 1024,
            max_triggers: 4096,
            max_ignored_accounts: 4096,
            max_concurrency: 1024,
            max_cooldown_seconds: 31_536_000,
            max_recent_deliveries: 1_000_000,
            max_issue_cooldowns: 1_000_000,
            max_signing_secret_bytes: 64 * 1024,
        };
        crate::ReceiverConfig::from_json(&config_json, &limits)?;
        let api_server_name = std::env::var("STEWARD_CHART_API_SERVER_NAME")
            .unwrap_or_else(|_| "steward-apiserver.steward.svc.cluster.local".to_owned());
        let api_origin = std::env::var("STEWARD_CHART_API_ORIGIN")
            .unwrap_or_else(|_| format!("https://{api_server_name}:443"));
        validate_task_origin(&api_origin, &api_server_name)?;
        let exchange_server_name = std::env::var("STEWARD_CHART_EXCHANGE_SERVER_NAME")
            .unwrap_or_else(|_| "identity.identity-exchange.svc.cluster.local".to_owned());
        let exchange_endpoint =
            std::env::var("STEWARD_CHART_EXCHANGE_ENDPOINT").unwrap_or_else(|_| {
                format!("https://{exchange_server_name}:8443/v1/workload/exchange")
            });
        steward_adapter_openshell::validate_workload_exchange_endpoint(
            &exchange_endpoint,
            &exchange_server_name,
        )
        .map_err(|_| "rendered workload exchange endpoint is invalid".to_owned())?;

        for (pointer, invalid) in [
            (
                "/triggers/0/envelopeDigest",
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ),
            ("/triggers/0/filters/project", "proj lower"),
            ("/triggers/0/filters/issueType", "not-numeric"),
        ] {
            let mut value: serde_json::Value =
                serde_json::from_str(&config_json).map_err(|error| error.to_string())?;
            *value
                .pointer_mut(pointer)
                .ok_or_else(|| "rendered Trigger field is missing".to_owned())? =
                serde_json::Value::String(invalid.to_owned());
            assert!(
                crate::ReceiverConfig::from_json(&value.to_string(), &limits).is_err(),
                "receiver accepted invalid chart field {pointer}"
            );
        }
        Ok(())
    }

    fn test_timing() -> TaskClientTiming {
        TaskClientTiming {
            deadline: Duration::from_millis(250),
            initial_poll_delay: Duration::from_millis(5),
            max_poll_delay: Duration::from_millis(10),
            connect_timeout: Duration::from_millis(50),
            request_timeout: Duration::from_millis(100),
            max_reply_bytes: 4093,
        }
    }

    #[tokio::test]
    async fn production_task_transport_refuses_redirects() -> Result<(), String> {
        let fixture = tls_fixture(vec![test_response(
            307,
            "Location: https://other.example.test/v1/tasks\r\n",
            "",
        )])
        .await?;
        let client = WorkloadExchangeClient::new(
            fixture.origin.clone(),
            "localhost".to_owned(),
            fixture.ca_certificate_pem.clone(),
            Tokens(Arc::new(Mutex::new(0))),
            test_timing(),
        )?;
        assert_eq!(
            client.start(&trigger()?, invocation()?).await.err(),
            Some(TaskLifecycleError::Final)
        );
        fixture.handle.await.map_err(|error| error.to_string())?;
        assert_eq!(fixture.requests.lock().map_err(|_| "poisoned")?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn production_task_transport_refuses_an_untrusted_ca() -> Result<(), String> {
        let trusted = tls_fixture(Vec::new()).await?;
        let target = tls_fixture(vec![test_response(401, "", "")]).await?;
        let client = WorkloadExchangeClient::new(
            target.origin.clone(),
            "localhost".to_owned(),
            trusted.ca_certificate_pem.clone(),
            Tokens(Arc::new(Mutex::new(0))),
            test_timing(),
        )?;
        assert_eq!(
            client.start(&trigger()?, invocation()?).await.err(),
            Some(TaskLifecycleError::Retriable)
        );
        trusted.handle.abort();
        target.handle.abort();
        Ok(())
    }

    #[tokio::test]
    async fn file_catalog_is_read_again_for_each_submit_check() -> Result<(), String> {
        static NEXT_CATALOG_ID: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "steward-trigger-catalog-{}-{}",
            std::process::id(),
            NEXT_CATALOG_ID.fetch_add(1, Ordering::Relaxed)
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
        fn lifecycle(
            &self,
        ) -> ReceiverFuture<'_, Result<Arc<dyn TaskAccessTokenProvider>, TaskLifecycleError>>
        {
            let tokens = self.clone();
            Box::pin(async move { Ok(Arc::new(tokens) as Arc<dyn TaskAccessTokenProvider>) })
        }
    }

    impl TaskAccessTokenProvider for Tokens {
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
        create_phase: &'static str,
        polls_before_done: usize,
        failure: Option<&'static str>,
        requests: Arc<Mutex<Vec<(Method, String, String)>>>,
        create_bodies: Arc<Mutex<Vec<String>>>,
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
        let body = to_bytes(request.into_body(), 64 * 1024)
            .await
            .unwrap_or_default();
        if method == Method::POST && path == "/v1/tasks" {
            let Ok(mut bodies) = state.create_bodies.lock() else {
                return HttpResponse::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::empty())
                    .unwrap_or_else(|_| HttpResponse::new(Body::empty()));
            };
            bodies.push(String::from_utf8_lossy(&body).into_owned());
        }
        let Ok(mut requests) = state.requests.lock() else {
            return HttpResponse::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::empty())
                .unwrap_or_else(|_| HttpResponse::new(Body::empty()));
        };
        requests.push((method.clone(), path.clone(), auth));
        drop(requests);
        let mut response = if method == Method::POST && path == "/v1/tasks" {
            HttpResponse::new(Body::from(format!(
                r#"{{"taskUid":"00000000-0000-4000-8000-000000000001","phase":"{}"}}"#,
                state.create_phase
            )))
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
            Some("upload") if method == Method::PUT => Some(StatusCode::SERVICE_UNAVAILABLE),
            Some("execute") if method == Method::POST && path.ends_with("/execute") => {
                Some(StatusCode::SERVICE_UNAVAILABLE)
            }
            Some("poll") if method == Method::GET => Some(StatusCode::SERVICE_UNAVAILABLE),
            Some("poll_once") => (method == Method::GET
                && state.polls.lock().map(|polls| *polls == 1).unwrap_or(false))
            .then_some(StatusCode::SERVICE_UNAVAILABLE),
            Some("poll_408_once") => (method == Method::GET
                && state.polls.lock().map(|polls| *polls == 1).unwrap_or(false))
            .then_some(StatusCode::REQUEST_TIMEOUT),
            Some("finalize") if method == Method::DELETE => Some(StatusCode::SERVICE_UNAVAILABLE),
            _ => None,
        };
        *response.status_mut() = if method == Method::POST && path == "/v1/tasks" {
            state.create_status
        } else if let Some(status) = injected_failure {
            status
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
        create_phase: &'static str,
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
            create_phase,
            polls_before_done,
            failure,
            requests: Arc::new(Mutex::new(Vec::new())),
            create_bodies: Arc::new(Mutex::new(Vec::new())),
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
                connect_timeout: Duration::from_secs(1),
                request_timeout: Duration::from_secs(2),
                max_reply_bytes: 64 * 1024,
            },
        };
        Ok((client, state, handle))
    }

    fn invocation() -> Result<TaskInvocation, String> {
        Ok(TaskInvocation {
            workflow: "triage@1".to_owned(),
            envelope_digest: format!("steward:sha256:{}", "a".repeat(64)),
            idempotency_key: format!("jira-sha256:{}", "b".repeat(64)),
            caller_reference: Some(steward_types::CallerReference::new(
                "Jira trigger jira-read · PROJ-123",
                Some("https://jira.example.com/browse/PROJ-123".to_owned()),
            )?),
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
    async fn accepted_terminal_task_is_finalized_before_noop() -> Result<(), String> {
        let (client, state, handle) = client_fixture(
            StatusCode::ACCEPTED,
            "succeeded",
            0,
            Duration::from_secs(1),
            None,
        )
        .await?;
        let result = client
            .start(&trigger()?, invocation()?)
            .await
            .map_err(|error| format!("{error:?}"))?;
        assert!(matches!(result, TaskStart::Duplicate));
        let create_body = state
            .create_bodies
            .lock()
            .map_err(|_| "poisoned")?
            .first()
            .cloned()
            .ok_or("create body missing")?;
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&create_body)
                .map_err(|error| error.to_string())?["callerReference"],
            serde_json::json!({
                "label": "Jira trigger jira-read · PROJ-123",
                "url": "https://jira.example.com/browse/PROJ-123"
            })
        );
        let requests = state.requests.lock().map_err(|_| "poisoned")?;
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].0, Method::DELETE);
        handle.abort();
        Ok(())
    }

    #[tokio::test]
    async fn create_omits_an_unavailable_caller_reference() -> Result<(), String> {
        let (client, state, handle) =
            client_fixture(StatusCode::OK, "succeeded", 0, Duration::from_secs(1), None).await?;
        let mut invocation = invocation()?;
        invocation.caller_reference = None;
        let result = client
            .start(&trigger()?, invocation)
            .await
            .map_err(|error| format!("{error:?}"))?;
        assert!(matches!(result, TaskStart::Duplicate));
        let create_body = state
            .create_bodies
            .lock()
            .map_err(|_| "poisoned")?
            .first()
            .cloned()
            .ok_or("create body missing")?;
        let create: serde_json::Value =
            serde_json::from_str(&create_body).map_err(|error| error.to_string())?;
        assert!(
            !create
                .as_object()
                .ok_or("create body is not an object")?
                .contains_key("callerReference")
        );
        handle.abort();
        Ok(())
    }

    #[tokio::test]
    async fn accepted_terminal_task_reports_finalize_failure() -> Result<(), String> {
        let (client, state, handle) = client_fixture(
            StatusCode::ACCEPTED,
            "succeeded",
            0,
            Duration::from_secs(1),
            Some("finalize"),
        )
        .await?;
        assert_eq!(
            client.start(&trigger()?, invocation()?).await.err(),
            Some(TaskLifecycleError::Retriable)
        );
        assert!(
            state
                .requests
                .lock()
                .map_err(|_| "poisoned")?
                .iter()
                .any(|(method, _, _)| *method == Method::DELETE)
        );
        handle.abort();
        Ok(())
    }

    #[tokio::test]
    async fn accepted_task_polls_with_fresh_tokens_and_finalizes() -> Result<(), String> {
        let (client, state, handle) = client_fixture(
            StatusCode::ACCEPTED,
            "submitted",
            1,
            Duration::from_secs(1),
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
        {
            let requests = state.requests.lock().map_err(|_| "poisoned")?;
            assert!(
                requests.iter().any(|(method, path, _)| {
                    *method == Method::PUT && path.ends_with("/inputs")
                }),
                "input upload must finish before Started is returned"
            );
            assert!(
                requests.iter().any(|(method, path, _)| {
                    *method == Method::POST && path.ends_with("/execute")
                }),
                "execute must finish before Started is returned"
            );
        }
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
            "submitted",
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
            (StatusCode::REQUEST_TIMEOUT, TaskLifecycleError::Retriable),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                TaskLifecycleError::Retriable,
            ),
            (StatusCode::FORBIDDEN, TaskLifecycleError::Final),
        ] {
            let (client, _, handle) =
                client_fixture(status, "submitted", 0, Duration::from_secs(1), None).await?;
            assert_eq!(
                client.start(&trigger()?, invocation()?).await.err(),
                Some(expected)
            );
            handle.abort();
        }
        Ok(())
    }

    #[tokio::test]
    async fn prepare_failure_is_returned_before_started_and_left_for_retry() -> Result<(), String> {
        for failure in ["upload", "execute"] {
            let (client, state, handle) = client_fixture(
                StatusCode::ACCEPTED,
                "submitted",
                0,
                Duration::from_secs(1),
                Some(failure),
            )
            .await?;
            assert_eq!(
                client.start(&trigger()?, invocation()?).await.err(),
                Some(TaskLifecycleError::Retriable)
            );
            assert!(
                !state
                    .requests
                    .lock()
                    .map_err(|_| "poisoned")?
                    .iter()
                    .any(|(method, _, _)| *method == Method::DELETE),
                "{failure} must leave the Task resumable for webhook retry"
            );
            handle.abort();
        }
        Ok(())
    }

    #[tokio::test]
    async fn transient_poll_failure_retries_before_finalization() -> Result<(), String> {
        let (client, state, handle) = client_fixture(
            StatusCode::ACCEPTED,
            "submitted",
            0,
            Duration::from_secs(1),
            Some("poll_once"),
        )
        .await?;
        let TaskStart::Started(completion) = client
            .start(&trigger()?, invocation()?)
            .await
            .map_err(|error| format!("{error:?}"))?
        else {
            return Err("expected started".to_owned());
        };
        completion.await.map_err(|error| format!("{error:?}"))?;
        let requests = state.requests.lock().map_err(|_| "poisoned")?;
        let poll_positions = requests
            .iter()
            .enumerate()
            .filter_map(|(index, (method, _, _))| (*method == Method::GET).then_some(index))
            .collect::<Vec<_>>();
        let finalize_position = requests
            .iter()
            .position(|(method, _, _)| *method == Method::DELETE)
            .ok_or("finalize was not attempted")?;
        assert_eq!(poll_positions.len(), 2);
        assert!(poll_positions[1] < finalize_position);
        handle.abort();
        Ok(())
    }

    #[tokio::test]
    async fn poll_request_timeout_retries_before_finalization() -> Result<(), String> {
        let (client, state, handle) = client_fixture(
            StatusCode::ACCEPTED,
            "submitted",
            0,
            Duration::from_secs(1),
            Some("poll_408_once"),
        )
        .await?;
        let TaskStart::Started(completion) = client
            .start(&trigger()?, invocation()?)
            .await
            .map_err(|error| format!("{error:?}"))?
        else {
            return Err("expected started".to_owned());
        };
        completion.await.map_err(|error| format!("{error:?}"))?;
        let requests = state.requests.lock().map_err(|_| "poisoned")?;
        let poll_positions = requests
            .iter()
            .enumerate()
            .filter_map(|(index, (method, _, _))| (*method == Method::GET).then_some(index))
            .collect::<Vec<_>>();
        let finalize_position = requests
            .iter()
            .position(|(method, _, _)| *method == Method::DELETE)
            .ok_or("finalize was not attempted")?;
        assert_eq!(poll_positions.len(), 2);
        assert!(poll_positions[1] < finalize_position);
        handle.abort();
        Ok(())
    }

    #[tokio::test]
    async fn duplicate_create_never_mutates_the_existing_task() -> Result<(), String> {
        for phase in [
            "submitted",
            "parked",
            "queued",
            "running",
            "succeeded",
            "failed",
            "cancelled",
        ] {
            let (client, state, handle) =
                client_fixture(StatusCode::OK, phase, 0, Duration::from_secs(1), None).await?;
            assert!(matches!(
                client
                    .start(&trigger()?, invocation()?)
                    .await
                    .map_err(|error| format!("{error:?}"))?,
                TaskStart::Duplicate
            ));
            let requests = state.requests.lock().map_err(|_| "poisoned")?;
            assert_eq!(
                requests.len(),
                1,
                "a {phase} duplicate must make only the create request"
            );
            assert_eq!(requests[0].0, Method::POST);
            assert_eq!(requests[0].1, "/v1/tasks");
            drop(requests);
            handle.abort();
        }
        Ok(())
    }
}
