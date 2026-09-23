//! Server-owned planning and orchestration for governed provider-control runtimes.

use std::collections::BTreeSet;
use std::hash::Hash;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::time::Duration as StdDuration;

use reqwest::Url;
use reqwest::header::AUTHORIZATION;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use steward_adapter_mcp_gw::{GithubStatusCredential, GithubStatusReader};
use steward_admission::internal_authorities::{steward_connections_v1, steward_connections_v2};
use steward_admission::{AdmissionDecision, Envelope, evaluate};
use steward_store::{
    ConnectionExecutionBindingSnapshot, ConnectionOAuthPhase,
    ConnectionOperationKind as StoredOperationKind, ConnectionOperationRecord,
    ConnectionOperationReservationRequest, ConnectionOperationRetention, ConnectionOperationState,
    PgStore, StoreError, TaskOrchestrationMode, TaskReservationRequest,
};
use steward_types::{
    AgentRuntimeSpec, AgentType, CanonicalAuthorityBinding, CanonicalUserId, Email, Principal,
    ToolGrant,
};
use uuid::Uuid;

use crate::BoxFuture;
use crate::connections::{
    AuthorizationUrl, ConnectionBrokerError, ConnectionPhase, ConnectionSession,
    ProviderConnectionBroker, ProviderConnectionStatus, StartedConnection,
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
pub const CONNECTION_CLEANUP_STALL_SECONDS: i64 = 150;
pub const MCP_GW_OAUTH_STATE_LIFETIME_SECONDS: i64 =
    steward_connections_v1::OAUTH_STATE_LIFETIME_SECONDS;
pub const MCP_GW_OAUTH_CLOCK_SKEW_SECONDS: i64 = steward_connections_v1::OAUTH_CLOCK_SKEW_SECONDS;
pub const MCP_GW_CONTRACT_VERSION: &str = steward_connections_v1::MCP_GW_VERSION;
pub const GITHUB_ATTESTATION_TRUST_MODE: &str = "github-attestation";
pub const OPERATOR_PINNED_TRUST_MODE: &str = "operator-pinned";
const MAX_BRIDGE_RESULT_BYTES: usize = 32 * 1024;
const TAR_BLOCK_BYTES: usize = 512;
const RECONCILE_INTERVAL: StdDuration = StdDuration::from_millis(100);
const DIRECT_STATUS_DEADLINE: StdDuration = StdDuration::from_secs(1);
const CONTROL_PLANE_STATUS_SCOPE: &str = "connections_status";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionOperationKind {
    Status,
    Start,
    Disconnect,
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
    })
}

fn provider_control_grant(action: &str) -> Result<ToolGrant, GovernedConnectionPlanError> {
    steward_connections_v1::provider_control_grant(action)
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
        steward_connections_v2::MCP_GW_VERSION => Ok((
            steward_connections_v2::envelope(),
            steward_connections_v2::AUTHORITY_VERSION,
            steward_connections_v2::AUTHORITY_DIGEST,
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
        }
    }

    pub const fn bridge_operation(self) -> &'static str {
        match self {
            Self::Status => "github.status",
            Self::Start => "github.start",
            Self::Disconnect => "github.disconnect",
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
}

#[derive(Clone)]
pub struct DirectConnectionStatusReader {
    client: reqwest::Client,
    control_plane_credential_file: PathBuf,
    gateway: GithubStatusReader,
    mint_endpoint: Url,
    store: PgStore,
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
        })
    }

    async fn read<B>(
        &self,
        session: &ConnectionSession<B>,
    ) -> Result<ProviderConnectionStatus, ConnectionBrokerError> {
        tokio::time::timeout(DIRECT_STATUS_DEADLINE, self.read_inner(session))
            .await
            .map_err(|_| ConnectionBrokerError::Unavailable)?
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
    ) -> BoxFuture<'a, Result<StartedConnection, ConnectionBrokerError>> {
        self.mutations.start(session)
    }

    fn disconnect<'a>(
        &'a self,
        session: &'a ConnectionSession<B>,
    ) -> BoxFuture<'a, Result<(), ConnectionBrokerError>> {
        self.mutations.disconnect(session)
    }
}

#[derive(Clone)]
pub struct GovernedConnectionsBroker<B> {
    store: PgStore,
    config: GovernedConnectionsConfig,
    binding: PhantomData<fn() -> B>,
    orchestration_mode: TaskOrchestrationMode,
}

impl<B> GovernedConnectionsBroker<B> {
    pub fn new(
        store: PgStore,
        config: GovernedConnectionsConfig,
        orchestration_mode: TaskOrchestrationMode,
    ) -> Self {
        Self {
            store,
            config,
            binding: PhantomData,
            orchestration_mode,
        }
    }

    async fn reserve(
        &self,
        canonical_user_id: &CanonicalUserId,
        display_email: &str,
        operation: ConnectionOperationKind,
        allow_status_cache: bool,
    ) -> Result<ConnectionOperationRecord, ConnectionBrokerError> {
        if !self.orchestration_mode.is_active() {
            return Err(ConnectionBrokerError::Unavailable);
        }
        let email = Email::parse(display_email.to_owned())
            .map_err(|_| ConnectionBrokerError::Unavailable)?;
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
        };
        let input = single_file_archive(
            "request.json",
            &serde_json::to_vec(&body).map_err(|_| ConnectionBrokerError::Unavailable)?,
        )?;
        let operation_id = Uuid::new_v4();
        let operation_key = operation_id.to_string();
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
        let (service_envelope, _, _) = connection_authority(&plan.bindings.mcp_gw_version)
            .map_err(|_| ConnectionBrokerError::Unavailable)?;
        let admission = evaluate(&plan.spec, &service_envelope)
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
            &service_envelope,
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
            workflow: if plan.authority_version == steward_connections_v2::AUTHORITY_VERSION {
                "internal:steward-connections/v2"
            } else {
                "internal:steward-connections/v1"
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
            direct_task_evidence: None,
            envelope_revision: plan.authority_version,
            service_envelope: &service_envelope,
            service_envelope_digest: &orchestration.service_envelope_digest,
            candidate_digest: &orchestration.candidate_digest,
            admission_decision: &admission,
            inert_manifest_digest: &orchestration.inert_manifest_digest,
            active_manifest_digest: &orchestration.active_manifest_digest,
        };
        self.store
            .reserve_connection_operation(&ConnectionOperationReservationRequest {
                operation_id,
                operation_kind: operation.into(),
                authority_id: plan.authority_id,
                authority_version: plan.authority_version,
                authority_digest: &plan.authority_digest,
                bindings: &bindings,
                idempotency_identity: &operation_key,
                response_deadline_seconds: CONNECTION_RESPONSE_DEADLINE_SECONDS,
                allow_status_cache,
                input_archive: &input,
                task,
            })
            .await
            .map(|reservation| reservation.record)
            .map_err(store_broker_error)
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
                    return Err(ConnectionBrokerError::Unavailable);
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

    async fn status_operation(
        &self,
        session: &ConnectionSession<B>,
        allow_cache: bool,
    ) -> Result<ProviderConnectionStatus, ConnectionBrokerError> {
        let record = self
            .reserve(
                &session.subject.canonical_user_id,
                &session.subject.display_email,
                ConnectionOperationKind::Status,
                allow_cache,
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
        }
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
    ) -> BoxFuture<'a, Result<StartedConnection, ConnectionBrokerError>> {
        Box::pin(async move {
            let record = self
                .reserve(
                    &session.subject.canonical_user_id,
                    &session.subject.display_email,
                    ConnectionOperationKind::Start,
                    true,
                )
                .await?;
            let completed = if record.operation_state == ConnectionOperationState::Succeeded {
                record
            } else {
                self.wait(&session.subject.canonical_user_id, record.operation_id)
                    .await?
            };
            if completed.oauth_phase != ConnectionOAuthPhase::Pending {
                return Err(ConnectionBrokerError::Unavailable);
            }
            let authorization_url = completed
                .authorization_url
                .ok_or(ConnectionBrokerError::Unavailable)
                .and_then(|value| {
                    AuthorizationUrl::new(value).map_err(|_| ConnectionBrokerError::Unavailable)
                })?;
            let expires_at = completed
                .flow_expires_at
                .ok_or(ConnectionBrokerError::Unavailable)?;
            Ok(StartedConnection {
                authorization_url,
                expires_at,
            })
        })
    }

    fn disconnect<'a>(
        &'a self,
        session: &'a ConnectionSession<B>,
    ) -> BoxFuture<'a, Result<(), ConnectionBrokerError>> {
        Box::pin(async move {
            let reserve = self
                .reserve(
                    &session.subject.canonical_user_id,
                    &session.subject.display_email,
                    ConnectionOperationKind::Disconnect,
                    true,
                )
                .await;
            let record = match reserve {
                Err(ConnectionBrokerError::OAuthFlowPending) => {
                    let status = self.status_operation(session, false).await?;
                    if status.phase != ConnectionPhase::Connected {
                        return Err(ConnectionBrokerError::OAuthFlowPending);
                    }
                    self.reserve(
                        &session.subject.canonical_user_id,
                        &session.subject.display_email,
                        ConnectionOperationKind::Disconnect,
                        true,
                    )
                    .await?
                }
                other => other?,
            };
            let completed = if record.operation_state == ConnectionOperationState::Succeeded {
                record
            } else {
                self.wait(&session.subject.canonical_user_id, record.operation_id)
                    .await?
            };
            let disconnected = completed
                .result
                .as_ref()
                .and_then(Value::as_object)
                .and_then(|object| object.get("disconnected"))
                .and_then(Value::as_bool)
                == Some(true);
            if disconnected {
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
}

impl ConnectionOperationReconciler {
    pub fn new(store: PgStore) -> Self {
        Self { store }
    }

    pub async fn run(self) {
        loop {
            if let Err(error) = self.reconcile_once().await {
                eprintln!("connection operation reconcile failed: {error}");
            }
            tokio::time::sleep(StdDuration::from_secs(1)).await;
        }
    }

    pub async fn reconcile_once(&self) -> Result<(), StoreError> {
        for operation in self
            .store
            .connection_operations_requiring_reconcile()
            .await?
        {
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
            ) {
                self.store
                    .fail_connection_operation(operation.operation_id, category)
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
            if self
                .store
                .connection_operation_deadline_elapsed(operation.operation_id)
                .await?
            {
                self.store
                    .fail_connection_operation(operation.operation_id, "deadline_exceeded")
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
                                        result_ttl_seconds: CONNECTION_MUTATION_RESULT_SECONDS,
                                        oauth_lifetime_seconds: MCP_GW_OAUTH_STATE_LIFETIME_SECONDS
                                            + MCP_GW_OAUTH_CLOCK_SKEW_SECONDS,
                                    },
                                )
                                .await?;
                        }
                        Err(_) => {
                            self.store
                                .fail_connection_operation(
                                    operation.operation_id,
                                    "invalid_bridge_result",
                                )
                                .await?;
                        }
                    }
                }
                steward_types::TaskPhase::Failed | steward_types::TaskPhase::Cancelled => {
                    self.store
                        .fail_connection_operation(operation.operation_id, "bridge_failed")
                        .await?;
                }
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
        steward_types::TaskPhase::Failed | steward_types::TaskPhase::Cancelled => "bridge_failed",
        steward_types::TaskPhase::Submitted
        | steward_types::TaskPhase::Parked
        | steward_types::TaskPhase::Queued
        | steward_types::TaskPhase::Running => "bridge_finalized_without_terminal_result",
    })
}

#[cfg(test)]
mod finalized_connection_operation_tests {
    use steward_store::ConnectionOperationState;
    use steward_types::TaskPhase;

    use super::finalized_nonterminal_failure;

    #[test]
    fn finalized_failed_bridge_terminalizes_its_connection_operation() {
        assert_eq!(
            finalized_nonterminal_failure(
                true,
                ConnectionOperationState::Queued,
                TaskPhase::Failed,
            ),
            Some("bridge_failed")
        );
        assert_eq!(
            finalized_nonterminal_failure(
                true,
                ConnectionOperationState::Succeeded,
                TaskPhase::Succeeded,
            ),
            None
        );
        assert_eq!(
            finalized_nonterminal_failure(
                false,
                ConnectionOperationState::Queued,
                TaskPhase::Failed,
            ),
            None
        );
    }
}

fn store_broker_error(error: StoreError) -> ConnectionBrokerError {
    match error {
        StoreError::ConnectionOAuthFlowPending => ConnectionBrokerError::OAuthFlowPending,
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
    }
    Ok(value)
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
        scopes_required,
        scopes_granted,
        scopes_missing,
        expires_at: None,
        active_credential_expires_at,
        renewal_credential_expires_at,
    })
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
        tools: vec![provider_control_grant(operation.action())?],
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
    use steward_types::{CanonicalUserId, Email, Principal};

    use crate::BoxFuture;
    use crate::connections::{
        ConnectionBrokerError, ConnectionPhase, ConnectionSession, ConnectionSubject,
        ProviderConnectionBroker, ProviderConnectionStatus, StartedConnection,
    };

    use super::{
        CONNECTION_RESPONSE_DEADLINE_SECONDS, CONNECTIONS_AUTHORITY_DIGEST,
        CONNECTIONS_AUTHORITY_DOCUMENT, CONNECTIONS_AUTHORITY_VERSION, CONNECTIONS_SERVICE,
        ConnectionExecutionBindings, ConnectionOperationKind, GITHUB_ATTESTATION_TRUST_MODE,
        GovernedConnectionPlanError, MCP_GW_CONTRACT_VERSION, MCP_GW_OAUTH_CLOCK_SKEW_SECONDS,
        MCP_GW_OAUTH_STATE_LIFETIME_SECONDS, OPERATOR_PINNED_TRUST_MODE,
        ProviderConnectionStatusSource, SplitConnectionsBroker, bridge_result,
        plan_connection_operation, provider_status, single_file_archive,
        valid_operator_pinned_image,
    };

    #[derive(Clone)]
    struct RejectingMutations {
        status_calls: Arc<AtomicUsize>,
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
        ) -> BoxFuture<'a, Result<StartedConnection, ConnectionBrokerError>> {
            Box::pin(async { Err(ConnectionBrokerError::Unavailable) })
        }

        fn disconnect<'a>(
            &'a self,
            _session: &'a ConnectionSession<String>,
        ) -> BoxFuture<'a, Result<(), ConnectionBrokerError>> {
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
    fn lifecycle_gateway_selects_immutable_v2_authority() -> Result<(), String> {
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
            "/../../config/internal-authorities/steward-connections/v2.json"
        ));
        assert_eq!(plan.authority_version, 2);
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
