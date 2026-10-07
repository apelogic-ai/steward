//! Server-owned planning and orchestration for governed provider-control runtimes.

use std::collections::BTreeSet;
use std::hash::Hash;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use reqwest::Url;
use reqwest::header::AUTHORIZATION;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use steward_adapter_mcp_gw::{
    GithubBridgeFailureDiagnostic, GithubStatusCredential, GithubStatusReader,
    github_bridge_failure_diagnostic,
};
use steward_admission::internal_authorities::{
    steward_connections_v1, steward_connections_v2, steward_connections_v3, steward_connections_v4,
};
use steward_admission::{AdmissionDecision, Envelope, evaluate};
use steward_store::{
    ConnectionExecutionBindingSnapshot, ConnectionOAuthPhase,
    ConnectionOperationKind as StoredOperationKind, ConnectionOperationRecord,
    ConnectionOperationReservationRequest, ConnectionOperationRetention, ConnectionOperationState,
    ConnectionOperationTiming, FederatedSubjectObservation, PgStore, StoreError,
    TaskOrchestrationMode, TaskReservationRequest,
};
use steward_types::{
    AgentRuntimeSpec, AgentType, CanonicalAuthorityBinding, CanonicalUserId, Email, Principal,
    ToolGrant,
};
use uuid::Uuid;

use crate::BoxFuture;
use crate::connections::{
    AuthorizationUrl, ConnectionBrokerError, ConnectionPhase, ConnectionSession,
    ConnectionStartOperation, GithubWorkflowRerunBroker, GithubWorkflowRerunRequest,
    ProviderConnectionBroker, ProviderConnectionStatus, ReservedConnectionStart, StartedConnection,
};

pub const CONNECTIONS_SERVICE: &str = steward_connections_v1::SERVICE;
pub const CONNECTIONS_AUTHORITY_VERSION: i64 = steward_connections_v1::AUTHORITY_VERSION;
pub const CONNECTIONS_AUTHORITY_DIGEST: &str = steward_connections_v1::AUTHORITY_DIGEST;
pub const CONNECTIONS_AUTHORITY_DOCUMENT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../config/internal-authorities/steward-connections/v1.json"
));
const CONNECTIONS_BRIDGE_BINARY: &str = steward_connections_v1::BRIDGE_BINARY;
pub const CONNECTION_RESPONSE_DEADLINE_SECONDS: i64 =
    steward_connections_v1::RESPONSE_DEADLINE_SECONDS;
pub const CONNECTION_STATUS_CACHE_SECONDS: i64 = 5;
pub const CONNECTION_MUTATION_RESULT_SECONDS: i64 = 30;
pub const GITHUB_REPOSITORY_CACHE_SECONDS: i64 = 60;
pub const GITHUB_RERUN_RESULT_SECONDS: i64 = 600;
pub const CONNECTION_CLEANUP_STALL_SECONDS: i64 = 150;
pub const MCP_GW_OAUTH_STATE_LIFETIME_SECONDS: i64 =
    steward_connections_v1::OAUTH_STATE_LIFETIME_SECONDS;
pub const MCP_GW_OAUTH_CLOCK_SKEW_SECONDS: i64 = steward_connections_v1::OAUTH_CLOCK_SKEW_SECONDS;
pub const MCP_GW_CONTRACT_VERSION: &str = steward_connections_v1::MCP_GW_VERSION;
pub const GITHUB_ATTESTATION_TRUST_MODE: &str = "github-attestation";
pub const OPERATOR_PINNED_TRUST_MODE: &str = "operator-pinned";
const MAX_BRIDGE_RESULT_BYTES: usize = 32 * 1024;
const TAR_BLOCK_BYTES: usize = 512;

const fn connection_result_ttl_seconds(operation: StoredOperationKind) -> i64 {
    match operation {
        StoredOperationKind::Repositories => GITHUB_REPOSITORY_CACHE_SECONDS,
        StoredOperationKind::Rerun => GITHUB_RERUN_RESULT_SECONDS,
        _ => CONNECTION_MUTATION_RESULT_SECONDS,
    }
}
const RECONCILE_INTERVAL: StdDuration = StdDuration::from_millis(100);
const DIRECT_STATUS_DEADLINE: StdDuration = StdDuration::from_secs(1);
const CONNECTION_ASSOCIATION_DEADLINE: StdDuration = StdDuration::from_secs(1);
const CONTROL_PLANE_STATUS_SCOPE: &str = "connections_status";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionOperationKind {
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectionExecutionBindings {
    pub artifact_trust_mode: String,
    pub bridge_image_digest: String,
    pub mcp_gw_origin: String,
    pub mcp_gw_version: String,
    pub namespace: String,
    pub runtime_class: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GovernedConnectionPlan {
    pub spec: AgentRuntimeSpec,
    pub command: Vec<String>,
    pub authority_id: &'static str,
    pub authority_version: i64,
    pub authority_digest: String,
    pub bindings: ConnectionExecutionBindings,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GovernedConnectionPlanError {
    Admission,
    InvalidBindings,
    Unavailable,
}

fn valid_operator_pinned_image(value: &str) -> bool {
    if value
        .bytes()
        .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
        || value.matches('@').count() != 1
        || value.contains("://")
    {
        return false;
    }
    let Some((repository, digest)) = value.split_once('@') else {
        return false;
    };
    let mut components = repository.split('/');
    let Some(registry) = components.next() else {
        return false;
    };
    let (registry, port) = registry
        .split_once(':')
        .map_or((registry, None), |(registry, port)| (registry, Some(port)));
    let valid_component = |component: &str| {
        !component.is_empty()
            && component.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'_' | b'-')
            })
    };
    if !valid_component(registry)
        || port
            .is_some_and(|port| port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()))
        || components.any(|component| !valid_component(component))
    {
        return false;
    }
    digest.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            && hex.bytes().any(|byte| byte != b'0')
    })
}

fn operation_grants(
    operation: ConnectionOperationKind,
) -> Result<Vec<ToolGrant>, GovernedConnectionPlanError> {
    steward_connections_v4::operation_grants(operation.action())
        .ok_or(GovernedConnectionPlanError::Admission)
}

fn connection_authority(
    version: &str,
) -> Result<(Envelope, i64, &'static str), GovernedConnectionPlanError> {
    match version {
        steward_connections_v1::MCP_GW_VERSION => Ok((
            steward_connections_v1::envelope(),
            steward_connections_v1::AUTHORITY_VERSION,
            steward_connections_v1::AUTHORITY_DIGEST,
        )),
        steward_connections_v4::MCP_GW_VERSION => Ok((
            steward_connections_v4::envelope(),
            steward_connections_v4::AUTHORITY_VERSION,
            steward_connections_v4::AUTHORITY_DIGEST,
        )),
        _ => Err(GovernedConnectionPlanError::InvalidBindings),
    }
}

impl ConnectionOperationKind {
    pub const fn action(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Start => "start",
            Self::Disconnect => "disconnect",
            Self::Rerun => "rerun",
            Self::Repositories => "repositories",
            Self::Workflow => "workflow",
            Self::RunStatus => "run-status",
            Self::Dispatch => "dispatch",
            Self::Publish => "publish",
        }
    }

    pub const fn bridge_operation(self) -> &'static str {
        match self {
            Self::Status => "github.status",
            Self::Start => "github.start",
            Self::Disconnect => "github.disconnect",
            Self::Rerun => "github.rerun",
            Self::Repositories => "github.repositories",
            Self::Workflow => "github.workflow",
            Self::RunStatus => "github.run-status",
            Self::Dispatch => "github.dispatch",
            Self::Publish => "github.publish",
        }
    }
}

impl ConnectionExecutionBindings {
    pub fn validate(&self) -> Result<(), GovernedConnectionPlanError> {
        let github_attested_image_is_digest_pinned = self
            .bridge_image_digest
            .split_once("@sha256:")
            .is_some_and(|(repository, digest)| {
                !repository.is_empty()
                    && digest.len() == 64
                    && digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                    && digest.bytes().any(|byte| byte != b'0')
            });
        if !match self.artifact_trust_mode.as_str() {
            GITHUB_ATTESTATION_TRUST_MODE => github_attested_image_is_digest_pinned,
            OPERATOR_PINNED_TRUST_MODE => valid_operator_pinned_image(&self.bridge_image_digest),
            _ => false,
        } {
            return Err(GovernedConnectionPlanError::InvalidBindings);
        }
        let origin = Url::parse(&self.mcp_gw_origin)
            .map_err(|_| GovernedConnectionPlanError::InvalidBindings)?;
        if !matches!(origin.scheme(), "http" | "https")
            || origin.host_str().is_none()
            || !origin.username().is_empty()
            || origin.password().is_some()
            || !matches!(origin.path(), "" | "/")
            || origin.query().is_some()
            || origin.fragment().is_some()
            || connection_authority(&self.mcp_gw_version).is_err()
            || self.namespace.trim().is_empty()
            || (!self.runtime_class.is_empty() && self.runtime_class.trim().is_empty())
        {
            return Err(GovernedConnectionPlanError::InvalidBindings);
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct GovernedConnectionsConfig {
    pub bindings: ConnectionExecutionBindings,
    redirect_after: String,
}

#[derive(Clone)]
pub struct DirectConnectionStatusConfig {
    pub control_plane_credential_file: PathBuf,
    pub mint_origin: String,
    pub federated_subject_issuer: Option<String>,
}

#[derive(Clone)]
pub struct DirectConnectionStatusReader {
    client: reqwest::Client,
    control_plane_credential_file: PathBuf,
    gateway: GithubStatusReader,
    mint_endpoint: Url,
    store: PgStore,
    federated_subject_issuer: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MintStatusTokenRequest {
    principal: Principal,
    canonical_authority: CanonicalAuthorityBinding,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MintStatusTokenResponse {
    access_token: String,
    expires_in: u64,
    scope: String,
    token_type: String,
}

pub trait ProviderConnectionStatusSource<B>: Clone + Send + Sync + 'static
where
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    fn status<'a>(
        &'a self,
        session: &'a ConnectionSession<B>,
    ) -> BoxFuture<'a, Result<ProviderConnectionStatus, ConnectionBrokerError>>;
}

#[derive(Clone)]
pub struct SplitConnectionsBroker<M, S> {
    mutations: M,
    status: S,
}

impl<M, S> SplitConnectionsBroker<M, S> {
    pub fn new(mutations: M, status: S) -> Self {
        Self { mutations, status }
    }

    pub fn governed_mutations(&self) -> &M {
        &self.mutations
    }
}

impl GovernedConnectionsConfig {
    pub fn new(
        bindings: ConnectionExecutionBindings,
        browser_origin: &str,
    ) -> Result<Self, GovernedConnectionPlanError> {
        bindings.validate()?;
        let mut redirect =
            Url::parse(browser_origin).map_err(|_| GovernedConnectionPlanError::InvalidBindings)?;
        let loopback_http = redirect.scheme() == "http" && redirect.host_str() == Some("127.0.0.1");
        if redirect.host_str().is_none()
            || redirect.port_or_known_default().is_none()
            || (redirect.scheme() != "https" && !loopback_http)
            || !redirect.username().is_empty()
            || redirect.password().is_some()
            || redirect.path() != "/"
            || redirect.query().is_some()
            || redirect.fragment().is_some()
        {
            return Err(GovernedConnectionPlanError::InvalidBindings);
        }
        redirect.set_path("/connections");
        redirect.set_fragment(Some("github-connected"));
        Ok(Self {
            bindings,
            redirect_after: redirect.to_string(),
        })
    }
}

impl DirectConnectionStatusReader {
    pub fn new(
        store: PgStore,
        config: DirectConnectionStatusConfig,
        mcp_gw_origin: &str,
        mcp_gw_version: &str,
    ) -> Result<Self, GovernedConnectionPlanError> {
        let mint_origin = Url::parse(&config.mint_origin)
            .map_err(|_| GovernedConnectionPlanError::InvalidBindings)?;
        if !matches!(mint_origin.scheme(), "http" | "https")
            || mint_origin.host_str().is_none()
            || !mint_origin.username().is_empty()
            || mint_origin.password().is_some()
            || mint_origin.path() != "/"
            || mint_origin.query().is_some()
            || mint_origin.fragment().is_some()
            || config.control_plane_credential_file.as_os_str().is_empty()
            || config
                .federated_subject_issuer
                .as_deref()
                .is_some_and(|issuer| {
                    !issuer.starts_with("https://")
                        || issuer.len() > 2_048
                        || issuer.chars().any(char::is_whitespace)
                })
        {
            return Err(GovernedConnectionPlanError::InvalidBindings);
        }
        let mint_endpoint = mint_origin
            .join("/control-plane/token")
            .map_err(|_| GovernedConnectionPlanError::InvalidBindings)?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(DIRECT_STATUS_DEADLINE)
            .timeout(DIRECT_STATUS_DEADLINE)
            .build()
            .map_err(|_| GovernedConnectionPlanError::Unavailable)?;
        let gateway = GithubStatusReader::new(mcp_gw_origin, mcp_gw_version)
            .map_err(|_| GovernedConnectionPlanError::InvalidBindings)?;
        Ok(Self {
            client,
            control_plane_credential_file: config.control_plane_credential_file,
            gateway,
            mint_endpoint,
            store,
            federated_subject_issuer: config.federated_subject_issuer,
        })
    }

    async fn read<B>(
        &self,
        session: &ConnectionSession<B>,
    ) -> Result<ProviderConnectionStatus, ConnectionBrokerError> {
        let mut status = tokio::time::timeout(DIRECT_STATUS_DEADLINE, self.read_inner(session))
            .await
            .map_err(|_| ConnectionBrokerError::Unavailable)??;
        if status.phase == ConnectionPhase::Connected {
            status.github_actions_identity_linked = tokio::time::timeout(
                CONNECTION_ASSOCIATION_DEADLINE,
                self.associate_github_actions_identity(session, &status),
            )
            .await
            .unwrap_or_else(|_| {
                eprintln!(
                    "best-effort GitHub connection identity association failed: category=store_timeout"
                );
                None
            });
        }
        Ok(status)
    }

    async fn read_inner<B>(
        &self,
        session: &ConnectionSession<B>,
    ) -> Result<ProviderConnectionStatus, ConnectionBrokerError> {
        let workload_credential = std::fs::read_to_string(&self.control_plane_credential_file)
            .map_err(|_| ConnectionBrokerError::Unavailable)?;
        if workload_credential.is_empty()
            || workload_credential.len() > 16 * 1024
            || workload_credential.trim() != workload_credential
        {
            return Err(ConnectionBrokerError::Unavailable);
        }
        let email = Email::parse(session.subject.display_email.clone())
            .map_err(|_| ConnectionBrokerError::Unavailable)?;
        let canonical_authority = CanonicalAuthorityBinding::new(
            session.subject.canonical_user_id.clone(),
            Some(session.subject.canonical_user_id.clone()),
        )
        .map_err(|_| ConnectionBrokerError::Unavailable)?;
        let response = self
            .client
            .post(self.mint_endpoint.clone())
            .header(AUTHORIZATION, format!("Bearer {workload_credential}"))
            .json(&MintStatusTokenRequest {
                principal: Principal::User { acting_user: email },
                canonical_authority,
            })
            .send()
            .await
            .map_err(|_| ConnectionBrokerError::Unavailable)?;
        if response.status() != reqwest::StatusCode::OK
            || response
                .content_length()
                .is_some_and(|length| length > MAX_BRIDGE_RESULT_BYTES as u64)
        {
            return Err(ConnectionBrokerError::Unavailable);
        }
        let body = response
            .bytes()
            .await
            .map_err(|_| ConnectionBrokerError::Unavailable)?;
        if body.len() > MAX_BRIDGE_RESULT_BYTES {
            return Err(ConnectionBrokerError::Unavailable);
        }
        let token: MintStatusTokenResponse =
            serde_json::from_slice(&body).map_err(|_| ConnectionBrokerError::Unavailable)?;
        if token.token_type != "Bearer"
            || token.scope != CONTROL_PLANE_STATUS_SCOPE
            || !(1..=15).contains(&token.expires_in)
        {
            return Err(ConnectionBrokerError::Unavailable);
        }
        let credential = GithubStatusCredential::new(token.access_token)
            .map_err(|_| ConnectionBrokerError::Unavailable)?;
        let value = self
            .gateway
            .read(&credential)
            .await
            .map_err(|_| ConnectionBrokerError::Unavailable)?;
        let status = provider_status(&value)?;
        if status.phase == ConnectionPhase::Connected {
            self.store
                .complete_pending_connection_oauth_flow(&session.subject.canonical_user_id)
                .await
                .map_err(|_| ConnectionBrokerError::Unavailable)?;
        }
        Ok(status)
    }

    async fn associate_github_actions_identity<B>(
        &self,
        session: &ConnectionSession<B>,
        status: &ProviderConnectionStatus,
    ) -> Option<bool> {
        let (Some(issuer), Some(account_id)) = (
            self.federated_subject_issuer.as_deref(),
            status.account_id.as_deref(),
        ) else {
            return None;
        };
        let subject = github_actions_subject(account_id)?;
        match self
            .store
            .associate_federated_subject_from_connection(
                FederatedSubjectObservation {
                    issuer,
                    subject: &subject,
                    actor_login: status.account_login.as_deref(),
                    display_name: status.account_email.as_deref(),
                },
                &session.subject.canonical_user_id,
                "github",
                account_id,
            )
            .await
        {
            Ok(_) => Some(true),
            Err(StoreError::FederatedSubjectConflict | StoreError::FederatedSubjectDisabled) => {
                Some(false)
            }
            Err(error) => {
                eprintln!(
                    "best-effort GitHub connection identity association failed: category={}",
                    connection_association_failure_category(&error)
                );
                None
            }
        }
    }
}

impl<B> ProviderConnectionStatusSource<B> for DirectConnectionStatusReader
where
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    fn status<'a>(
        &'a self,
        session: &'a ConnectionSession<B>,
    ) -> BoxFuture<'a, Result<ProviderConnectionStatus, ConnectionBrokerError>> {
        Box::pin(async move { self.read(session).await })
    }
}

impl<B, M, S> ProviderConnectionBroker<B> for SplitConnectionsBroker<M, S>
where
    B: Clone + Eq + Hash + Send + Sync + 'static,
    M: ProviderConnectionBroker<B>,
    S: ProviderConnectionStatusSource<B>,
{
    fn status<'a>(
        &'a self,
        session: &'a ConnectionSession<B>,
    ) -> BoxFuture<'a, Result<ProviderConnectionStatus, ConnectionBrokerError>> {
        self.status.status(session)
    }

    fn start<'a>(
        &'a self,
        session: &'a ConnectionSession<B>,
    ) -> BoxFuture<'a, Result<ReservedConnectionStart, ConnectionBrokerError>> {
        self.mutations.start(session)
    }

    fn start_operation<'a>(
        &'a self,
        session: &'a ConnectionSession<B>,
        operation_id: Uuid,
    ) -> BoxFuture<'a, Result<Option<ConnectionStartOperation>, ConnectionBrokerError>> {
        self.mutations.start_operation(session, operation_id)
    }

    fn disconnect<'a>(
        &'a self,
        session: &'a ConnectionSession<B>,
    ) -> BoxFuture<'a, Result<ReservedConnectionStart, ConnectionBrokerError>> {
        self.mutations.disconnect(session)
    }
}

impl<B, M, S> GithubWorkflowRerunBroker<B> for SplitConnectionsBroker<M, S>
where
    B: Clone + Eq + Hash + Send + Sync + 'static,
    M: GithubWorkflowRerunBroker<B>,
    S: ProviderConnectionStatusSource<B>,
{
    fn rerun<'a>(
        &'a self,
        session: &'a ConnectionSession<B>,
        request: &'a GithubWorkflowRerunRequest,
    ) -> BoxFuture<'a, Result<(), ConnectionBrokerError>> {
        self.mutations.rerun(session, request)
    }
}

#[derive(Clone)]
pub struct GovernedConnectionsBroker<B> {
    store: PgStore,
    config: GovernedConnectionsConfig,
    binding: PhantomData<fn() -> B>,
    orchestration_mode: TaskOrchestrationMode,
    failure_reporter: Arc<dyn Fn(String) + Send + Sync>,
}

#[derive(Clone, Copy, Default)]
struct ConnectionReservationIdentity<'a> {
    operation: Option<&'a str>,
    client_scope: Option<&'a str>,
    publication_branch: Option<&'a str>,
}

struct ConnectionReservation<'a> {
    operation: ConnectionOperationKind,
    allow_status_cache: bool,
    allow_result_cache: bool,
    request_body: Option<Value>,
    identity: ConnectionReservationIdentity<'a>,
}

pub(crate) struct AutomationOperationIdentity<'a> {
    pub(crate) idempotency_identity: &'a str,
    pub(crate) idempotency_scope: Option<&'a str>,
    pub(crate) publication_subject: Option<&'a str>,
    pub(crate) allow_result_cache: bool,
}

const STAGED_CONNECTIONS_WARNING: &str = "connections bridge enabled but taskOrchestrationMode=staged; Connections operations are refused";

fn connection_orchestration_error(mode: TaskOrchestrationMode) -> Option<ConnectionBrokerError> {
    (!mode.is_active()).then_some(ConnectionBrokerError::OrchestrationNotActive)
}

fn connections_startup_warning(mode: TaskOrchestrationMode) -> Option<&'static str> {
    (!mode.is_active()).then_some(STAGED_CONNECTIONS_WARNING)
}

impl<B> GovernedConnectionsBroker<B> {
    pub fn new(
        store: PgStore,
        config: GovernedConnectionsConfig,
        orchestration_mode: TaskOrchestrationMode,
    ) -> Self {
        if let Some(warning) = connections_startup_warning(orchestration_mode) {
            eprintln!("{warning}");
        }
        Self {
            store,
            config,
            binding: PhantomData,
            orchestration_mode,
            failure_reporter: Arc::new(|line| eprintln!("{line}")),
        }
    }

    pub fn with_failure_reporter(
        mut self,
        reporter: impl Fn(String) + Send + Sync + 'static,
    ) -> Self {
        self.failure_reporter = Arc::new(reporter);
        self
    }

    async fn reserve(
        &self,
        canonical_user_id: &CanonicalUserId,
        display_email: &str,
        reservation: ConnectionReservation<'_>,
    ) -> Result<ConnectionOperationRecord, ConnectionBrokerError> {
        if let Some(error) = connection_orchestration_error(self.orchestration_mode) {
            return Err(error);
        }
        let email = Email::parse(display_email.to_owned())
            .map_err(|_| ConnectionBrokerError::Unavailable)?;
        let operation = reservation.operation;
        let plan = plan_connection_operation(
            canonical_user_id,
            &email,
            operation,
            self.config.bindings.clone(),
        )
        .map_err(|_| ConnectionBrokerError::Unavailable)?;
        let body = match operation {
            ConnectionOperationKind::Start => json!({"redirectAfter": self.config.redirect_after}),
            ConnectionOperationKind::Status | ConnectionOperationKind::Disconnect => json!({}),
            ConnectionOperationKind::Rerun
            | ConnectionOperationKind::Repositories
            | ConnectionOperationKind::Workflow
            | ConnectionOperationKind::RunStatus
            | ConnectionOperationKind::Dispatch
            | ConnectionOperationKind::Publish => reservation
                .request_body
                .ok_or(ConnectionBrokerError::Unavailable)?,
        };
        let input = single_file_archive(
            "request.json",
            &serde_json::to_vec(&body).map_err(|_| ConnectionBrokerError::Unavailable)?,
        )?;
        let operation_id = Uuid::new_v4();
        let operation_key = reservation
            .identity
            .operation
            .map(str::to_owned)
            .unwrap_or_else(|| operation_id.to_string());
        let runtime_name = format!("conn-{}", operation_id.simple());
        let acting_user_id = canonical_user_id.as_str();
        let bindings = ConnectionExecutionBindingSnapshot {
            artifact_trust_mode: plan.bindings.artifact_trust_mode.clone(),
            bridge_image_digest: plan.bindings.bridge_image_digest.clone(),
            mcp_gw_origin: plan.bindings.mcp_gw_origin.clone(),
            mcp_gw_version: plan.bindings.mcp_gw_version.clone(),
            namespace: plan.bindings.namespace.clone(),
            runtime_class: plan.bindings.runtime_class.clone(),
        };
        let (internal_authority, _, _) = connection_authority(&plan.bindings.mcp_gw_version)
            .map_err(|_| ConnectionBrokerError::Unavailable)?;
        let admission = evaluate(&plan.spec, &internal_authority)
            .map_err(|_| ConnectionBrokerError::Unavailable)?;
        // Connection reservations use one identity for the operation and its Task.
        // Manifest digests must describe the identity the store actually persists.
        let task_uid = operation_id;
        let orchestration = super::tasks::task_orchestration_reservation(
            task_uid,
            operation_id,
            &bindings.namespace,
            &runtime_name,
            &plan.spec,
            &internal_authority,
            None,
        )
        .map_err(|_| ConnectionBrokerError::Unavailable)?;
        let task = TaskReservationRequest {
            task_uid,
            operation_id,
            idempotency_key: &operation_key,
            submitter_service: CONNECTIONS_SERVICE,
            acting_user: Some(email.as_str()),
            acting_user_id: Some(acting_user_id),
            owner: email.as_str(),
            owner_user_id: canonical_user_id.as_str(),
            workflow: match plan.authority_version {
                steward_connections_v4::AUTHORITY_VERSION => "internal:steward-connections/v4",
                steward_connections_v3::AUTHORITY_VERSION => "internal:steward-connections/v3",
                steward_connections_v2::AUTHORITY_VERSION => "internal:steward-connections/v2",
                _ => "internal:steward-connections/v1",
            },
            workflow_name: None,
            workflow_version: None,
            workflow_digest: None,
            user_envelope_instance_id: None,
            user_envelope_revision: None,
            user_envelope_digest: None,
            coding_agent_runtime: "connections-bridge",
            runtime_uid: None,
            runtime_namespace: &bindings.namespace,
            runtime_name: &runtime_name,
            runtime_ownership: steward_types::RuntimeOwnership::Provisioned,
            runtime_spec: &plan.spec,
            agent_command: &plan.command,
            execution_binding: None,
            source_provenance: None,
            direct_task_evidence: None,
            task_origin: steward_types::direct_package::TaskOrigin::Connections,
            browser_task_evidence: None,
            user_envelope_snapshot: None,
            candidate_digest: &orchestration.candidate_digest,
            admission_decision: &admission,
            inert_manifest_digest: &orchestration.inert_manifest_digest,
            active_manifest_digest: &orchestration.active_manifest_digest,
        };
        let reservation = self
            .store
            .reserve_connection_operation(&ConnectionOperationReservationRequest {
                operation_id,
                operation_kind: operation.into(),
                authority_id: plan.authority_id,
                authority_version: plan.authority_version,
                authority_digest: &plan.authority_digest,
                bindings: &bindings,
                idempotency_identity: &operation_key,
                idempotency_scope: reservation.identity.client_scope,
                publication_branch: reservation.identity.publication_branch,
                response_deadline_seconds: CONNECTION_RESPONSE_DEADLINE_SECONDS,
                allow_status_cache: reservation.allow_status_cache,
                allow_result_cache: reservation.allow_result_cache,
                input_archive: &input,
                task,
            })
            .await
            .map_err(store_broker_error)?;
        for failure in reservation.failed_operations {
            (self.failure_reporter)(connection_operation_category_log_line(
                failure.operation_id,
                failure.category,
            ));
        }
        Ok(reservation.record)
    }

    async fn wait(
        &self,
        canonical_user_id: &CanonicalUserId,
        operation_id: Uuid,
    ) -> Result<ConnectionOperationRecord, ConnectionBrokerError> {
        let deadline = tokio::time::Instant::now()
            + StdDuration::from_secs(CONNECTION_RESPONSE_DEADLINE_SECONDS as u64);
        loop {
            let record = self
                .store
                .connection_operation(operation_id, canonical_user_id)
                .await
                .map_err(|_| ConnectionBrokerError::Unavailable)?
                .ok_or(ConnectionBrokerError::Unavailable)?;
            match record.operation_state {
                ConnectionOperationState::Succeeded => return Ok(record),
                ConnectionOperationState::Failed => {
                    return Err(connection_broker_error(
                        record.failure_category.as_deref(),
                        record.failure_detail.as_ref(),
                    ));
                }
                ConnectionOperationState::Queued
                | ConnectionOperationState::Provisioning
                | ConnectionOperationState::Running => {}
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ConnectionBrokerError::Unavailable);
            }
            tokio::time::sleep(RECONCILE_INTERVAL).await;
        }
    }

    pub(crate) async fn run_automation_operation(
        &self,
        session: &ConnectionSession<B>,
        operation: ConnectionOperationKind,
        mut request_body: Value,
        identity: AutomationOperationIdentity<'_>,
    ) -> Result<Value, ConnectionBrokerError> {
        if matches!(
            operation,
            ConnectionOperationKind::Status
                | ConnectionOperationKind::Start
                | ConnectionOperationKind::Disconnect
                | ConnectionOperationKind::Rerun
        ) {
            return Err(ConnectionBrokerError::Unavailable);
        }
        if operation == ConnectionOperationKind::Publish {
            let subject = identity
                .publication_subject
                .ok_or(ConnectionBrokerError::Unavailable)?;
            let previous_branch = self
                .store
                .connection_publication_branch(&session.subject.canonical_user_id, subject)
                .await
                .map_err(store_broker_error)?;
            let request = request_body
                .as_object_mut()
                .ok_or(ConnectionBrokerError::Unavailable)?;
            let requested_branch = request
                .get("branch")
                .and_then(Value::as_str)
                .ok_or(ConnectionBrokerError::Unavailable)?;
            let resume_owned_branch = previous_branch.is_some();
            let branch = previous_branch
                .unwrap_or_else(|| format!("{requested_branch}-{}", Uuid::new_v4().simple()));
            request.insert("branch".to_owned(), Value::String(branch.clone()));
            request.insert(
                "resumeOwnedBranch".to_owned(),
                Value::Bool(resume_owned_branch),
            );
            let record = self
                .reserve(
                    &session.subject.canonical_user_id,
                    &session.subject.display_email,
                    ConnectionReservation {
                        operation,
                        allow_status_cache: true,
                        allow_result_cache: true,
                        request_body: Some(request_body),
                        identity: ConnectionReservationIdentity {
                            operation: Some(identity.idempotency_identity),
                            client_scope: identity.idempotency_scope,
                            publication_branch: Some(&branch),
                        },
                    },
                )
                .await?;
            let completed = if record.operation_state == ConnectionOperationState::Succeeded {
                record
            } else {
                self.wait(&session.subject.canonical_user_id, record.operation_id)
                    .await?
            };
            return completed.result.ok_or(ConnectionBrokerError::Unavailable);
        }
        let record = self
            .reserve(
                &session.subject.canonical_user_id,
                &session.subject.display_email,
                ConnectionReservation {
                    operation,
                    allow_status_cache: true,
                    allow_result_cache: identity.allow_result_cache,
                    request_body: Some(request_body),
                    identity: ConnectionReservationIdentity {
                        operation: Some(identity.idempotency_identity),
                        client_scope: identity.idempotency_scope,
                        publication_branch: None,
                    },
                },
            )
            .await?;
        let completed = if record.operation_state == ConnectionOperationState::Succeeded {
            record
        } else {
            self.wait(&session.subject.canonical_user_id, record.operation_id)
                .await?
        };
        completed.result.ok_or(ConnectionBrokerError::Unavailable)
    }

    pub(crate) async fn stored_automation_result(
        &self,
        canonical_user_id: &CanonicalUserId,
        operation: ConnectionOperationKind,
        idempotency_identity_prefix: &str,
    ) -> Result<Option<Value>, ConnectionBrokerError> {
        if !matches!(
            operation,
            ConnectionOperationKind::Workflow
                | ConnectionOperationKind::Publish
                | ConnectionOperationKind::Dispatch
        ) {
            return Err(ConnectionBrokerError::Unavailable);
        }
        self.store
            .latest_connection_operation_result(
                canonical_user_id,
                operation.into(),
                idempotency_identity_prefix,
            )
            .await
            .map_err(store_broker_error)
    }

    async fn status_operation(
        &self,
        session: &ConnectionSession<B>,
        allow_cache: bool,
    ) -> Result<ProviderConnectionStatus, ConnectionBrokerError> {
        let record = self
            .reserve(
                &session.subject.canonical_user_id,
                &session.subject.display_email,
                ConnectionReservation {
                    operation: ConnectionOperationKind::Status,
                    allow_status_cache: allow_cache,
                    allow_result_cache: true,
                    request_body: None,
                    identity: ConnectionReservationIdentity::default(),
                },
            )
            .await?;
        let completed = if record.operation_state == ConnectionOperationState::Succeeded {
            record
        } else {
            self.wait(&session.subject.canonical_user_id, record.operation_id)
                .await?
        };
        let status = provider_status(
            completed
                .result
                .as_ref()
                .or(completed.cached_status.as_ref())
                .ok_or(ConnectionBrokerError::Unavailable)?,
        )?;
        if allow_cache && status.phase == ConnectionPhase::Connected {
            self.store
                .complete_pending_connection_oauth_flow(&session.subject.canonical_user_id)
                .await
                .map_err(|_| ConnectionBrokerError::Unavailable)?;
        }
        Ok(status)
    }
}

impl From<ConnectionOperationKind> for StoredOperationKind {
    fn from(value: ConnectionOperationKind) -> Self {
        match value {
            ConnectionOperationKind::Status => Self::Status,
            ConnectionOperationKind::Start => Self::Start,
            ConnectionOperationKind::Disconnect => Self::Disconnect,
            ConnectionOperationKind::Rerun => Self::Rerun,
            ConnectionOperationKind::Repositories => Self::Repositories,
            ConnectionOperationKind::Workflow => Self::Workflow,
            ConnectionOperationKind::RunStatus => Self::RunStatus,
            ConnectionOperationKind::Dispatch => Self::Dispatch,
            ConnectionOperationKind::Publish => Self::Publish,
        }
    }
}

fn start_poll_deadline<'a>(
    operation_state: ConnectionOperationState,
    oauth_phase: ConnectionOAuthPhase,
    flow_expires_at: Option<&'a str>,
    response_deadline_at: &'a str,
) -> &'a str {
    if operation_state == ConnectionOperationState::Succeeded
        && oauth_phase == ConnectionOAuthPhase::Pending
    {
        flow_expires_at.unwrap_or(response_deadline_at)
    } else {
        response_deadline_at
    }
}

impl<B> ProviderConnectionBroker<B> for GovernedConnectionsBroker<B>
where
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    fn status<'a>(
        &'a self,
        session: &'a ConnectionSession<B>,
    ) -> BoxFuture<'a, Result<ProviderConnectionStatus, ConnectionBrokerError>> {
        Box::pin(async move { self.status_operation(session, true).await })
    }

    fn start<'a>(
        &'a self,
        session: &'a ConnectionSession<B>,
    ) -> BoxFuture<'a, Result<ReservedConnectionStart, ConnectionBrokerError>> {
        Box::pin(async move {
            let record = self
                .reserve(
                    &session.subject.canonical_user_id,
                    &session.subject.display_email,
                    ConnectionReservation {
                        operation: ConnectionOperationKind::Start,
                        allow_status_cache: true,
                        allow_result_cache: true,
                        request_body: None,
                        identity: ConnectionReservationIdentity::default(),
                    },
                )
                .await?;
            Ok(ReservedConnectionStart {
                operation_id: record.operation_id,
                poll_deadline_at: start_poll_deadline(
                    record.operation_state,
                    record.oauth_phase,
                    record.flow_expires_at.as_deref(),
                    &record.response_deadline_at,
                )
                .to_owned(),
            })
        })
    }

    fn start_operation<'a>(
        &'a self,
        session: &'a ConnectionSession<B>,
        operation_id: Uuid,
    ) -> BoxFuture<'a, Result<Option<ConnectionStartOperation>, ConnectionBrokerError>> {
        Box::pin(async move {
            let Some(record) = self
                .store
                .connection_operation(operation_id, &session.subject.canonical_user_id)
                .await
                .map_err(|_| ConnectionBrokerError::Unavailable)?
            else {
                return Ok(None);
            };
            if record.provider != "github"
                || !matches!(
                    record.operation_kind,
                    StoredOperationKind::Start | StoredOperationKind::Disconnect
                )
            {
                return Ok(None);
            }
            let operation = match record.operation_state {
                ConnectionOperationState::Queued
                | ConnectionOperationState::Provisioning
                | ConnectionOperationState::Running => ConnectionStartOperation::Pending,
                ConnectionOperationState::Failed => {
                    ConnectionStartOperation::Failed(connection_broker_error(
                        record.failure_category.as_deref(),
                        record.failure_detail.as_ref(),
                    ))
                }
                ConnectionOperationState::Succeeded => match record.operation_kind {
                    StoredOperationKind::Start => {
                        if record.oauth_phase != ConnectionOAuthPhase::Pending {
                            ConnectionStartOperation::Failed(ConnectionBrokerError::Unavailable)
                        } else {
                            match (record.authorization_url, record.flow_expires_at) {
                                (Some(value), Some(expires_at)) => {
                                    match AuthorizationUrl::new(value) {
                                        Ok(authorization_url) => {
                                            ConnectionStartOperation::Succeeded(StartedConnection {
                                                authorization_url,
                                                expires_at,
                                            })
                                        }
                                        Err(_) => ConnectionStartOperation::Failed(
                                            ConnectionBrokerError::Unavailable,
                                        ),
                                    }
                                }
                                _ => ConnectionStartOperation::Failed(
                                    ConnectionBrokerError::Unavailable,
                                ),
                            }
                        }
                    }
                    StoredOperationKind::Disconnect => {
                        if record
                            .result
                            .as_ref()
                            .and_then(Value::as_object)
                            .and_then(|object| object.get("disconnected"))
                            .and_then(Value::as_bool)
                            == Some(true)
                        {
                            ConnectionStartOperation::Disconnected
                        } else {
                            ConnectionStartOperation::Failed(ConnectionBrokerError::Unavailable)
                        }
                    }
                    StoredOperationKind::Status
                    | StoredOperationKind::Rerun
                    | StoredOperationKind::Repositories
                    | StoredOperationKind::Workflow
                    | StoredOperationKind::RunStatus
                    | StoredOperationKind::Dispatch
                    | StoredOperationKind::Publish => {
                        ConnectionStartOperation::Failed(ConnectionBrokerError::Unavailable)
                    }
                },
            };
            Ok(Some(operation))
        })
    }

    fn disconnect<'a>(
        &'a self,
        session: &'a ConnectionSession<B>,
    ) -> BoxFuture<'a, Result<ReservedConnectionStart, ConnectionBrokerError>> {
        Box::pin(async move {
            let record = self
                .reserve(
                    &session.subject.canonical_user_id,
                    &session.subject.display_email,
                    ConnectionReservation {
                        operation: ConnectionOperationKind::Disconnect,
                        allow_status_cache: true,
                        allow_result_cache: true,
                        request_body: None,
                        identity: ConnectionReservationIdentity::default(),
                    },
                )
                .await?;
            Ok(ReservedConnectionStart {
                operation_id: record.operation_id,
                poll_deadline_at: record.response_deadline_at,
            })
        })
    }
}

impl<B> GithubWorkflowRerunBroker<B> for GovernedConnectionsBroker<B>
where
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    fn rerun<'a>(
        &'a self,
        session: &'a ConnectionSession<B>,
        request: &'a GithubWorkflowRerunRequest,
    ) -> BoxFuture<'a, Result<(), ConnectionBrokerError>> {
        Box::pin(async move {
            let body = json!({
                "owner": request.owner,
                "repo": request.repository,
                "runId": request.run_id,
            });
            let idempotency_scope = format!(
                "github-rerun:client:{}",
                secret_digest(&request.idempotency_key)
            );
            let idempotency_identity = format!(
                "{idempotency_scope}:payload:{}",
                secret_digest(&body.to_string())
            );
            let record = self
                .reserve(
                    &session.subject.canonical_user_id,
                    &session.subject.display_email,
                    ConnectionReservation {
                        operation: ConnectionOperationKind::Rerun,
                        allow_status_cache: true,
                        allow_result_cache: true,
                        request_body: Some(body),
                        identity: ConnectionReservationIdentity {
                            operation: Some(&idempotency_identity),
                            client_scope: Some(&idempotency_scope),
                            publication_branch: None,
                        },
                    },
                )
                .await?;
            let completed = if record.operation_state == ConnectionOperationState::Succeeded {
                record
            } else {
                self.wait(&session.subject.canonical_user_id, record.operation_id)
                    .await?
            };
            let dispatched = completed
                .result
                .as_ref()
                .and_then(Value::as_object)
                .and_then(|object| object.get("dispatched"))
                .and_then(Value::as_bool)
                == Some(true);
            if dispatched {
                Ok(())
            } else {
                Err(ConnectionBrokerError::Unavailable)
            }
        })
    }
}

#[derive(Clone)]
pub struct ConnectionOperationReconciler {
    store: PgStore,
    failure_reporter: Arc<dyn Fn(String) + Send + Sync>,
    latency_reporter: Arc<dyn Fn(String) + Send + Sync>,
}

impl ConnectionOperationReconciler {
    pub fn new(store: PgStore) -> Self {
        Self {
            store,
            failure_reporter: Arc::new(|line| eprintln!("{line}")),
            latency_reporter: Arc::new(|line| eprintln!("{line}")),
        }
    }

    pub fn with_failure_reporter(
        mut self,
        reporter: impl Fn(String) + Send + Sync + 'static,
    ) -> Self {
        self.failure_reporter = Arc::new(reporter);
        self
    }

    pub fn with_latency_reporter(
        mut self,
        reporter: impl Fn(String) + Send + Sync + 'static,
    ) -> Self {
        self.latency_reporter = Arc::new(reporter);
        self
    }

    pub async fn run(self) {
        loop {
            if let Err(error) = self.reconcile_once().await {
                eprintln!("connection operation reconcile failed: {error}");
            }
            tokio::time::sleep(StdDuration::from_secs(1)).await;
        }
    }

    async fn fail_operation(
        &self,
        operation_id: Uuid,
        operation_kind: StoredOperationKind,
        failure: &ConnectionOperationFailure,
    ) -> Result<(), StoreError> {
        let detail = failure
            .detail
            .as_ref()
            .map(GithubBridgeFailureDiagnostic::to_value);
        self.store
            .fail_connection_operation(operation_id, failure.category, detail.as_ref())
            .await?;
        (self.failure_reporter)(connection_operation_failure_log_line(operation_id, failure));
        if let Some(timing) = self.store.connection_operation_timing(operation_id).await? {
            (self.latency_reporter)(connection_operation_latency_log_line(
                operation_kind,
                timing,
            ));
        }
        Ok(())
    }

    pub async fn reconcile_once(&self) -> Result<(), StoreError> {
        for operation in self
            .store
            .connection_operations_requiring_reconcile()
            .await?
        {
            let task_failure = if matches!(
                operation.task_phase,
                steward_types::TaskPhase::Failed | steward_types::TaskPhase::Cancelled
            ) {
                let failure_reason = self
                    .store
                    .task(operation.task_uid)
                    .await?
                    .and_then(|task| task.failure_reason);
                let execution_stderr = self
                    .store
                    .connection_operation_failure_stderr(operation.task_uid)
                    .await?;
                connection_operation_failure(failure_reason.as_deref(), execution_stderr.as_deref())
            } else {
                ConnectionOperationFailure {
                    category: "bridge_failed",
                    detail: None,
                }
            };
            if operation.oauth_phase == ConnectionOAuthPhase::Pending {
                let _ = self
                    .store
                    .expire_connection_oauth_flow(operation.operation_id)
                    .await?;
            }
            if let Some(category) = finalized_nonterminal_failure(
                operation.finalized,
                operation.operation_state,
                operation.task_phase,
                task_failure.category,
            ) {
                let failure = if category == task_failure.category {
                    task_failure.clone()
                } else {
                    ConnectionOperationFailure {
                        category,
                        detail: None,
                    }
                };
                self.fail_operation(operation.operation_id, operation.operation_kind, &failure)
                    .await?;
                continue;
            }
            if operation.finalized {
                self.store
                    .reconcile_connection_cleanup_state(operation.operation_id, true)
                    .await?;
                continue;
            }
            if matches!(
                operation.operation_state,
                ConnectionOperationState::Succeeded | ConnectionOperationState::Failed
            ) {
                if self
                    .store
                    .mark_stalled_connection_cleanup(
                        operation.operation_id,
                        CONNECTION_CLEANUP_STALL_SECONDS,
                    )
                    .await?
                {
                    eprintln!(
                        "connection operation cleanup stalled: operation_id={}",
                        operation.operation_id
                    );
                }
                continue;
            }
            if matches!(
                operation.task_phase,
                steward_types::TaskPhase::Failed | steward_types::TaskPhase::Cancelled
            ) {
                self.fail_operation(
                    operation.operation_id,
                    operation.operation_kind,
                    &task_failure,
                )
                .await?;
                continue;
            }
            if self
                .store
                .connection_operation_deadline_elapsed(operation.operation_id)
                .await?
            {
                self.fail_operation(
                    operation.operation_id,
                    operation.operation_kind,
                    &ConnectionOperationFailure {
                        category: "deadline_exceeded",
                        detail: None,
                    },
                )
                .await?;
                continue;
            }
            match operation.task_phase {
                steward_types::TaskPhase::Succeeded => {
                    let result = operation
                        .output_archive
                        .as_deref()
                        .ok_or(StoreError::InvalidConnectionOperation)
                        .and_then(|archive| bridge_result(operation.operation_kind, archive));
                    match result {
                        Ok(result) => {
                            let authorization_url =
                                result.get("authorizationUrl").and_then(Value::as_str);
                            let digest = authorization_url.map(secret_digest);
                            self.store
                                .complete_connection_operation(
                                    operation.operation_id,
                                    &result,
                                    authorization_url,
                                    digest.as_deref(),
                                    ConnectionOperationRetention {
                                        cache_ttl_seconds: CONNECTION_STATUS_CACHE_SECONDS,
                                        result_ttl_seconds: connection_result_ttl_seconds(
                                            operation.operation_kind,
                                        ),
                                        oauth_lifetime_seconds: MCP_GW_OAUTH_STATE_LIFETIME_SECONDS
                                            + MCP_GW_OAUTH_CLOCK_SKEW_SECONDS,
                                    },
                                )
                                .await?;
                            if let Some(timing) = self
                                .store
                                .connection_operation_timing(operation.operation_id)
                                .await?
                            {
                                (self.latency_reporter)(connection_operation_latency_log_line(
                                    operation.operation_kind,
                                    timing,
                                ));
                            }
                        }
                        Err(_) => {
                            self.fail_operation(
                                operation.operation_id,
                                operation.operation_kind,
                                &ConnectionOperationFailure {
                                    category: "invalid_bridge_result",
                                    detail: None,
                                },
                            )
                            .await?;
                        }
                    }
                }
                steward_types::TaskPhase::Failed | steward_types::TaskPhase::Cancelled => {}
                steward_types::TaskPhase::Submitted
                | steward_types::TaskPhase::Parked
                | steward_types::TaskPhase::Queued
                | steward_types::TaskPhase::Running => {}
            }
        }
        Ok(())
    }
}

fn finalized_nonterminal_failure(
    finalized: bool,
    operation_state: ConnectionOperationState,
    task_phase: steward_types::TaskPhase,
    task_failure_category: &'static str,
) -> Option<&'static str> {
    if !finalized
        || matches!(
            operation_state,
            ConnectionOperationState::Succeeded | ConnectionOperationState::Failed
        )
    {
        return None;
    }
    Some(match task_phase {
        steward_types::TaskPhase::Succeeded => "invalid_bridge_result",
        steward_types::TaskPhase::Failed | steward_types::TaskPhase::Cancelled => {
            task_failure_category
        }
        steward_types::TaskPhase::Submitted
        | steward_types::TaskPhase::Parked
        | steward_types::TaskPhase::Queued
        | steward_types::TaskPhase::Running => "bridge_finalized_without_terminal_result",
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ConnectionOperationFailure {
    category: &'static str,
    detail: Option<GithubBridgeFailureDiagnostic>,
}

fn connection_operation_failure(
    failure_reason: Option<&str>,
    execution_stderr: Option<&[u8]>,
) -> ConnectionOperationFailure {
    let category = match failure_reason {
        Some("bridge-runtime-authentication") => "bridge-runtime-authentication",
        Some("bridge-proxy-policy") => "bridge-proxy-policy",
        Some("bridge-runtime-authorization") => "bridge-runtime-authorization",
        Some("bridge-token-grant") => "bridge-token-grant",
        Some("bridge-contract") => "bridge-contract",
        Some("bridge-response-contract") => "bridge-response-contract",
        Some("bridge-gateway-transport") => "bridge-gateway-transport",
        Some("bridge-gateway-status") => "bridge-gateway-status",
        Some("bridge-gateway-body") => "bridge-gateway-body",
        Some("bridge-gateway-unavailable") => "bridge-gateway-unavailable",
        Some("bridge-gateway-http") => "bridge-gateway-http",
        Some("runtime_create_admission_rejected") => "runtime_create_admission_rejected",
        Some("runtime_start_failed") => "runtime_start_failed",
        Some("deadline_exceeded") => "deadline_exceeded",
        _ => "bridge_failed",
    };
    let detail = (category == "bridge-gateway-http")
        .then(|| execution_stderr.and_then(github_bridge_failure_diagnostic))
        .flatten();
    ConnectionOperationFailure { category, detail }
}

fn connection_operation_failure_log_line(
    operation_id: Uuid,
    failure: &ConnectionOperationFailure,
) -> String {
    if let Some(detail) = &failure.detail {
        let code = detail
            .code
            .as_ref()
            .map_or_else(|| "null".to_owned(), |code| json!(code).to_string());
        let reason = detail
            .reason
            .as_ref()
            .map_or_else(|| "null".to_owned(), |reason| json!(reason).to_string());
        format!(
            "connection operation failed: operation_id={operation_id} category={} upstream_status={} code={code} detail={reason}",
            failure.category, detail.status,
        )
    } else {
        format!(
            "connection operation failed: operation_id={operation_id} category={}",
            failure.category
        )
    }
}

fn connection_operation_category_log_line(operation_id: Uuid, category: &str) -> String {
    format!("connection operation failed: operation_id={operation_id} category={category}")
}

fn connection_operation_latency_log_line(
    operation: StoredOperationKind,
    timing: ConnectionOperationTiming,
) -> String {
    format!(
        "connection operation latency: operation_kind={} queue_wait_ms={} attempt_duration_ms={} total_latency_ms={}",
        operation.as_str(),
        timing.queue_wait_ms,
        timing.attempt_duration_ms,
        timing.total_latency_ms,
    )
}

fn connection_broker_error(
    failure_category: Option<&str>,
    failure_detail: Option<&Value>,
) -> ConnectionBrokerError {
    match failure_category {
        Some("bridge-runtime-authentication") => ConnectionBrokerError::RuntimeAuthenticationFailed,
        Some("bridge-proxy-policy") => ConnectionBrokerError::ProxyPolicyDenied,
        Some("bridge-runtime-authorization") => ConnectionBrokerError::ProviderAuthorizationFailed,
        Some("bridge-token-grant") => ConnectionBrokerError::TokenGrantFailed,
        Some("bridge-contract") => ConnectionBrokerError::BridgeContractInvalid,
        Some("bridge-response-contract") => ConnectionBrokerError::ProviderResponseInvalid,
        Some("bridge-gateway-transport") => ConnectionBrokerError::GatewayTransportFailed,
        Some("bridge-gateway-status") => ConnectionBrokerError::GatewayStatusInvalid,
        Some("bridge-gateway-body") => ConnectionBrokerError::GatewayBodyUnavailable,
        Some("bridge-gateway-unavailable") => ConnectionBrokerError::GatewayUnavailable,
        Some("invalid_bridge_result") => ConnectionBrokerError::ProviderResponseInvalid,
        Some("runtime_create_admission_rejected") => ConnectionBrokerError::RuntimeCreateFailed,
        Some("runtime_start_failed") => ConnectionBrokerError::RuntimeStartFailed,
        Some("deadline_exceeded") => ConnectionBrokerError::DeadlineExceeded,
        Some("bridge-gateway-http") => failure_detail
            .and_then(GithubBridgeFailureDiagnostic::from_value)
            .map_or(ConnectionBrokerError::Unavailable, |detail| {
                ConnectionBrokerError::GatewayHttp {
                    status: detail.status,
                    code: detail.code,
                    reason: detail.reason,
                }
            }),
        _ => ConnectionBrokerError::Unavailable,
    }
}

#[cfg(test)]
mod finalized_connection_operation_tests {
    use steward_store::ConnectionOperationState;
    use steward_types::TaskPhase;
    use uuid::Uuid;

    use super::{
        ConnectionOperationFailure, connection_broker_error,
        connection_operation_category_log_line, connection_operation_failure,
        connection_operation_failure_log_line, finalized_nonterminal_failure,
    };

    #[test]
    fn finalized_failed_bridge_terminalizes_its_connection_operation() {
        assert_eq!(
            finalized_nonterminal_failure(
                true,
                ConnectionOperationState::Queued,
                TaskPhase::Failed,
                "bridge_failed",
            ),
            Some("bridge_failed")
        );
        assert_eq!(
            finalized_nonterminal_failure(
                true,
                ConnectionOperationState::Succeeded,
                TaskPhase::Succeeded,
                "bridge_failed",
            ),
            None
        );
        assert_eq!(
            finalized_nonterminal_failure(
                false,
                ConnectionOperationState::Queued,
                TaskPhase::Failed,
                "bridge_failed",
            ),
            None
        );
    }

    #[test]
    fn store_owned_failures_use_the_connection_failure_log_contract() {
        assert_eq!(
            connection_operation_category_log_line(Uuid::nil(), "superseded_by_mutation"),
            "connection operation failed: operation_id=00000000-0000-0000-0000-000000000000 category=superseded_by_mutation"
        );
    }

    #[test]
    fn bridge_denials_remain_bounded_and_actionable_through_operation_failure() {
        for (task_reason, operation_category, broker_error) in [
            (
                "bridge-proxy-policy",
                "bridge-proxy-policy",
                crate::connections::ConnectionBrokerError::ProxyPolicyDenied,
            ),
            (
                "bridge-runtime-authorization",
                "bridge-runtime-authorization",
                crate::connections::ConnectionBrokerError::ProviderAuthorizationFailed,
            ),
            (
                "opaque-agent-failure",
                "bridge_failed",
                crate::connections::ConnectionBrokerError::Unavailable,
            ),
        ] {
            assert_eq!(
                connection_operation_failure(Some(task_reason), None).category,
                operation_category
            );
            assert_eq!(
                connection_broker_error(Some(operation_category), None),
                broker_error
            );
        }
        for (task_reason, operation_category, broker_error) in [
            (
                "bridge-runtime-authentication",
                "bridge-runtime-authentication",
                crate::connections::ConnectionBrokerError::RuntimeAuthenticationFailed,
            ),
            (
                "bridge-token-grant",
                "bridge-token-grant",
                crate::connections::ConnectionBrokerError::TokenGrantFailed,
            ),
            (
                "bridge-contract",
                "bridge-contract",
                crate::connections::ConnectionBrokerError::BridgeContractInvalid,
            ),
            (
                "runtime_create_admission_rejected",
                "runtime_create_admission_rejected",
                crate::connections::ConnectionBrokerError::RuntimeCreateFailed,
            ),
            (
                "runtime_start_failed",
                "runtime_start_failed",
                crate::connections::ConnectionBrokerError::RuntimeStartFailed,
            ),
            (
                "deadline_exceeded",
                "deadline_exceeded",
                crate::connections::ConnectionBrokerError::DeadlineExceeded,
            ),
        ] {
            assert_eq!(
                connection_operation_failure(Some(task_reason), None).category,
                operation_category
            );
            assert_eq!(
                connection_broker_error(Some(operation_category), None),
                broker_error
            );
        }
    }

    #[test]
    fn every_safe_gateway_failure_keeps_its_actionable_category() {
        for (category, broker_error) in [
            (
                "bridge-response-contract",
                crate::connections::ConnectionBrokerError::ProviderResponseInvalid,
            ),
            (
                "bridge-gateway-transport",
                crate::connections::ConnectionBrokerError::GatewayTransportFailed,
            ),
            (
                "bridge-gateway-status",
                crate::connections::ConnectionBrokerError::GatewayStatusInvalid,
            ),
            (
                "bridge-gateway-body",
                crate::connections::ConnectionBrokerError::GatewayBodyUnavailable,
            ),
            (
                "bridge-gateway-unavailable",
                crate::connections::ConnectionBrokerError::GatewayUnavailable,
            ),
        ] {
            assert_eq!(
                connection_operation_failure(Some(category), None).category,
                category,
                "safe bridge failure categories must not collapse to bridge_failed"
            );
            assert_eq!(connection_broker_error(Some(category), None), broker_error);
        }
        assert_eq!(
            connection_broker_error(Some("invalid_bridge_result"), None),
            crate::connections::ConnectionBrokerError::ProviderResponseInvalid,
            "an invalid successful bridge result must remain distinct at the API boundary"
        );
    }

    #[test]
    fn gateway_http_failure_retains_only_the_fixed_adapter_diagnostic() {
        let failure = connection_operation_failure(
            Some("bridge-gateway-http"),
            Some(
                b"steward-connections-bridge: bridge MCP-GW returned HTTP 400 [code=oauth_redirect_target_not_allowed] (OAuth redirect target is not allowed)\n",
            ),
        );
        assert_eq!(failure.category, "bridge-gateway-http");
        let expected_detail = steward_adapter_mcp_gw::GithubBridgeFailureDiagnostic {
            status: 400,
            code: Some("oauth_redirect_target_not_allowed".to_owned()),
            reason: Some("OAuth redirect target is not allowed".to_owned()),
        };
        assert_eq!(failure.detail, Some(expected_detail.clone()));
        let operation_id = Uuid::nil();
        assert_eq!(
            connection_operation_failure_log_line(operation_id, &failure),
            "connection operation failed: operation_id=00000000-0000-0000-0000-000000000000 category=bridge-gateway-http upstream_status=400 code=\"oauth_redirect_target_not_allowed\" detail=\"OAuth redirect target is not allowed\""
        );
        assert_eq!(
            connection_broker_error(Some(failure.category), Some(&expected_detail.to_value()),),
            crate::connections::ConnectionBrokerError::GatewayHttp {
                status: 400,
                code: Some("oauth_redirect_target_not_allowed".to_owned()),
                reason: Some("OAuth redirect target is not allowed".to_owned()),
            }
        );

        assert_eq!(
            connection_operation_failure(
                Some("bridge-gateway-http"),
                Some(b"Authorization: Bearer obviously-fake-secret")
            ),
            ConnectionOperationFailure {
                category: "bridge-gateway-http",
                detail: None,
            },
            "arbitrary stderr must never become a durable or logged failure detail"
        );
    }
}

fn store_broker_error(error: StoreError) -> ConnectionBrokerError {
    match error {
        StoreError::ConnectionOAuthFlowPending => ConnectionBrokerError::OAuthFlowPending,
        StoreError::ConnectionOperationIdempotencyConflict => {
            ConnectionBrokerError::IdempotencyConflict
        }
        _ => ConnectionBrokerError::Unavailable,
    }
}

fn bridge_result(operation: StoredOperationKind, archive: &[u8]) -> Result<Value, StoreError> {
    let body = single_file_payload(archive, "response.json")
        .ok_or(StoreError::InvalidConnectionOperation)?;
    if body.len() > MAX_BRIDGE_RESULT_BYTES {
        return Err(StoreError::InvalidConnectionOperation);
    }
    let value: Value =
        serde_json::from_slice(body).map_err(|_| StoreError::InvalidConnectionOperation)?;
    match operation {
        StoredOperationKind::Status => {
            provider_status(&value).map_err(|_| StoreError::InvalidConnectionOperation)?;
        }
        StoredOperationKind::Start => {
            let Some(object) = value.as_object() else {
                return Err(StoreError::InvalidConnectionOperation);
            };
            if object.len() != 1
                || object
                    .get("authorizationUrl")
                    .and_then(Value::as_str)
                    .and_then(|value| AuthorizationUrl::new(value.to_owned()).ok())
                    .is_none()
            {
                return Err(StoreError::InvalidConnectionOperation);
            }
        }
        StoredOperationKind::Disconnect => {
            if value != json!({"disconnected": true}) {
                return Err(StoreError::InvalidConnectionOperation);
            }
        }
        StoredOperationKind::Rerun => {
            if value != json!({"dispatched": true}) {
                return Err(StoreError::InvalidConnectionOperation);
            }
        }
        StoredOperationKind::Repositories => {
            validate_repositories_result(&value)?;
        }
        StoredOperationKind::Workflow => {
            validate_workflow_result(&value)?;
        }
        StoredOperationKind::RunStatus => {
            validate_run_status_result(&value)?;
        }
        StoredOperationKind::Dispatch => {
            validate_dispatch_result(&value)?;
        }
        StoredOperationKind::Publish => {
            validate_publish_result(&value)?;
        }
    }
    Ok(value)
}

fn exact_object_keys(object: &Map<String, Value>, expected: &[&str]) -> bool {
    object.len() == expected.len() && expected.iter().all(|key| object.contains_key(*key))
}

fn valid_https_github_url(value: &Value) -> bool {
    value.as_str().is_some_and(|value| {
        Url::parse(value).is_ok_and(|url| {
            url.scheme() == "https"
                && url.host_str() == Some("github.com")
                && url.username().is_empty()
                && url.password().is_none()
        })
    })
}

fn validate_repositories_result(value: &Value) -> Result<(), StoreError> {
    let object = value
        .as_object()
        .ok_or(StoreError::InvalidConnectionOperation)?;
    if !exact_object_keys(object, &["login", "repositories", "page", "hasNextPage"])
        || object.get("login").and_then(Value::as_str).is_none()
        || object.get("page").and_then(Value::as_u64).is_none()
        || object.get("hasNextPage").and_then(Value::as_bool).is_none()
    {
        return Err(StoreError::InvalidConnectionOperation);
    }
    let repositories = object
        .get("repositories")
        .and_then(Value::as_array)
        .filter(|repositories| repositories.len() <= 100)
        .ok_or(StoreError::InvalidConnectionOperation)?;
    if repositories.iter().any(|repository| {
        let Some(repository) = repository.as_object() else {
            return true;
        };
        !exact_object_keys(
            repository,
            &[
                "owner",
                "ownerId",
                "name",
                "repositoryId",
                "defaultBranch",
                "private",
                "url",
            ],
        ) || repository.get("owner").and_then(Value::as_str).is_none()
            || repository.get("ownerId").and_then(Value::as_str).is_none()
            || repository.get("name").and_then(Value::as_str).is_none()
            || repository
                .get("repositoryId")
                .and_then(Value::as_str)
                .is_none()
            || repository
                .get("defaultBranch")
                .and_then(Value::as_str)
                .is_none()
            || repository.get("private").and_then(Value::as_bool).is_none()
            || !repository.get("url").is_some_and(valid_https_github_url)
    }) {
        return Err(StoreError::InvalidConnectionOperation);
    }
    Ok(())
}

fn validate_workflow_result(value: &Value) -> Result<(), StoreError> {
    let object = value
        .as_object()
        .ok_or(StoreError::InvalidConnectionOperation)?;
    if !exact_object_keys(object, &["exists", "compatible", "path", "sha"])
        || object.get("exists").and_then(Value::as_bool).is_none()
        || object.get("compatible").and_then(Value::as_bool).is_none()
        || object.get("path").and_then(Value::as_str).is_none()
        || !matches!(
            object.get("sha"),
            Some(Value::String(_)) | Some(Value::Null)
        )
    {
        return Err(StoreError::InvalidConnectionOperation);
    }
    Ok(())
}

fn validate_run_status_result(value: &Value) -> Result<(), StoreError> {
    let object = value
        .as_object()
        .ok_or(StoreError::InvalidConnectionOperation)?;
    if !object.keys().all(|key| {
        matches!(
            key.as_str(),
            "runId" | "runAttempt" | "phase" | "conclusion" | "url" | "jobs" | "failureLog"
        )
    }) || !matches!(object.len(), 6 | 7)
        || object.get("runId").and_then(Value::as_u64).is_none()
        || !object
            .get("runAttempt")
            .and_then(Value::as_u64)
            .is_some_and(|attempt| attempt > 0 && attempt <= u64::from(u32::MAX))
        || !matches!(
            object.get("phase").and_then(Value::as_str),
            Some("queued" | "in_progress" | "completed")
        )
        || !matches!(
            object.get("conclusion"),
            Some(Value::String(_)) | Some(Value::Null)
        )
        || !object.get("url").is_some_and(valid_https_github_url)
        || object.get("jobs").and_then(Value::as_array).is_none()
        || object
            .get("failureLog")
            .is_some_and(|value| !matches!(value, Value::String(_) | Value::Null))
    {
        return Err(StoreError::InvalidConnectionOperation);
    }
    Ok(())
}

fn validate_dispatch_result(value: &Value) -> Result<(), StoreError> {
    let object = value
        .as_object()
        .ok_or(StoreError::InvalidConnectionOperation)?;
    if !exact_object_keys(object, &["runId", "url"])
        || object.get("runId").and_then(Value::as_u64).is_none()
        || !object.get("url").is_some_and(valid_https_github_url)
    {
        return Err(StoreError::InvalidConnectionOperation);
    }
    Ok(())
}

fn validate_publish_result(value: &Value) -> Result<(), StoreError> {
    let object = value
        .as_object()
        .ok_or(StoreError::InvalidConnectionOperation)?;
    if !exact_object_keys(object, &["pullRequestUrl", "pullRequestNumber", "branch"])
        || !object
            .get("pullRequestUrl")
            .is_some_and(valid_https_github_url)
        || object
            .get("pullRequestNumber")
            .and_then(Value::as_u64)
            .is_none()
        || object.get("branch").and_then(Value::as_str).is_none()
    {
        return Err(StoreError::InvalidConnectionOperation);
    }
    Ok(())
}

fn provider_status(value: &Value) -> Result<ProviderConnectionStatus, ConnectionBrokerError> {
    let object = value
        .as_object()
        .ok_or(ConnectionBrokerError::Unavailable)?;
    if !object.keys().all(|key| {
        matches!(
            key.as_str(),
            "phase"
                | "connected"
                | "email"
                | "accountId"
                | "accountLogin"
                | "scopesRequired"
                | "scopesGranted"
                | "missingScopes"
                | "activeCredentialExpiresAt"
                | "renewalCredentialExpiresAt"
        )
    }) {
        return Err(ConnectionBrokerError::Unavailable);
    }
    let connected = object
        .get("connected")
        .and_then(Value::as_bool)
        .ok_or(ConnectionBrokerError::Unavailable)?;
    let email = optional_string(object, "email")?;
    let account_id = optional_string(object, "accountId")?;
    let account_login = optional_string(object, "accountLogin")?;
    if account_id
        .as_deref()
        .is_some_and(|value| github_actions_subject(value).is_none())
    {
        return Err(ConnectionBrokerError::Unavailable);
    }
    let scopes_required = string_array(object, "scopesRequired")?;
    let scopes_granted = string_array(object, "scopesGranted")?;
    let scopes_missing = string_array(object, "missingScopes")?;
    if connected && (email.is_none() || !scopes_missing.is_empty()) {
        return Err(ConnectionBrokerError::Unavailable);
    }
    let phase = match object.get("phase").and_then(Value::as_str) {
        Some("connected") if connected => ConnectionPhase::Connected,
        Some("reauthorization_required") if !connected => ConnectionPhase::ReauthRequired,
        Some("authorizing" | "renewing") if !connected => ConnectionPhase::Connecting,
        Some("unavailable") if !connected => ConnectionPhase::Unavailable,
        Some(
            "disconnected" | "revocation_pending" | "disconnected_with_provider_cleanup_pending",
        ) if !connected => ConnectionPhase::Disconnected,
        None if connected => ConnectionPhase::Connected,
        None if email.is_some() && !scopes_missing.is_empty() => ConnectionPhase::ReauthRequired,
        None => ConnectionPhase::Disconnected,
        _ => return Err(ConnectionBrokerError::Unavailable),
    };
    let active_credential_expires_at = optional_expiry(object, "activeCredentialExpiresAt")?;
    let renewal_credential_expires_at = optional_expiry(object, "renewalCredentialExpiresAt")?;
    Ok(ProviderConnectionStatus {
        phase,
        account_email: email,
        account_id,
        account_login,
        github_actions_identity_linked: None,
        scopes_required,
        scopes_granted,
        scopes_missing,
        expires_at: None,
        active_credential_expires_at,
        renewal_credential_expires_at,
    })
}

fn github_actions_subject(account_id: &str) -> Option<String> {
    (!account_id.is_empty()
        && account_id.len() <= 20
        && account_id.bytes().all(|byte| byte.is_ascii_digit())
        && !account_id.starts_with('0'))
    .then(|| format!("github-actions:actor:{account_id}"))
}

fn connection_association_failure_category(error: &StoreError) -> &'static str {
    match error {
        StoreError::CanonicalIdentityNotFound | StoreError::CanonicalIdentityInactive => {
            "canonical_identity_unavailable"
        }
        StoreError::InvalidFederatedSubject | StoreError::InvalidFederatedSubjectRecord => {
            "invalid_identity_evidence"
        }
        StoreError::Database(_) => "store_unavailable",
        _ => "association_rejected",
    }
}

fn optional_expiry(
    object: &Map<String, Value>,
    key: &str,
) -> Result<Option<String>, ConnectionBrokerError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.is_empty() && value.len() <= 40 => {
            Ok(Some(value.clone()))
        }
        _ => Err(ConnectionBrokerError::Unavailable),
    }
}

fn optional_string(
    object: &Map<String, Value>,
    key: &str,
) -> Result<Option<String>, ConnectionBrokerError> {
    object
        .get(key)
        .map(|value| {
            value
                .as_str()
                .filter(|value| !value.is_empty() && value.len() <= 320)
                .map(str::to_owned)
                .ok_or(ConnectionBrokerError::Unavailable)
        })
        .transpose()
}

fn string_array(
    object: &Map<String, Value>,
    key: &str,
) -> Result<Vec<String>, ConnectionBrokerError> {
    let values = match object.get(key) {
        Some(value) => value
            .as_array()
            .cloned()
            .ok_or(ConnectionBrokerError::Unavailable)?,
        None => Vec::new(),
    };
    let strings = values
        .into_iter()
        .map(|value| {
            value
                .as_str()
                .filter(|value| !value.is_empty() && value.len() <= 128)
                .map(str::to_owned)
                .ok_or(ConnectionBrokerError::Unavailable)
        })
        .collect::<Result<Vec<_>, _>>()?;
    if strings.len() > 32 || strings.iter().collect::<BTreeSet<_>>().len() != strings.len() {
        return Err(ConnectionBrokerError::Unavailable);
    }
    Ok(strings)
}

fn secret_digest(value: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(value.as_bytes()))
}

fn single_file_archive(name: &str, body: &[u8]) -> Result<Vec<u8>, ConnectionBrokerError> {
    if name.is_empty() || name.len() > 100 || body.len() > MAX_BRIDGE_RESULT_BYTES {
        return Err(ConnectionBrokerError::Unavailable);
    }
    let mut header = vec![0_u8; TAR_BLOCK_BYTES];
    header[..name.len()].copy_from_slice(name.as_bytes());
    header[100..108].copy_from_slice(b"0000644\0");
    let size = format!("{:011o}\0", body.len());
    header[124..136].copy_from_slice(size.as_bytes());
    header[148..156].fill(b' ');
    header[156] = b'0';
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    let checksum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
    let checksum = format!("{checksum:06o}\0 ");
    header[148..156].copy_from_slice(checksum.as_bytes());
    let mut archive = header;
    archive.extend_from_slice(body);
    archive.resize(archive.len().div_ceil(TAR_BLOCK_BYTES) * TAR_BLOCK_BYTES, 0);
    archive.extend_from_slice(&[0; TAR_BLOCK_BYTES * 2]);
    Ok(archive)
}

fn single_file_payload<'a>(archive: &'a [u8], expected: &str) -> Option<&'a [u8]> {
    if archive.len() < TAR_BLOCK_BYTES * 3 || !archive.len().is_multiple_of(TAR_BLOCK_BYTES) {
        return None;
    }
    let header = &archive[..TAR_BLOCK_BYTES];
    if tar_checksum(header).is_none()
        || tar_string(&header[..100]) != Some(expected)
        || !matches!(header[156], 0 | b'0')
        || header[157..257].iter().any(|byte| *byte != 0)
        || header[345..500].iter().any(|byte| *byte != 0)
    {
        return None;
    }
    let size = tar_octal(&header[124..136])?;
    let body_end = TAR_BLOCK_BYTES.checked_add(size)?;
    let padded_end = body_end
        .checked_add(TAR_BLOCK_BYTES - 1)?
        .checked_div(TAR_BLOCK_BYTES)?
        .checked_mul(TAR_BLOCK_BYTES)?;
    if padded_end > archive.len().checked_sub(TAR_BLOCK_BYTES * 2)?
        || archive[padded_end..].iter().any(|byte| *byte != 0)
    {
        return None;
    }
    Some(&archive[TAR_BLOCK_BYTES..body_end])
}

fn tar_checksum(header: &[u8]) -> Option<()> {
    let expected = tar_octal(header.get(148..156)?)?;
    let actual = header
        .iter()
        .enumerate()
        .map(|(index, byte)| {
            u32::from(if (148..156).contains(&index) {
                b' '
            } else {
                *byte
            })
        })
        .sum::<u32>();
    (usize::try_from(actual).ok()? == expected).then_some(())
}

fn tar_octal(field: &[u8]) -> Option<usize> {
    let field = field
        .strip_prefix(b" ")
        .unwrap_or(field)
        .split(|byte| *byte == 0 || *byte == b' ')
        .next()
        .filter(|value| !value.is_empty())?;
    field.iter().try_fold(0_usize, |value, byte| {
        byte.is_ascii_digit()
            .then_some(())
            .filter(|_| *byte <= b'7')?;
        value.checked_mul(8)?.checked_add(usize::from(*byte - b'0'))
    })
}

fn tar_string(field: &[u8]) -> Option<&str> {
    let end = field
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(field.len());
    std::str::from_utf8(&field[..end]).ok()
}

pub fn plan_connection_operation(
    canonical_user_id: &CanonicalUserId,
    display_email: &Email,
    operation: ConnectionOperationKind,
    bindings: ConnectionExecutionBindings,
) -> Result<GovernedConnectionPlan, GovernedConnectionPlanError> {
    bindings.validate()?;
    let (authority, authority_version, authority_digest) =
        connection_authority(&bindings.mcp_gw_version)?;
    let spec = AgentRuntimeSpec {
        principal: Principal::Service {
            name: CONNECTIONS_SERVICE.to_owned(),
            acting_user: Some(display_email.clone()),
        },
        owner: display_email.clone(),
        canonical_authority: Some(
            CanonicalAuthorityBinding::new(
                canonical_user_id.clone(),
                Some(canonical_user_id.clone()),
            )
            .map_err(|_| GovernedConnectionPlanError::Admission)?,
        ),
        agent_type: AgentType {
            name: "connections-bridge".to_owned(),
        },
        llms: Vec::new(),
        tools: operation_grants(operation)?,
        budget: authority.spec.budget.clone(),
        ttl: authority.spec.ttl.clone(),
        runner: authority.spec.runner.clone(),
        bindings: None,
    };
    if evaluate(&spec, &authority).map_err(|_| GovernedConnectionPlanError::Admission)?
        != AdmissionDecision::Admit
    {
        return Err(GovernedConnectionPlanError::Admission);
    }
    Ok(GovernedConnectionPlan {
        spec,
        command: vec![
            CONNECTIONS_BRIDGE_BINARY.to_owned(),
            "--operation".to_owned(),
            operation.bridge_operation().to_owned(),
            "--input".to_owned(),
            "request.json".to_owned(),
        ],
        authority_id: CONNECTIONS_SERVICE,
        authority_version,
        authority_digest: authority_digest.to_owned(),
        bindings,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use sha2::{Digest, Sha256};
    use steward_admission::{AdmissionDecision, evaluate};
    use steward_types::{CanonicalUserId, Email, Principal, ToolGrant};
    use uuid::Uuid;

    use crate::BoxFuture;
    use crate::connections::{
        ConnectionBrokerError, ConnectionPhase, ConnectionSession, ConnectionStartOperation,
        ConnectionSubject, ProviderConnectionBroker, ProviderConnectionStatus,
        ReservedConnectionStart,
    };

    use super::{
        CONNECTION_MUTATION_RESULT_SECONDS, CONNECTION_RESPONSE_DEADLINE_SECONDS,
        CONNECTIONS_AUTHORITY_DIGEST, CONNECTIONS_AUTHORITY_DOCUMENT,
        CONNECTIONS_AUTHORITY_VERSION, CONNECTIONS_SERVICE, ConnectionExecutionBindings,
        ConnectionOperationKind, GITHUB_ATTESTATION_TRUST_MODE, GITHUB_REPOSITORY_CACHE_SECONDS,
        GITHUB_RERUN_RESULT_SECONDS, GovernedConnectionPlanError, MCP_GW_CONTRACT_VERSION,
        MCP_GW_OAUTH_CLOCK_SKEW_SECONDS, MCP_GW_OAUTH_STATE_LIFETIME_SECONDS,
        OPERATOR_PINNED_TRUST_MODE, ProviderConnectionStatusSource, SplitConnectionsBroker,
        bridge_result, connection_operation_latency_log_line, connection_orchestration_error,
        connection_result_ttl_seconds, connections_startup_warning, plan_connection_operation,
        provider_status, single_file_archive, start_poll_deadline, valid_operator_pinned_image,
    };
    use steward_store::{ConnectionOAuthPhase, ConnectionOperationState};

    #[derive(Clone)]
    struct RejectingMutations {
        status_calls: Arc<AtomicUsize>,
    }

    #[test]
    fn repository_reads_have_a_sixty_second_result_cache() {
        assert_eq!(
            connection_result_ttl_seconds(steward_store::ConnectionOperationKind::Repositories),
            GITHUB_REPOSITORY_CACHE_SECONDS
        );
        assert_eq!(
            connection_result_ttl_seconds(steward_store::ConnectionOperationKind::Rerun),
            GITHUB_RERUN_RESULT_SECONDS
        );
        assert_eq!(
            connection_result_ttl_seconds(steward_store::ConnectionOperationKind::Dispatch),
            CONNECTION_MUTATION_RESULT_SECONDS
        );
    }

    #[test]
    fn operation_latency_log_is_labelled_and_machine_readable() {
        assert_eq!(
            connection_operation_latency_log_line(
                steward_store::ConnectionOperationKind::Repositories,
                steward_store::ConnectionOperationTiming {
                    queue_wait_ms: 1700,
                    attempt_duration_ms: 7000,
                    total_latency_ms: 8900,
                },
            ),
            "connection operation latency: operation_kind=repositories queue_wait_ms=1700 attempt_duration_ms=7000 total_latency_ms=8900"
        );
    }

    impl ProviderConnectionBroker<String> for RejectingMutations {
        fn status<'a>(
            &'a self,
            _session: &'a ConnectionSession<String>,
        ) -> BoxFuture<'a, Result<ProviderConnectionStatus, ConnectionBrokerError>> {
            self.status_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(ConnectionBrokerError::Unavailable) })
        }

        fn start<'a>(
            &'a self,
            _session: &'a ConnectionSession<String>,
        ) -> BoxFuture<'a, Result<ReservedConnectionStart, ConnectionBrokerError>> {
            Box::pin(async { Err(ConnectionBrokerError::Unavailable) })
        }

        fn start_operation<'a>(
            &'a self,
            _session: &'a ConnectionSession<String>,
            _operation_id: Uuid,
        ) -> BoxFuture<'a, Result<Option<ConnectionStartOperation>, ConnectionBrokerError>>
        {
            Box::pin(async { Err(ConnectionBrokerError::Unavailable) })
        }

        fn disconnect<'a>(
            &'a self,
            _session: &'a ConnectionSession<String>,
        ) -> BoxFuture<'a, Result<ReservedConnectionStart, ConnectionBrokerError>> {
            Box::pin(async { Err(ConnectionBrokerError::Unavailable) })
        }
    }

    #[derive(Clone)]
    struct FixedStatus;

    impl ProviderConnectionStatusSource<String> for FixedStatus {
        fn status<'a>(
            &'a self,
            _session: &'a ConnectionSession<String>,
        ) -> BoxFuture<'a, Result<ProviderConnectionStatus, ConnectionBrokerError>> {
            Box::pin(async {
                Ok(ProviderConnectionStatus {
                    phase: ConnectionPhase::Disconnected,
                    account_email: None,
                    account_id: None,
                    account_login: None,
                    github_actions_identity_linked: None,
                    scopes_required: Vec::new(),
                    scopes_granted: Vec::new(),
                    scopes_missing: Vec::new(),
                    expires_at: None,
                    active_credential_expires_at: None,
                    renewal_credential_expires_at: None,
                })
            })
        }
    }

    #[tokio::test]
    async fn browser_status_never_enters_the_governed_mutation_broker() -> Result<(), String> {
        let calls = Arc::new(AtomicUsize::new(0));
        let broker = SplitConnectionsBroker::new(
            RejectingMutations {
                status_calls: calls.clone(),
            },
            FixedStatus,
        );
        let session = ConnectionSession {
            subject: ConnectionSubject {
                canonical_user_id: CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?,
                display_email: "alice@example.com".to_owned(),
            },
            binding: "browser-session".to_owned(),
        };

        let status = broker
            .status(&session)
            .await
            .map_err(|error| format!("read split status: {error:?}"))?;

        assert_eq!(status.phase, ConnectionPhase::Disconnected);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "status must not reserve a governed connection operation, Task, or AgentRuntime"
        );
        Ok(())
    }

    #[test]
    fn staged_orchestration_refuses_mutations_with_an_actionable_reason() {
        assert_eq!(
            connection_orchestration_error(steward_store::TaskOrchestrationMode::Staged),
            Some(ConnectionBrokerError::OrchestrationNotActive)
        );
        assert_eq!(
            connection_orchestration_error(steward_store::TaskOrchestrationMode::Active),
            None
        );
    }

    #[test]
    fn staged_connections_have_one_bounded_startup_warning() {
        assert_eq!(
            connections_startup_warning(steward_store::TaskOrchestrationMode::Staged),
            Some(super::STAGED_CONNECTIONS_WARNING)
        );
        assert_eq!(
            connections_startup_warning(steward_store::TaskOrchestrationMode::Active),
            None
        );
    }

    fn bindings() -> ConnectionExecutionBindings {
        ConnectionExecutionBindings {
            artifact_trust_mode: GITHUB_ATTESTATION_TRUST_MODE.to_owned(),
            bridge_image_digest:
                "ghcr.io/example-org/steward-connections-bridge@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                    .to_owned(),
            mcp_gw_origin: "https://mcp-gw.example.test".to_owned(),
            mcp_gw_version: super::MCP_GW_CONTRACT_VERSION.to_owned(),
            namespace: "steward-test".to_owned(),
            runtime_class: "sandbox-vm".to_owned(),
        }
    }

    #[test]
    fn operator_pinned_image_reference_is_exact_and_tag_free() {
        let digest = "a".repeat(64);
        assert!(valid_operator_pinned_image(&format!(
            "registry.example.test:5000/team/bridge@sha256:{digest}"
        )));
        for invalid in [
            "registry.example.test/team/bridge:latest".to_owned(),
            format!(
                "registry.example.test/team/bridge@sha256:{}",
                "0".repeat(64)
            ),
            format!("registry.example.test/team/bridge:tag@sha256:{digest}"),
            format!("registry.example.test/team:5000/bridge@sha256:{digest}"),
            format!("registry.example.test/team/bridge@@sha256:{digest}"),
            format!("registry.example.test//team/bridge@sha256:{digest}"),
            format!("https://registry.example.test/team/bridge@sha256:{digest}"),
            format!("registry.example.test/team/bridge @sha256:{digest}"),
            format!(
                "registry.example.test/team/bridge@sha256:{}",
                "A".repeat(64)
            ),
            format!(
                "registry.example.test/team/bridge@sha256:{}",
                "a".repeat(63)
            ),
        ] {
            assert!(
                !valid_operator_pinned_image(&invalid),
                "operator-pinned validation accepted {invalid:?}"
            );
        }
    }

    #[test]
    fn reused_succeeded_start_polls_until_flow_expiry_after_response_deadline() {
        let elapsed_response_deadline = "2026-09-01T12:00:30Z";
        let pending_flow_expiry = "2026-09-01T12:10:30Z";

        assert_eq!(
            start_poll_deadline(
                ConnectionOperationState::Succeeded,
                ConnectionOAuthPhase::Pending,
                Some(pending_flow_expiry),
                elapsed_response_deadline,
            ),
            pending_flow_expiry
        );
        assert_eq!(
            start_poll_deadline(
                ConnectionOperationState::Running,
                ConnectionOAuthPhase::None,
                None,
                elapsed_response_deadline,
            ),
            elapsed_response_deadline,
            "an active start must retain its bounded runtime response deadline"
        );
    }

    #[test]
    fn trust_mode_is_explicit_and_operator_pinned_uses_the_strict_reference_contract() {
        let mut candidate = bindings();
        candidate.artifact_trust_mode = OPERATOR_PINNED_TRUST_MODE.to_owned();
        candidate.bridge_image_digest = format!(
            "registry.example.test:5000/team/bridge@sha256:{}",
            "a".repeat(64)
        );
        assert_eq!(candidate.validate(), Ok(()));

        candidate.bridge_image_digest = format!(
            "registry.example.test/team/bridge:tag@sha256:{}",
            "a".repeat(64)
        );
        assert_eq!(
            candidate.validate(),
            Err(GovernedConnectionPlanError::InvalidBindings)
        );

        candidate.artifact_trust_mode = "implicit-or-unknown".to_owned();
        candidate.bridge_image_digest = format!(
            "registry.example.test/team/bridge@sha256:{}",
            "a".repeat(64)
        );
        assert_eq!(
            candidate.validate(),
            Err(GovernedConnectionPlanError::InvalidBindings)
        );
    }

    #[test]
    fn governed_connection_operations_may_use_the_openshell_default_runtime() {
        let mut candidate = bindings();
        candidate.runtime_class.clear();

        assert_eq!(candidate.validate(), Ok(()));
    }

    #[test]
    fn governed_status_uses_canonical_mint_authority_and_no_inference() -> Result<(), String> {
        let user = CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?;
        let email = Email::parse("alice@example.com")?;
        let plan =
            plan_connection_operation(&user, &email, ConnectionOperationKind::Status, bindings())
                .map_err(|error| format!("plan governed status operation: {error:?}"))?;

        assert_eq!(plan.authority_id, CONNECTIONS_SERVICE);
        assert_eq!(plan.authority_version, 1);
        assert!(plan.authority_digest.starts_with("sha256:"));
        assert_eq!(plan.spec.llms, []);
        assert_eq!(plan.spec.tools.len(), 1);
        assert_eq!(plan.spec.tools[0].provider, "github");
        assert_eq!(plan.spec.tools[0].resource, "provider-control");
        assert_eq!(plan.spec.tools[0].action, "status");
        assert_eq!(
            plan.spec.principal,
            Principal::Service {
                name: CONNECTIONS_SERVICE.to_owned(),
                acting_user: Some(email.clone()),
            }
        );
        let canonical = plan
            .spec
            .canonical_authority
            .as_ref()
            .ok_or_else(|| "bridge plan omitted canonical authority".to_owned())?;
        assert_eq!(canonical.owner_user_id, user);
        assert_eq!(canonical.acting_user_id.as_ref(), Some(&user));
        assert_eq!(plan.spec.owner, email);
        assert_eq!(plan.spec.agent_type.name, "connections-bridge");
        assert_eq!(
            plan.command,
            [
                "/usr/local/bin/steward-connections-bridge",
                "--operation",
                "github.status",
                "--input",
                "request.json",
            ]
        );
        assert_eq!(
            evaluate(
                &plan.spec,
                &steward_admission::internal_authorities::steward_connections_v1::envelope()
            )
            .map_err(|error| format!("evaluate internal authority: {error:?}"))?,
            AdmissionDecision::Admit,
            "the fixed bridge plan must pass the same admission library as agent runtimes"
        );
        Ok(())
    }

    #[test]
    fn every_operation_uses_one_exact_admitted_provider_control_grant() -> Result<(), String> {
        let user = CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?;
        let email = Email::parse("alice@example.com")?;
        for (operation, action, bridge_operation) in [
            (ConnectionOperationKind::Status, "status", "github.status"),
            (ConnectionOperationKind::Start, "start", "github.start"),
            (
                ConnectionOperationKind::Disconnect,
                "disconnect",
                "github.disconnect",
            ),
        ] {
            let plan = plan_connection_operation(&user, &email, operation, bindings())
                .map_err(|error| format!("plan {action}: {error:?}"))?;
            assert!(plan.spec.llms.is_empty());
            assert_eq!(plan.spec.tools.len(), 1);
            assert_eq!(plan.spec.tools[0].provider, "github");
            assert_eq!(plan.spec.tools[0].resource, "provider-control");
            assert_eq!(plan.spec.tools[0].action, action);
            assert_eq!(plan.command[2], bridge_operation);
            assert_eq!(
                evaluate(
                    &plan.spec,
                    &steward_admission::internal_authorities::steward_connections_v1::envelope()
                )
                .map_err(|error| format!("evaluate {action}: {error:?}"))?,
                AdmissionDecision::Admit
            );

            let mut ordinary_tool = plan.spec;
            ordinary_tool.tools[0].resource = "repository".to_owned();
            ordinary_tool.tools[0].action = "get_file_contents".to_owned();
            assert_ne!(
                evaluate(
                    &ordinary_tool,
                    &steward_admission::internal_authorities::steward_connections_v1::envelope()
                )
                .map_err(|error| format!("evaluate ordinary tool: {error:?}"))?,
                AdmissionDecision::Admit,
                "provider-control authority must never authorize an ordinary GitHub MCP tool"
            );
        }
        Ok(())
    }

    #[test]
    fn display_email_changes_never_change_the_canonical_credential_owner() -> Result<(), String> {
        let user = CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?;
        let first = plan_connection_operation(
            &user,
            &Email::parse("alice@example.com")?,
            ConnectionOperationKind::Status,
            bindings(),
        )
        .map_err(|error| format!("plan first display email: {error:?}"))?;
        let renamed = plan_connection_operation(
            &user,
            &Email::parse("alice-renamed@example.com")?,
            ConnectionOperationKind::Status,
            bindings(),
        )
        .map_err(|error| format!("plan renamed display email: {error:?}"))?;
        assert_ne!(first.spec.principal, renamed.spec.principal);
        assert_eq!(
            first.spec.canonical_authority,
            renamed.spec.canonical_authority
        );
        assert_eq!(
            first
                .spec
                .canonical_authority
                .as_ref()
                .map(|authority| authority.owner_user_id.as_str()),
            Some(user.as_str())
        );
        Ok(())
    }

    #[test]
    fn authority_document_and_upstream_oauth_contract_are_exactly_pinned() -> Result<(), String> {
        assert_eq!(CONNECTIONS_AUTHORITY_VERSION, 1);
        assert_eq!(MCP_GW_CONTRACT_VERSION, "0.3.2");
        assert_eq!(MCP_GW_OAUTH_STATE_LIFETIME_SECONDS, 600);
        assert_eq!(MCP_GW_OAUTH_CLOCK_SKEW_SECONDS, 30);
        assert_eq!(CONNECTION_RESPONSE_DEADLINE_SECONDS, 40);
        assert_eq!(
            format!(
                "sha256:{:x}",
                Sha256::digest(CONNECTIONS_AUTHORITY_DOCUMENT.as_bytes())
            ),
            CONNECTIONS_AUTHORITY_DIGEST
        );
        let document: serde_json::Value = serde_json::from_str(CONNECTIONS_AUTHORITY_DOCUMENT)
            .map_err(|error| format!("fixed authority JSON is invalid: {error}"))?;
        assert_eq!(document["oauthContract"]["mcpGwVersion"], "0.3.2");
        assert_eq!(document["oauthContract"]["stateLifetimeSeconds"], 600);
        assert_eq!(document["oauthContract"]["clockSkewSeconds"], 30);
        assert_eq!(document["execution"]["responseDeadlineSeconds"], 40);
        Ok(())
    }

    #[test]
    fn lifecycle_gateway_selects_immutable_v4_authority() -> Result<(), String> {
        let mut lifecycle_bindings = bindings();
        lifecycle_bindings.mcp_gw_version = "0.4.9".to_owned();
        let plan = plan_connection_operation(
            &CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")
                .map_err(|error| format!("user: {error:?}"))?,
            &Email::parse("alice@example.com").map_err(|error| format!("email: {error:?}"))?,
            ConnectionOperationKind::Status,
            lifecycle_bindings,
        )
        .map_err(|error| format!("plan: {error:?}"))?;
        let document = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../config/internal-authorities/steward-connections/v4.json"
        ));
        assert_eq!(plan.authority_version, 4);
        assert_eq!(
            plan.authority_digest,
            format!("sha256:{:x}", Sha256::digest(document.as_bytes()))
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(document)
                .map_err(|error| error.to_string())?["oauthContract"]["mcpGwVersion"],
            "0.4.9"
        );
        Ok(())
    }

    #[test]
    fn github_automation_v4_authority_document_has_the_exact_allowlist() -> Result<(), String> {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../config/internal-authorities/steward-connections/v4.json"
        );
        let document = std::fs::read_to_string(path)
            .map_err(|error| format!("read immutable v4 authority document: {error}"))?;
        let value: serde_json::Value = serde_json::from_str(&document)
            .map_err(|error| format!("fixed v4 authority JSON is invalid: {error}"))?;
        let tools = value["tools"]
            .as_array()
            .ok_or_else(|| "v4 authority tools must be an array".to_owned())?;
        let grants = tools
            .iter()
            .map(|tool| {
                Ok((
                    tool["resource"]
                        .as_str()
                        .ok_or_else(|| "tool resource must be a string".to_owned())?,
                    tool["action"]
                        .as_str()
                        .ok_or_else(|| "tool action must be a string".to_owned())?,
                ))
            })
            .collect::<Result<Vec<_>, String>>()?;
        assert_eq!(value["authorityVersion"], 4);
        assert_eq!(
            grants,
            [
                ("provider-control", "status"),
                ("provider-control", "start"),
                ("provider-control", "disconnect"),
                ("actions_run_trigger", "write"),
                ("search_repositories", "read"),
                ("get_me", "read"),
                ("get_file_contents", "read"),
                ("actions_list", "read"),
                ("actions_get", "read"),
                ("get_job_logs", "read"),
                ("get_commit", "read"),
                ("list_pull_requests", "read"),
                ("create_branch", "write"),
                ("push_files", "write"),
                ("create_pull_request", "write"),
            ]
        );
        Ok(())
    }

    #[test]
    fn github_rerun_has_only_the_exact_actions_write_grant() -> Result<(), String> {
        let mut lifecycle_bindings = bindings();
        lifecycle_bindings.mcp_gw_version = "0.4.9".to_owned();
        let plan = plan_connection_operation(
            &CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?,
            &Email::parse("alice@example.com")?,
            ConnectionOperationKind::Rerun,
            lifecycle_bindings,
        )
        .map_err(|error| format!("plan rerun: {error:?}"))?;
        assert!(plan.spec.llms.is_empty());
        assert_eq!(
            plan.spec.tools,
            [ToolGrant {
                provider: "github".to_owned(),
                resource: "actions_run_trigger".to_owned(),
                action: "write".to_owned(),
            }]
        );
        assert_eq!(plan.command[2], "github.rerun");
        assert_eq!(
            evaluate(
                &plan.spec,
                &steward_admission::internal_authorities::steward_connections_v3::envelope()
            )
            .map_err(|error| format!("evaluate rerun: {error:?}"))?,
            AdmissionDecision::Admit
        );
        Ok(())
    }

    #[test]
    fn github_automation_operations_receive_only_their_exact_v4_grants() -> Result<(), String> {
        let user = CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?;
        let email = Email::parse("alice@example.com")?;
        let expected = [
            (
                ConnectionOperationKind::Repositories,
                "github.repositories",
                vec![("search_repositories", "read"), ("get_me", "read")],
            ),
            (
                ConnectionOperationKind::Workflow,
                "github.workflow",
                vec![("get_file_contents", "read")],
            ),
            (
                ConnectionOperationKind::RunStatus,
                "github.run-status",
                vec![
                    ("actions_list", "read"),
                    ("actions_get", "read"),
                    ("get_job_logs", "read"),
                ],
            ),
            (
                ConnectionOperationKind::Dispatch,
                "github.dispatch",
                vec![
                    ("get_file_contents", "read"),
                    ("actions_run_trigger", "write"),
                    ("actions_list", "read"),
                ],
            ),
            (
                ConnectionOperationKind::Publish,
                "github.publish",
                vec![
                    ("get_commit", "read"),
                    ("get_file_contents", "read"),
                    ("list_pull_requests", "read"),
                    ("create_branch", "write"),
                    ("push_files", "write"),
                    ("create_pull_request", "write"),
                ],
            ),
        ];
        for (operation, command, grants) in expected {
            let mut lifecycle_bindings = bindings();
            lifecycle_bindings.mcp_gw_version = "0.4.9".to_owned();
            let plan = plan_connection_operation(&user, &email, operation, lifecycle_bindings)
                .map_err(|error| format!("plan {command}: {error:?}"))?;
            assert_eq!(plan.authority_version, 4);
            assert_eq!(plan.command[2], command);
            assert_eq!(
                plan.spec.tools,
                grants
                    .into_iter()
                    .map(|(resource, action)| ToolGrant {
                        provider: "github".to_owned(),
                        resource: resource.to_owned(),
                        action: action.to_owned(),
                    })
                    .collect::<Vec<_>>()
            );
        }
        Ok(())
    }

    #[test]
    fn incompatible_gateway_contract_version_fails_closed() {
        let mut incompatible = bindings();
        incompatible.mcp_gw_version = "0.3.1".to_owned();
        assert_eq!(
            incompatible.validate(),
            Err(GovernedConnectionPlanError::InvalidBindings)
        );
    }

    #[test]
    fn normalized_status_retains_precise_credential_expiries() -> Result<(), String> {
        let status = provider_status(&serde_json::json!({
            "phase": "connected",
            "connected": true,
            "email": "alice@example.com",
            "accountId": "123456",
            "accountLogin": "mutable-login",
            "scopesRequired": ["repo"],
            "scopesGranted": ["repo"],
            "missingScopes": [],
            "activeCredentialExpiresAt": "2026-09-15T12:00:00.000Z",
            "renewalCredentialExpiresAt": "2026-09-16T12:00:00.000Z"
        }))
        .map_err(|error| format!("normalized status was rejected: {error:?}"))?;
        assert_eq!(
            status.active_credential_expires_at.as_deref(),
            Some("2026-09-15T12:00:00.000Z")
        );
        assert_eq!(
            status.renewal_credential_expires_at.as_deref(),
            Some("2026-09-16T12:00:00.000Z")
        );
        assert_eq!(status.account_id.as_deref(), Some("123456"));
        assert_eq!(status.account_login.as_deref(), Some("mutable-login"));
        assert!(status.github_actions_identity_linked.is_none());
        for invalid in ["0", "012345", "123456789012345678901", "123x"] {
            let invalid_status = serde_json::json!({
                "phase": "connected",
                "connected": true,
                "email": "alice@example.com",
                "accountId": invalid,
                "accountLogin": "mutable-login",
                "scopesRequired": ["repo"],
                "scopesGranted": ["repo"],
                "missingScopes": [],
                "activeCredentialExpiresAt": null,
                "renewalCredentialExpiresAt": null
            });
            assert!(
                provider_status(&invalid_status).is_err(),
                "invalid GitHub account ID {invalid} must not become identity evidence"
            );
        }
        Ok(())
    }

    #[test]
    fn bridge_results_accept_only_the_exact_bounded_operation_schema() -> Result<(), String> {
        let status = single_file_archive("response.json", br#"{"connected":false}"#)
            .map_err(|error| format!("archive status: {error:?}"))?;
        assert!(bridge_result(steward_store::ConnectionOperationKind::Status, &status).is_ok());
        let ordinary_tool = single_file_archive(
            "response.json",
            br#"{"connected":false,"toolResult":{"contents":"hidden"}}"#,
        )
        .map_err(|error| format!("archive ordinary tool result: {error:?}"))?;
        assert!(
            bridge_result(
                steward_store::ConnectionOperationKind::Status,
                &ordinary_tool
            )
            .is_err()
        );
        let wrong_file = single_file_archive("other.json", br#"{"connected":false}"#)
            .map_err(|error| format!("archive wrong file: {error:?}"))?;
        assert!(
            bridge_result(steward_store::ConnectionOperationKind::Status, &wrong_file).is_err()
        );
        let rerun = single_file_archive("response.json", br#"{"dispatched":true}"#)
            .map_err(|error| format!("archive rerun: {error:?}"))?;
        assert!(bridge_result(steward_store::ConnectionOperationKind::Rerun, &rerun).is_ok());
        let leaked = single_file_archive(
            "response.json",
            br#"{"dispatched":true,"providerResponse":{"secret":"hidden"}}"#,
        )
        .map_err(|error| format!("archive leaked rerun: {error:?}"))?;
        assert!(bridge_result(steward_store::ConnectionOperationKind::Rerun, &leaked).is_err());
        for (kind, body) in [
            (
                steward_store::ConnectionOperationKind::Repositories,
                br#"{"login":"alice","repositories":[],"page":1,"hasNextPage":false}"#.as_slice(),
            ),
            (
                steward_store::ConnectionOperationKind::Workflow,
                br#"{"path":".github/workflows/steward-task.yml","exists":false,"compatible":false,"sha":null}"#.as_slice(),
            ),
            (
                steward_store::ConnectionOperationKind::RunStatus,
                br#"{"runId":12345,"runAttempt":1,"phase":"completed","conclusion":"success","url":"https://github.com/example-org/example-repo/actions/runs/12345","jobs":[]}"#.as_slice(),
            ),
            (
                steward_store::ConnectionOperationKind::Dispatch,
                br#"{"runId":12345,"url":"https://github.com/example-org/example-repo/actions/runs/12345"}"#.as_slice(),
            ),
            (
                steward_store::ConnectionOperationKind::Publish,
                br#"{"pullRequestUrl":"https://github.com/example-org/example-repo/pull/1","pullRequestNumber":1,"branch":"steward/task-00000000000000000000000000000001"}"#.as_slice(),
            ),
        ] {
            let archive = single_file_archive("response.json", body)
                .map_err(|error| format!("archive {kind:?}: {error:?}"))?;
            assert!(bridge_result(kind, &archive).is_ok(), "{kind:?}");
            let mut leaked: serde_json::Value =
                serde_json::from_slice(body).map_err(|error| error.to_string())?;
            leaked["providerResponse"] = serde_json::json!({"secret": "hidden"});
            let leaked = single_file_archive("response.json", leaked.to_string().as_bytes())
                .map_err(|error| format!("archive leaked {kind:?}: {error:?}"))?;
            assert!(bridge_result(kind, &leaked).is_err(), "{kind:?}");
        }
        Ok(())
    }

    #[test]
    fn bridge_request_archive_is_readable_by_the_task_execution_identity() -> Result<(), String> {
        let archive = single_file_archive("request.json", br#"{}"#)
            .map_err(|error| format!("archive bridge request: {error:?}"))?;
        let mode = super::tar_octal(&archive[100..108])
            .ok_or_else(|| "bridge request archive has no valid file mode".to_owned())?;

        assert_eq!(
            mode, 0o644,
            "staged bridge request must be readable by the OpenShell task execution identity"
        );
        Ok(())
    }
}
