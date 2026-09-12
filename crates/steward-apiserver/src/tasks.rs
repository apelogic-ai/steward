use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::future::Future;
use std::path::Path as FilePath;
use std::pin::Pin;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use jsonwebtoken::jwk::{Jwk, JwkSet, KeyAlgorithm, PublicKeyUse};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
#[cfg(test)]
use k8s_openapi::api::authentication::v1::TokenReviewStatus;
use k8s_openapi::api::authentication::v1::{TokenReview, UserInfo};
use kube::api::{Api, PostParams};
use kube::{Client, ResourceExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use steward_admission::{
    AdmissionDecision, AdmissionDelta, Envelope, EnvelopeSpec, evaluate_with_grants,
};
use steward_ports::{
    GitFile, GitFileRequest, GitHostingPlane, GitRepositoryIdentity, MAX_TASK_INPUT_ARCHIVE_BYTES,
    TaskExecutionAdapter, TaskExecutionPlanRequest,
};
use steward_store::{
    EnvelopeRequestRecord, PgStore, StoreError, TaskOrchestrationMode, TaskRecord,
    TaskReservationRequest, TaskRuntimeOperationRecord, TaskRuntimeOwnership,
    WorkflowRevisionRecord,
};
use steward_types::direct_package::{
    BoundedText, ClosureEntry, ClosureEntryKind, ContentDigest, DirectAdmissionDelta,
    DirectRequirements, DirectRuntimeOwnership, DirectTaskBindingEvidence, DirectTaskDefinition,
    DirectTaskPhase, DirectTaskStatusResponse, DirectTaskSubmission, EnvelopeDigest,
    EnvelopeEvidence, InstructionSkill, InvocationManifest, PACKAGE_CLOSURE_CONTRACT_VERSION,
    PackageClosure, PackageCommit, RelativePath, RepositoryUrl, ResolvedSource, SourceProvenance,
    StableProviderId, TASK_BINDING_EVIDENCE_SCHEMA, TriggerRepository, canonical_json_bytes,
};
use steward_types::{
    AgentRuntime, AgentRuntimeSpec, Budget, CanonicalAuthorityBinding, CanonicalUserId, Duration,
    Email, ModelRef, PENDING_APPROVAL_ANNOTATION, Principal, RuntimeOwnership,
    TaskExecutionBinding, TaskPhase, ToolGrant,
};
use uuid::Uuid;

use crate::WorkflowReference;
use crate::execution_bindings::ExecutionBindingCatalog;
use crate::{
    AdmissionLedger, ApiError, BoxFuture, KubernetesTokenReviewAudience, RuntimeRepository,
    authenticated_token_review_user, spec_digest, token_review_request,
};

const SERVICE_GROUP_PREFIX: &str = "agents.apelogic.ai/service-principal:";
const ACTING_USER_GROUP_PREFIX: &str = "agents.apelogic.ai/acting-user:";
const TASK_OWNER_GROUP_PREFIX: &str = "agents.apelogic.ai/task-owner:";
const CANONICAL_USER_GROUP_PREFIX: &str = "agents.apelogic.ai/canonical-user:";
const VERSIONED_WORKFLOW_NAMESPACE: &str = "steward-workflows";
const IDENTITY_TASK_CONTRACT: &str = "steward-task-v2";
const MAX_IDENTITY_TASK_TOKEN_BYTES: usize = 16 * 1024;
const MAX_IDENTITY_JWKS_BYTES: usize = 128 * 1024;
const MAX_IDENTITY_TASK_TOKEN_AGE_SECONDS: u64 = 300;
const IDENTITY_CLOCK_SKEW_SECONDS: u64 = 60;
pub const MAX_SOURCE_REPOSITORY_BINDINGS_BYTES: usize = 1024 * 1024;
const SOURCE_REPOSITORY_BINDINGS_CONTRACT: &str = "steward.source-repository-bindings/v1";
const MAX_SOURCE_REPOSITORY_BINDINGS: usize = 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SourceRepositoryBindingsDocument {
    contract_version: String,
    bindings: Vec<SourceRepositoryBinding>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SourceRepositoryBinding {
    caller: SourceRepositoryIdentity,
    source: SourceRepositoryIdentity,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SourceRepositoryIdentity {
    owner_id: StableProviderId,
    repository_id: StableProviderId,
}

type SourceRepositoryBindingKey = (String, String, String, String);

fn versioned_workflow_reference(
    workflow: &str,
    coding_agent_runtime: Option<&str>,
) -> Result<Option<WorkflowReference>, ApiError> {
    if !workflow.contains('@') {
        return Ok(None);
    }
    let reference = WorkflowReference::parse(workflow).map_err(|_| {
        ApiError::Admission("workflow must be an exact lowercase name@positive-version".to_owned())
    })?;
    if coding_agent_runtime.is_some() {
        return Err(ApiError::Admission(
            "codingAgentRuntime is server-selected for versioned Workflows".to_owned(),
        ));
    }
    Ok(Some(reference))
}

struct VersionedTaskPlan {
    workflow: WorkflowRevisionRecord,
    envelope: EnvelopeRequestRecord,
    spec: AgentRuntimeSpec,
    command: Vec<String>,
    execution_binding: TaskExecutionBinding,
}

struct DirectTaskPreAdmission {
    definition: DirectTaskDefinition,
    invocation: ResolvedSource,
    package: ResolvedSource,
    closure: PackageClosure,
    closure_digest: ContentDigest,
    diagnostics: steward_types::direct_package::DiagnosticsRequest,
    envelope: EnvelopeRequestRecord,
    effective_requirements: DirectRequirements,
    spec: AgentRuntimeSpec,
    command: Vec<String>,
    execution_binding: TaskExecutionBinding,
}

trait LegacyRuntimeResolver: Send + Sync {
    fn get_by_uid<'a>(
        &'a self,
        runtime_uid: &'a str,
    ) -> BoxFuture<'a, Result<AgentRuntime, String>>;
}

trait DirectGitResolver: Send + Sync {
    fn resolve_repository<'a>(
        &'a self,
        repository: &'a steward_types::direct_package::RepositoryUrl,
    ) -> BoxFuture<'a, Result<GitRepositoryIdentity, steward_ports::PortError>>;

    fn read_file<'a>(
        &'a self,
        request: &'a GitFileRequest,
    ) -> BoxFuture<'a, Result<GitFile, steward_ports::PortError>>;
}

impl<G> DirectGitResolver for G
where
    G: GitHostingPlane,
{
    fn resolve_repository<'a>(
        &'a self,
        repository: &'a steward_types::direct_package::RepositoryUrl,
    ) -> BoxFuture<'a, Result<GitRepositoryIdentity, steward_ports::PortError>> {
        Box::pin(GitHostingPlane::resolve_repository(self, repository))
    }

    fn read_file<'a>(
        &'a self,
        request: &'a GitFileRequest,
    ) -> BoxFuture<'a, Result<GitFile, steward_ports::PortError>> {
        Box::pin(GitHostingPlane::read_file(self, request))
    }
}

impl<R> LegacyRuntimeResolver for R
where
    R: RuntimeRepository,
{
    fn get_by_uid<'a>(
        &'a self,
        runtime_uid: &'a str,
    ) -> BoxFuture<'a, Result<AgentRuntime, String>> {
        RuntimeRepository::get_by_uid(self, runtime_uid)
    }
}

#[derive(Clone)]
pub struct TaskApiConfig {
    tool_transport_endpoint: Option<String>,
    execution_bindings: ExecutionBindingCatalog,
    execution_adapters: BTreeMap<String, Arc<dyn TaskExecutionAdapter>>,
    execution_bindings_active: bool,
    orchestration_mode: TaskOrchestrationMode,
    legacy_runtime_resolver: Option<Arc<dyn LegacyRuntimeResolver>>,
    direct_git_resolver: Option<Arc<dyn DirectGitResolver>>,
    source_repository_bindings: BTreeSet<SourceRepositoryBindingKey>,
}

impl Default for TaskApiConfig {
    fn default() -> Self {
        Self {
            tool_transport_endpoint: None,
            execution_bindings: ExecutionBindingCatalog::default(),
            execution_adapters: BTreeMap::new(),
            execution_bindings_active: false,
            orchestration_mode: TaskOrchestrationMode::Staged,
            legacy_runtime_resolver: None,
            direct_git_resolver: None,
            source_repository_bindings: BTreeSet::new(),
        }
    }
}

impl TaskApiConfig {
    pub fn new(tool_transport_endpoint: Option<String>) -> Result<Self, String> {
        let tool_transport_endpoint = match tool_transport_endpoint {
            Some(value) if value.is_empty() => None,
            Some(value) => Some(validate_tool_transport_endpoint(value)?),
            None => None,
        };
        Ok(Self {
            tool_transport_endpoint,
            ..Self::default()
        })
    }

    pub fn with_execution_adapter(
        mut self,
        adapter: Arc<dyn TaskExecutionAdapter>,
    ) -> Result<Self, String> {
        let contract = adapter.contract();
        if contract.is_empty()
            || contract.trim() != contract
            || contract.chars().any(char::is_control)
        {
            return Err("execution adapter contract must be an exact non-empty value".to_owned());
        }
        if self
            .execution_adapters
            .insert(contract.to_owned(), adapter)
            .is_some()
        {
            return Err(format!(
                "execution adapter contract {contract} is configured more than once"
            ));
        }
        Ok(self)
    }

    pub fn with_execution_bindings_json(mut self, value: Option<&str>) -> Result<Self, String> {
        if let Some(value) = value {
            self.execution_bindings = ExecutionBindingCatalog::from_json(value)?;
        }
        Ok(self)
    }

    pub fn with_execution_bindings_active(mut self, active: bool) -> Result<Self, String> {
        if active {
            for binding in self.execution_bindings.bindings() {
                if !self.execution_adapters.contains_key(&binding.adapter) {
                    return Err(format!(
                        "execution binding {} uses unavailable adapter {}",
                        binding.agent_ref, binding.adapter
                    ));
                }
            }
        }
        self.execution_bindings_active = active;
        Ok(self)
    }

    pub fn with_task_orchestration_mode(mut self, mode: TaskOrchestrationMode) -> Self {
        self.orchestration_mode = mode;
        self
    }

    pub fn with_legacy_runtime_resolver<R>(mut self, resolver: R) -> Self
    where
        R: RuntimeRepository,
    {
        self.legacy_runtime_resolver = Some(Arc::new(resolver));
        self
    }

    pub fn with_git_hosting_plane<G>(mut self, resolver: G) -> Self
    where
        G: GitHostingPlane,
    {
        self.direct_git_resolver = Some(Arc::new(resolver));
        self
    }

    pub fn with_source_repository_bindings_json(
        mut self,
        value: Option<&str>,
    ) -> Result<Self, String> {
        let Some(value) = value else {
            return Ok(self);
        };
        if value.len() > MAX_SOURCE_REPOSITORY_BINDINGS_BYTES {
            return Err("source repository binding catalog exceeds 1048576 bytes".to_owned());
        }
        let document = serde_json::from_str::<SourceRepositoryBindingsDocument>(value)
            .map_err(|error| format!("source repository binding catalog is invalid: {error}"))?;
        if document.contract_version != SOURCE_REPOSITORY_BINDINGS_CONTRACT
            || document.bindings.len() > MAX_SOURCE_REPOSITORY_BINDINGS
        {
            return Err("source repository binding catalog is invalid".to_owned());
        }
        for binding in document.bindings {
            let key = (
                binding.caller.owner_id.as_str().to_owned(),
                binding.caller.repository_id.as_str().to_owned(),
                binding.source.owner_id.as_str().to_owned(),
                binding.source.repository_id.as_str().to_owned(),
            );
            if !self.source_repository_bindings.insert(key) {
                return Err("source repository binding catalog contains a duplicate".to_owned());
            }
        }
        Ok(self)
    }

    fn source_repository_is_authorized(
        &self,
        caller: &TriggerRepository,
        source: &GitRepositoryIdentity,
    ) -> bool {
        self.source_repository_bindings.contains(&(
            caller.owner_id.as_str().to_owned(),
            caller.id.as_str().to_owned(),
            source.repository_owner_id.as_str().to_owned(),
            source.repository_id.as_str().to_owned(),
        ))
    }

    pub fn execution_binding_refs(&self) -> Vec<String> {
        if self.execution_bindings_active {
            self.execution_bindings
                .bindings()
                .filter(|binding| self.execution_adapters.contains_key(&binding.adapter))
                .map(|binding| binding.agent_ref.clone())
                .collect()
        } else {
            Vec::new()
        }
    }

    pub fn execution_binding_advertisements(
        &self,
    ) -> Vec<crate::execution_bindings::ExecutionBindingAdvertisement> {
        if self.execution_bindings_active {
            self.execution_bindings
                .advertisements()
                .into_iter()
                .filter(|advertisement| {
                    self.execution_bindings
                        .resolve(&advertisement.agent_ref)
                        .is_some_and(|binding| {
                            self.execution_adapters.contains_key(&binding.adapter)
                        })
                })
                .collect()
        } else {
            Vec::new()
        }
    }
}

fn validate_tool_transport_endpoint(value: String) -> Result<String, String> {
    if value.trim() != value || value.chars().any(char::is_control) {
        return Err("task tool transport endpoint must be an exact HTTP(S) URL".to_owned());
    }
    let endpoint = reqwest::Url::parse(&value)
        .map_err(|_| "task tool transport endpoint must be an exact HTTP(S) URL".to_owned())?;
    if !matches!(endpoint.scheme(), "http" | "https")
        || endpoint.host_str().is_none()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
        || endpoint.port() == Some(0)
    {
        return Err("task tool transport endpoint must be an exact HTTP(S) URL".to_owned());
    }
    Ok(endpoint.to_string())
}

fn resolve_versioned_task_plan(
    identity: &TaskIdentity,
    workflow: WorkflowRevisionRecord,
    envelopes: Vec<EnvelopeRequestRecord>,
    config: &TaskApiConfig,
) -> Result<VersionedTaskPlan, ApiError> {
    let [envelope] = envelopes.as_slice() else {
        return if envelopes.is_empty() {
            Err(ApiError::MissingEnvelope)
        } else {
            Err(ApiError::Conflict(
                "multiple active provisioned User Envelopes are ambiguous".to_owned(),
            ))
        };
    };
    if envelope.owner_user_id != identity.canonical_user_id {
        return Err(ApiError::PrincipalMismatch);
    }
    if envelope.status != steward_store::EnvelopeRequestStatus::Provisioned
        || envelope.envelope_instance_id.is_none()
        || envelope.envelope_digest.is_none()
    {
        return Err(ApiError::MissingEnvelope);
    }
    let approved = envelope
        .approved_envelope
        .as_ref()
        .ok_or(ApiError::MissingEnvelope)?;
    let [model] = approved.spec.llms.as_slice() else {
        return Err(ApiError::Admission(
            "versioned Workflows require exactly one approved model".to_owned(),
        ));
    };
    let execution_binding = config
        .execution_bindings_active
        .then(|| config.execution_bindings.resolve(&workflow.agent))
        .flatten()
        .cloned()
        .ok_or_else(|| {
            ApiError::TaskRuntimeContractUnavailable(format!(
                "logical agent {} has no deployment execution binding",
                workflow.agent
            ))
        })?;
    if execution_binding.provider_profiles.inference.is_none() {
        return Err(ApiError::TaskRuntimeContractUnavailable(format!(
            "logical agent {} has no inference provider profile",
            workflow.agent
        )));
    }
    if !approved.spec.tools.is_empty() && execution_binding.provider_profiles.tools.is_none() {
        return Err(ApiError::TaskRuntimeContractUnavailable(format!(
            "logical agent {} has no tool provider profile",
            workflow.agent
        )));
    }
    let adapter = config
        .execution_adapters
        .get(&execution_binding.adapter)
        .ok_or_else(|| {
            ApiError::TaskRuntimeContractUnavailable(format!(
                "logical agent {} uses an unavailable execution adapter",
                workflow.agent
            ))
        })?;
    let tool_transport_endpoint = if approved.spec.tools.is_empty() {
        None
    } else {
        Some(config.tool_transport_endpoint.as_deref().ok_or_else(|| {
            ApiError::TaskRuntimeContractUnavailable(
                "tool-bearing versioned Workflow requires a tool transport endpoint".to_owned(),
            )
        })?)
    };
    let canonical_authority = CanonicalAuthorityBinding::new(
        identity.canonical_user_id.clone(),
        identity
            .acting_user
            .as_ref()
            .map(|_| identity.canonical_user_id.clone()),
    )
    .map_err(ApiError::Admission)?;
    let spec = AgentRuntimeSpec {
        principal: Principal::Service {
            name: identity.service.clone(),
            acting_user: identity.acting_user.clone(),
        },
        owner: identity.owner.clone(),
        canonical_authority: Some(canonical_authority),
        agent_type: steward_types::AgentType {
            name: workflow.agent.clone(),
        },
        llms: approved.spec.llms.clone(),
        tools: approved.spec.tools.clone(),
        budget: approved.spec.budget.clone(),
        ttl: approved.spec.ttl.clone(),
        runner: approved.spec.runner.clone(),
        bindings: None,
    };
    let command = adapter
        .render(TaskExecutionPlanRequest {
            workflow_prompt: &workflow.prompt,
            model,
            tools: &approved.spec.tools,
            tool_transport_endpoint,
            binding: &execution_binding,
        })
        .map_err(|error| {
            ApiError::TaskRuntimeContractUnavailable(format!(
                "logical agent {} execution plan could not be rendered: {error:?}",
                workflow.agent
            ))
        })?
        .command;
    Ok(VersionedTaskPlan {
        workflow,
        envelope: envelope.clone(),
        spec,
        command,
        execution_binding: TaskExecutionBinding::Disposable(execution_binding),
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskAuthenticationError {
    InvalidCredentials,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskIdentity {
    pub service: String,
    pub acting_user: Option<Email>,
    pub owner: Email,
    pub canonical_user_id: CanonicalUserId,
    /// Identity-ratified GitHub source provenance. Kubernetes identities and legacy
    /// Identity credentials intentionally carry no Git source authority.
    pub source_provenance: Option<SourceProvenance>,
}

pub trait TaskIdentityResolver: Clone + Send + Sync + 'static {
    fn resolve<'a>(
        &'a self,
        assertion: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<TaskIdentity, TaskAuthenticationError>> + Send + 'a>>;
}

#[derive(Clone)]
pub struct KubernetesTaskIdentityResolver {
    client: Client,
    audience: KubernetesTokenReviewAudience,
    canonical_identities: PgStore,
}

impl KubernetesTaskIdentityResolver {
    pub fn new(
        client: Client,
        audience: KubernetesTokenReviewAudience,
        canonical_identities: PgStore,
    ) -> Self {
        Self {
            client,
            audience,
            canonical_identities,
        }
    }
}

impl TaskIdentityResolver for KubernetesTaskIdentityResolver {
    fn resolve<'a>(
        &'a self,
        assertion: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<TaskIdentity, TaskAuthenticationError>> + Send + 'a>>
    {
        Box::pin(async move {
            let review = token_review_request(assertion, &self.audience);
            let reviewed = Api::<TokenReview>::all(self.client.clone())
                .create(&PostParams::default(), &review)
                .await
                .map_err(|_| TaskAuthenticationError::Unavailable)?;
            let user = authenticated_token_review_user(reviewed.status, self.audience.as_str())
                .ok_or(TaskAuthenticationError::InvalidCredentials)?;
            let identity = task_identity_from_kubernetes_user(&user)?;
            self.canonical_identities
                .resolve_canonical_principal(&identity.canonical_user_id, &identity.owner)
                .await
                .map_err(|error| match error {
                    StoreError::Database(_) => TaskAuthenticationError::Unavailable,
                    _ => TaskAuthenticationError::InvalidCredentials,
                })?;
            Ok(identity)
        })
    }
}

/// Verifies the short-lived, ES256 task credential issued by the deployed Identity service.
///
/// This is deliberately a distinct resolver from Kubernetes TokenReview. A GitHub Actions
/// runner cannot present a Kubernetes service-account token, and an Identity credential must
/// never be sent to the Kubernetes TokenReview API. Deployments select exactly one resolver.
#[derive(Clone)]
pub struct IdentityTaskIdentityResolver {
    jwks: JwkSet,
    issuer: String,
    audience: String,
    canonical_identities: PgStore,
}

#[derive(Clone)]
pub enum ConfiguredTaskIdentityResolver {
    Kubernetes(KubernetesTaskIdentityResolver),
    Identity(IdentityTaskIdentityResolver),
}

impl ConfiguredTaskIdentityResolver {
    pub fn kubernetes(
        client: Client,
        audience: KubernetesTokenReviewAudience,
        canonical_identities: PgStore,
    ) -> Self {
        Self::Kubernetes(KubernetesTaskIdentityResolver::new(
            client,
            audience,
            canonical_identities,
        ))
    }

    pub fn identity_from_jwks_file(
        issuer: String,
        audience: String,
        jwks_file: &FilePath,
        canonical_identities: PgStore,
    ) -> Result<Self, TaskAuthenticationError> {
        Ok(Self::Identity(
            IdentityTaskIdentityResolver::from_jwks_file(
                issuer,
                audience,
                jwks_file,
                canonical_identities,
            )?,
        ))
    }

    /// Resolve the ratified Identity claims into the authenticated username and groups without
    /// performing task admission. This is used only by the route-scoped administrator
    /// authenticator for the Steward-run service-envelope bootstrap path.
    ///
    /// Kubernetes service-account credentials still use TokenReview. A caller cannot select
    /// this path: deployments select the configured task identity verifier, which validates the
    /// exact Identity issuer, audience, signature, expiry and identity contract first.
    pub(crate) fn authenticated_user(
        &self,
        assertion: &str,
    ) -> Result<UserInfo, TaskAuthenticationError> {
        match self {
            Self::Kubernetes(_) => Err(TaskAuthenticationError::InvalidCredentials),
            Self::Identity(resolver) => resolver.authenticated_user(assertion),
        }
    }
}

impl TaskIdentityResolver for ConfiguredTaskIdentityResolver {
    fn resolve<'a>(
        &'a self,
        assertion: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<TaskIdentity, TaskAuthenticationError>> + Send + 'a>>
    {
        match self {
            Self::Kubernetes(resolver) => resolver.resolve(assertion),
            Self::Identity(resolver) => resolver.resolve(assertion),
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(untagged)]
enum IdentityTaskAudience {
    Single(String),
    Multiple(Vec<String>),
}

#[derive(Clone, Deserialize)]
struct IdentityTaskClaims {
    iss: String,
    sub: String,
    aud: IdentityTaskAudience,
    exp: u64,
    iat: u64,
    nbf: u64,
    jti: String,
    email: String,
    email_verified: bool,
    groups: Vec<String>,
    identity_contract: String,
    #[serde(default)]
    source_provenance: Option<SourceProvenance>,
}

impl IdentityTaskIdentityResolver {
    pub fn from_jwks_file(
        issuer: String,
        audience: String,
        jwks_file: &FilePath,
        canonical_identities: PgStore,
    ) -> Result<Self, TaskAuthenticationError> {
        if !valid_identity_issuer(&issuer) || !bounded_non_whitespace(&audience, 256) {
            return Err(TaskAuthenticationError::InvalidCredentials);
        }
        let jwks =
            fs::read_to_string(jwks_file).map_err(|_| TaskAuthenticationError::Unavailable)?;
        if jwks.len() > MAX_IDENTITY_JWKS_BYTES {
            return Err(TaskAuthenticationError::InvalidCredentials);
        }
        let jwks =
            serde_json::from_str(&jwks).map_err(|_| TaskAuthenticationError::InvalidCredentials)?;
        validate_identity_task_jwks(&jwks)?;
        Ok(Self {
            jwks,
            issuer,
            audience,
            canonical_identities,
        })
    }
}

impl IdentityTaskIdentityResolver {
    pub(crate) fn authenticated_user(
        &self,
        assertion: &str,
    ) -> Result<UserInfo, TaskAuthenticationError> {
        let claims =
            verify_identity_task_token(assertion, &self.jwks, &self.issuer, &self.audience)?;
        Ok(UserInfo {
            username: Some(claims.email),
            groups: Some(claims.groups),
            ..UserInfo::default()
        })
    }
}

impl TaskIdentityResolver for IdentityTaskIdentityResolver {
    fn resolve<'a>(
        &'a self,
        assertion: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<TaskIdentity, TaskAuthenticationError>> + Send + 'a>>
    {
        Box::pin(async move {
            let claims =
                verify_identity_task_token(assertion, &self.jwks, &self.issuer, &self.audience)?;
            let identity = task_identity_from_identity_claims(claims)?;
            self.canonical_identities
                .resolve_canonical_principal(&identity.canonical_user_id, &identity.owner)
                .await
                .map_err(|error| match error {
                    StoreError::Database(_) => TaskAuthenticationError::Unavailable,
                    _ => TaskAuthenticationError::InvalidCredentials,
                })?;
            Ok(identity)
        })
    }
}

fn verify_identity_task_token(
    assertion: &str,
    jwks: &JwkSet,
    issuer: &str,
    audience: &str,
) -> Result<IdentityTaskClaims, TaskAuthenticationError> {
    if assertion.is_empty() || assertion.len() > MAX_IDENTITY_TASK_TOKEN_BYTES {
        return Err(TaskAuthenticationError::InvalidCredentials);
    }
    let header =
        decode_header(assertion).map_err(|_| TaskAuthenticationError::InvalidCredentials)?;
    if header.alg != Algorithm::ES256
        || header.jku.is_some()
        || header.jwk.is_some()
        || header.x5u.is_some()
        || header.x5c.is_some()
        || header.x5t.is_some()
        || header.x5t_s256.is_some()
        || header.crit.is_some()
    {
        return Err(TaskAuthenticationError::InvalidCredentials);
    }
    let kid = header
        .kid
        .ok_or(TaskAuthenticationError::InvalidCredentials)?;
    if !bounded_non_whitespace(&kid, 128) || !kid.is_ascii() {
        return Err(TaskAuthenticationError::InvalidCredentials);
    }
    let key = select_identity_task_key(jwks, &kid)?;
    let key =
        DecodingKey::from_jwk(key).map_err(|_| TaskAuthenticationError::InvalidCredentials)?;
    let mut validation = Validation::new(Algorithm::ES256);
    validation.required_spec_claims.clear();
    validation.validate_exp = false;
    validation.validate_aud = false;
    validation.validate_nbf = false;
    let claims = decode::<IdentityTaskClaims>(assertion, &key, &validation)
        .map_err(|_| TaskAuthenticationError::InvalidCredentials)?
        .claims;
    validate_identity_task_claims(&claims, issuer, audience)?;
    Ok(claims)
}

fn validate_identity_task_jwks(jwks: &JwkSet) -> Result<(), TaskAuthenticationError> {
    if jwks.keys.is_empty() || jwks.keys.len() > 16 {
        return Err(TaskAuthenticationError::InvalidCredentials);
    }
    let mut kids = std::collections::HashSet::new();
    let mut identity_task_keys = 0_usize;
    for key in &jwks.keys {
        let kid = key
            .common
            .key_id
            .as_deref()
            .ok_or(TaskAuthenticationError::InvalidCredentials)?;
        if !bounded_non_whitespace(kid, 128) || !kid.is_ascii() || !kids.insert(kid) {
            return Err(TaskAuthenticationError::InvalidCredentials);
        }
        // Identity's public JWKS can also publish keys for its separate workload
        // exchange contract. Only ES256 signing keys participate in the
        // steward-task-v2 trust domain; non-ES256 keys remain unselectable.
        if key.common.key_algorithm == Some(KeyAlgorithm::ES256) {
            if key.common.public_key_use != Some(PublicKeyUse::Signature)
                || DecodingKey::from_jwk(key).is_err()
            {
                return Err(TaskAuthenticationError::InvalidCredentials);
            }
            identity_task_keys += 1;
        }
    }
    (identity_task_keys > 0)
        .then_some(())
        .ok_or(TaskAuthenticationError::InvalidCredentials)
}

fn select_identity_task_key<'a>(
    jwks: &'a JwkSet,
    kid: &str,
) -> Result<&'a Jwk, TaskAuthenticationError> {
    let mut matching = jwks.keys.iter().filter(|key| {
        key.common.key_id.as_deref() == Some(kid)
            && key.common.key_algorithm == Some(KeyAlgorithm::ES256)
            && key.common.public_key_use == Some(PublicKeyUse::Signature)
    });
    let key = matching
        .next()
        .ok_or(TaskAuthenticationError::InvalidCredentials)?;
    if matching.next().is_some() {
        return Err(TaskAuthenticationError::InvalidCredentials);
    }
    Ok(key)
}

fn validate_identity_task_claims(
    claims: &IdentityTaskClaims,
    issuer: &str,
    audience: &str,
) -> Result<(), TaskAuthenticationError> {
    let now = jsonwebtoken::get_current_timestamp();
    let audience_matches = match &claims.aud {
        IdentityTaskAudience::Single(value) => value == audience,
        IdentityTaskAudience::Multiple(values) => {
            values.len() == 1 && values.first() == Some(&audience.to_owned())
        }
    };
    if claims.iss != issuer
        || !audience_matches
        || claims.identity_contract != IDENTITY_TASK_CONTRACT
        || !claims.email_verified
        || !valid_email(&claims.email)
        || !bounded_non_whitespace(&claims.sub, 255)
        || !bounded_non_whitespace(&claims.jti, 128)
        || claims.exp.saturating_add(IDENTITY_CLOCK_SKEW_SECONDS) <= now
        || claims.iat > now.saturating_add(IDENTITY_CLOCK_SKEW_SECONDS)
        || now
            > claims
                .iat
                .saturating_add(MAX_IDENTITY_TASK_TOKEN_AGE_SECONDS)
        || claims.nbf > now.saturating_add(IDENTITY_CLOCK_SKEW_SECONDS)
        || claims.groups.len() > 16
    {
        return Err(TaskAuthenticationError::InvalidCredentials);
    }
    if claims
        .source_provenance
        .as_ref()
        .is_some_and(|provenance| provenance.validate().is_err())
    {
        return Err(TaskAuthenticationError::InvalidCredentials);
    }
    Ok(())
}

fn task_identity_from_identity_claims(
    claims: IdentityTaskClaims,
) -> Result<TaskIdentity, TaskAuthenticationError> {
    let source_provenance = claims.source_provenance;
    let user = UserInfo {
        username: Some(claims.email),
        groups: Some(claims.groups),
        ..UserInfo::default()
    };
    task_identity_from_kubernetes_user(&user).map(|mut identity| {
        identity.source_provenance = source_provenance;
        identity
    })
}

fn valid_identity_issuer(value: &str) -> bool {
    value.starts_with("https://") && value.len() <= 2_048 && !value.chars().any(char::is_whitespace)
}

fn bounded_non_whitespace(value: &str, maximum: usize) -> bool {
    !value.is_empty() && value.len() <= maximum && !value.chars().any(char::is_whitespace)
}

#[cfg(test)]
pub(crate) fn task_identity_from_token_review(
    status: Option<TokenReviewStatus>,
    requested_audience: &str,
) -> Result<TaskIdentity, TaskAuthenticationError> {
    let user = authenticated_token_review_user(status, requested_audience)
        .ok_or(TaskAuthenticationError::InvalidCredentials)?;
    task_identity_from_kubernetes_user(&user)
}

fn task_identity_from_kubernetes_user(
    user: &UserInfo,
) -> Result<TaskIdentity, TaskAuthenticationError> {
    let username = user
        .username
        .as_deref()
        .filter(|username| !username.is_empty())
        .ok_or(TaskAuthenticationError::InvalidCredentials)?;
    let groups = user.groups.as_deref().unwrap_or_default();
    let services = group_values(groups, SERVICE_GROUP_PREFIX);
    let acting_users = group_values(groups, ACTING_USER_GROUP_PREFIX);
    let owners = group_values(groups, TASK_OWNER_GROUP_PREFIX);
    let canonical_users = group_values(groups, CANONICAL_USER_GROUP_PREFIX);
    let [service] = services.as_slice() else {
        return Err(TaskAuthenticationError::InvalidCredentials);
    };
    let [canonical_user] = canonical_users.as_slice() else {
        return Err(TaskAuthenticationError::InvalidCredentials);
    };
    let canonical_user_id = CanonicalUserId::parse(canonical_user.clone())
        .map_err(|_| TaskAuthenticationError::InvalidCredentials)?;
    if service.is_empty() {
        return Err(TaskAuthenticationError::InvalidCredentials);
    }
    let acting_user = match acting_users.as_slice() {
        [] => None,
        [acting_user]
            if valid_email(username) && valid_email(acting_user) && username == acting_user =>
        {
            Some(Email(acting_user.clone()))
        }
        _ => return Err(TaskAuthenticationError::InvalidCredentials),
    };
    let owner = if let Some(acting_user) = &acting_user {
        if !owners.is_empty() {
            return Err(TaskAuthenticationError::InvalidCredentials);
        }
        acting_user.clone()
    } else {
        let [owner] = owners.as_slice() else {
            return Err(TaskAuthenticationError::InvalidCredentials);
        };
        if !valid_email(owner) {
            return Err(TaskAuthenticationError::InvalidCredentials);
        }
        Email(owner.clone())
    };
    Ok(TaskIdentity {
        service: service.clone(),
        acting_user,
        owner,
        canonical_user_id,
        source_provenance: None,
    })
}

fn group_values(groups: &[String], prefix: &str) -> Vec<String> {
    groups
        .iter()
        .filter_map(|group| group.strip_prefix(prefix))
        .map(str::to_owned)
        .collect()
}

fn valid_email(value: &str) -> bool {
    let mut parts = value.split('@');
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(local), Some(domain), None)
            if !local.is_empty() && domain.contains('.') && !value.contains(char::is_whitespace)
    )
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskWorkflow {
    pub name: String,
    pub namespace: String,
    pub coding_agent_runtime: String,
    pub llms: Vec<ModelRef>,
    pub tools: Vec<ToolGrant>,
    pub budget: Budget,
    pub ttl: Duration,
    pub command: Vec<String>,
}

pub trait TaskWorkflowCatalog: Clone + Send + Sync + 'static {
    fn workflow(&self, name: &str) -> Option<TaskWorkflow>;
}

#[derive(Clone, Default)]
pub struct StaticTaskWorkflowCatalog {
    workflows: BTreeMap<String, TaskWorkflow>,
}

impl StaticTaskWorkflowCatalog {
    pub fn new(workflows: impl IntoIterator<Item = TaskWorkflow>) -> Self {
        Self {
            workflows: workflows
                .into_iter()
                .map(|workflow| (workflow.name.clone(), workflow))
                .collect(),
        }
    }

    pub fn from_json(value: &str) -> Result<Self, String> {
        let workflows = serde_json::from_str::<Vec<TaskWorkflow>>(value)
            .map_err(|error| format!("task workflow catalog is invalid: {error}"))?;
        if workflows.iter().any(|workflow| {
            workflow.name.is_empty()
                || workflow.namespace.is_empty()
                || workflow.coding_agent_runtime.is_empty()
                || workflow.command.is_empty()
                || workflow.command.iter().any(String::is_empty)
        }) {
            return Err(
                "task workflows require non-empty identity, namespace, runtime, and command fields"
                    .to_owned(),
            );
        }
        // Versioned Workflows are stored separately and may be the only task
        // submission path.  An empty legacy catalog is therefore valid: no
        // legacy workflow can resolve, while a versioned `name@version`
        // reference remains governed by the persisted Workflow repository.
        Ok(Self::new(workflows))
    }
}

impl TaskWorkflowCatalog for StaticTaskWorkflowCatalog {
    fn workflow(&self, name: &str) -> Option<TaskWorkflow> {
        self.workflows.get(name).cloned()
    }
}

pub trait TaskSubmissionLedger: Clone + Send + Sync + 'static {
    fn active_source_repository_binding<'a>(
        &'a self,
        _caller: &'a TriggerRepository,
        _source: &'a GitRepositoryIdentity,
    ) -> BoxFuture<'a, Result<bool, StoreError>> {
        Box::pin(async { Ok(false) })
    }

    fn active_provisioned_user_envelopes_by_digest<'a>(
        &'a self,
        _owner_user_id: &'a CanonicalUserId,
        _digest: &'a EnvelopeDigest,
    ) -> BoxFuture<'a, Result<Vec<EnvelopeRequestRecord>, StoreError>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn workflow_revision<'a>(
        &'a self,
        _name: &'a str,
        _version: i64,
    ) -> BoxFuture<'a, Result<Option<WorkflowRevisionRecord>, StoreError>> {
        Box::pin(async { Ok(None) })
    }

    fn active_provisioned_user_envelopes<'a>(
        &'a self,
        _owner_user_id: &'a CanonicalUserId,
    ) -> BoxFuture<'a, Result<Vec<EnvelopeRequestRecord>, StoreError>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn task_by_idempotency<'a>(
        &'a self,
        submitter_service: &'a str,
        owner_user_id: &'a str,
        idempotency_key: &'a str,
    ) -> BoxFuture<'a, Result<Option<TaskRecord>, StoreError>>;

    fn task_runtime_operation(
        &self,
        task_uid: Uuid,
    ) -> BoxFuture<'_, Result<Option<TaskRuntimeOperationRecord>, StoreError>>;

    fn reserve_task<'a>(
        &'a self,
        request: TaskReservationRequest<'a>,
    ) -> BoxFuture<'a, Result<steward_store::TaskReservation, StoreError>>;

    fn put_task_inputs<'a>(
        &'a self,
        task_uid: Uuid,
        submitter_service: &'a str,
        owner_user_id: &'a str,
        archive: &'a [u8],
    ) -> BoxFuture<'a, Result<TaskRecord, StoreError>>;

    fn request_task_execution<'a>(
        &'a self,
        task_uid: Uuid,
        submitter_service: &'a str,
        owner_user_id: &'a str,
    ) -> BoxFuture<'a, Result<TaskRecord, StoreError>>;

    fn task_for_submitter<'a>(
        &'a self,
        task_uid: Uuid,
        submitter_service: &'a str,
        owner_user_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<TaskRecord>, StoreError>>;

    fn request_task_finalization<'a>(
        &'a self,
        task_uid: Uuid,
        submitter_service: &'a str,
        owner_user_id: &'a str,
    ) -> BoxFuture<'a, Result<TaskRecord, StoreError>>;
}

impl TaskSubmissionLedger for PgStore {
    fn active_provisioned_user_envelopes_by_digest<'a>(
        &'a self,
        owner_user_id: &'a CanonicalUserId,
        digest: &'a EnvelopeDigest,
    ) -> BoxFuture<'a, Result<Vec<EnvelopeRequestRecord>, StoreError>> {
        Box::pin(async move {
            let store_digest = digest
                .as_str()
                .strip_prefix("steward:")
                .ok_or(StoreError::InvalidEnvelopeRequest)?;
            PgStore::envelope_requests(self, owner_user_id)
                .await
                .map(|records| {
                    records
                        .into_iter()
                        .filter(|record| {
                            record.status == steward_store::EnvelopeRequestStatus::Provisioned
                                && record.envelope_digest.as_deref() == Some(store_digest)
                        })
                        .collect()
                })
        })
    }

    fn workflow_revision<'a>(
        &'a self,
        name: &'a str,
        version: i64,
    ) -> BoxFuture<'a, Result<Option<WorkflowRevisionRecord>, StoreError>> {
        Box::pin(async move { PgStore::workflow_revision(self, name, version).await })
    }

    fn active_provisioned_user_envelopes<'a>(
        &'a self,
        owner_user_id: &'a CanonicalUserId,
    ) -> BoxFuture<'a, Result<Vec<EnvelopeRequestRecord>, StoreError>> {
        Box::pin(async move {
            PgStore::envelope_requests(self, owner_user_id)
                .await
                .map(|records| {
                    records
                        .into_iter()
                        .filter(|record| {
                            record.status == steward_store::EnvelopeRequestStatus::Provisioned
                        })
                        .collect()
                })
        })
    }

    fn task_by_idempotency<'a>(
        &'a self,
        submitter_service: &'a str,
        owner_user_id: &'a str,
        idempotency_key: &'a str,
    ) -> BoxFuture<'a, Result<Option<TaskRecord>, StoreError>> {
        Box::pin(async move {
            PgStore::task_by_idempotency(self, submitter_service, owner_user_id, idempotency_key)
                .await
        })
    }

    fn task_runtime_operation(
        &self,
        task_uid: Uuid,
    ) -> BoxFuture<'_, Result<Option<TaskRuntimeOperationRecord>, StoreError>> {
        Box::pin(async move { PgStore::task_runtime_operation(self, task_uid).await })
    }

    fn reserve_task<'a>(
        &'a self,
        request: TaskReservationRequest<'a>,
    ) -> BoxFuture<'a, Result<steward_store::TaskReservation, StoreError>> {
        Box::pin(async move { PgStore::reserve_task(self, &request).await })
    }

    fn put_task_inputs<'a>(
        &'a self,
        task_uid: Uuid,
        submitter_service: &'a str,
        owner_user_id: &'a str,
        archive: &'a [u8],
    ) -> BoxFuture<'a, Result<TaskRecord, StoreError>> {
        Box::pin(async move {
            PgStore::put_task_inputs(self, task_uid, submitter_service, owner_user_id, archive)
                .await
        })
    }

    fn request_task_execution<'a>(
        &'a self,
        task_uid: Uuid,
        submitter_service: &'a str,
        owner_user_id: &'a str,
    ) -> BoxFuture<'a, Result<TaskRecord, StoreError>> {
        Box::pin(async move {
            PgStore::request_task_execution(self, task_uid, submitter_service, owner_user_id).await
        })
    }

    fn task_for_submitter<'a>(
        &'a self,
        task_uid: Uuid,
        submitter_service: &'a str,
        owner_user_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<TaskRecord>, StoreError>> {
        Box::pin(async move {
            PgStore::task_for_submitter(self, task_uid, submitter_service, owner_user_id).await
        })
    }

    fn request_task_finalization<'a>(
        &'a self,
        task_uid: Uuid,
        submitter_service: &'a str,
        owner_user_id: &'a str,
    ) -> BoxFuture<'a, Result<TaskRecord, StoreError>> {
        Box::pin(async move {
            PgStore::request_task_finalization(self, task_uid, submitter_service, owner_user_id)
                .await
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskSubmissionRequest {
    pub workflow: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coding_agent_runtime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_runtime_uid: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(untagged)]
pub enum TaskCreateRequest {
    Existing(TaskSubmissionRequest),
    Direct(DirectTaskSubmission),
}

#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LegacyTaskStatusResponse {
    #[schema(value_type = String, format = "uuid")]
    pub task_uid: Uuid,
    #[schema(value_type = Option<String>)]
    pub runtime_uid: Option<String>,
    pub phase: TaskPhase,
    pub runtime_ownership: RuntimeOwnership,
    pub finalized: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schema(value_type = Vec<TaskAdmissionDelta>)]
    pub deltas: Vec<AdmissionDelta>,
}

#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(untagged)]
pub enum TaskStatusResponse {
    Existing(LegacyTaskStatusResponse),
    Direct(Box<DirectTaskStatusResponse>),
}

/// Machine-readable shape of an admission delta returned in Task status.
#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", tag = "dimension")]
pub enum TaskAdmissionDelta {
    Budget {
        requested: String,
        ceiling: String,
        currency: String,
    },
    Ttl {
        requested: String,
        ceiling: String,
    },
    Models {
        requested: Vec<ModelRef>,
        ceiling: Vec<ModelRef>,
    },
    Tools {
        requested: Vec<ToolGrant>,
        ceiling: Vec<ToolGrant>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TaskErrorResponse {
    pub error: String,
}

#[derive(utoipa::ToSchema)]
#[schema(
    value_type = String,
    format = Binary,
    description = "Opaque tar archive, limited to 67,108,864 raw bytes"
)]
pub struct TaskArchive(pub Vec<u8>);

#[derive(Clone)]
struct TaskApiState<L, I, W> {
    identities: I,
    application: TaskApplicationService<L, W>,
}

/// The single internal application boundary for Task resolution, admission, reservation,
/// and immutable execution-plan snapshotting.
#[derive(Clone)]
struct TaskApplicationService<L, W> {
    ledger: L,
    workflows: W,
    config: TaskApiConfig,
}

pub fn task_router<L, I, W>(ledger: L, identities: I, workflows: W, config: TaskApiConfig) -> Router
where
    L: AdmissionLedger + TaskSubmissionLedger,
    I: TaskIdentityResolver,
    W: TaskWorkflowCatalog,
{
    Router::new()
        .route("/v1/tasks", post(submit_task::<L, I, W>))
        .route(
            "/v1/tasks/{task_uid}/inputs",
            put(put_task_inputs::<L, I, W>),
        )
        .route(
            "/v1/tasks/{task_uid}/execute",
            post(execute_task::<L, I, W>),
        )
        .route(
            "/v1/tasks/{task_uid}/outputs",
            get(get_task_outputs::<L, I, W>),
        )
        .route(
            "/v1/tasks/{task_uid}",
            get(get_task::<L, I, W>).delete(delete_task::<L, I, W>),
        )
        .layer(DefaultBodyLimit::max(MAX_TASK_INPUT_ARCHIVE_BYTES))
        .with_state(TaskApiState {
            identities,
            application: TaskApplicationService {
                ledger,
                workflows,
                config,
            },
        })
}

async fn get_task_outputs<L, I, W>(
    State(state): State<TaskApiState<L, I, W>>,
    Path(task_uid): Path<Uuid>,
    headers: HeaderMap,
) -> Response
where
    L: AdmissionLedger + TaskSubmissionLedger,
    I: TaskIdentityResolver,
    W: TaskWorkflowCatalog,
{
    let identity = match resolve_task_identity(&state.identities, &headers).await {
        Ok(identity) => identity,
        Err(error) => return error.into_response(),
    };
    let record = match state
        .application
        .ledger
        .task_for_submitter(
            task_uid,
            &identity.service,
            identity.canonical_user_id.as_str(),
        )
        .await
    {
        Ok(Some(record)) => record,
        Ok(None) => return ApiError::Store(StoreError::TaskNotFound).into_response(),
        Err(error) => return ApiError::Store(error).into_response(),
    };
    if record.phase != TaskPhase::Succeeded {
        return ApiError::TaskOutputNotReady.into_response();
    }
    match record.output_archive {
        Some(archive) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/x-tar")],
            archive,
        )
            .into_response(),
        None => ApiError::TaskOutputNotReady.into_response(),
    }
}

async fn delete_task<L, I, W>(
    State(state): State<TaskApiState<L, I, W>>,
    Path(task_uid): Path<Uuid>,
    headers: HeaderMap,
) -> Response
where
    L: AdmissionLedger + TaskSubmissionLedger,
    I: TaskIdentityResolver,
    W: TaskWorkflowCatalog,
{
    let identity = match resolve_task_identity(&state.identities, &headers).await {
        Ok(identity) => identity,
        Err(error) => return error.into_response(),
    };
    match state
        .application
        .ledger
        .request_task_finalization(
            task_uid,
            &identity.service,
            identity.canonical_user_id.as_str(),
        )
        .await
    {
        Ok(record) => match status_response(&state.application.ledger, record, Vec::new()).await {
            Ok(response) => (StatusCode::ACCEPTED, Json(response)).into_response(),
            Err(error) => error.into_response(),
        },
        Err(error) => ApiError::Store(error).into_response(),
    }
}

async fn execute_task<L, I, W>(
    State(state): State<TaskApiState<L, I, W>>,
    Path(task_uid): Path<Uuid>,
    headers: HeaderMap,
) -> Response
where
    L: AdmissionLedger + TaskSubmissionLedger,
    I: TaskIdentityResolver,
    W: TaskWorkflowCatalog,
{
    let identity = match resolve_task_identity(&state.identities, &headers).await {
        Ok(identity) => identity,
        Err(error) => return error.into_response(),
    };
    match state
        .application
        .ledger
        .request_task_execution(
            task_uid,
            &identity.service,
            identity.canonical_user_id.as_str(),
        )
        .await
    {
        Ok(record) => match status_response(&state.application.ledger, record, Vec::new()).await {
            Ok(response) => (StatusCode::ACCEPTED, Json(response)).into_response(),
            Err(error) => error.into_response(),
        },
        Err(error) => ApiError::Store(error).into_response(),
    }
}

async fn get_task<L, I, W>(
    State(state): State<TaskApiState<L, I, W>>,
    Path(task_uid): Path<Uuid>,
    headers: HeaderMap,
) -> Response
where
    L: AdmissionLedger + TaskSubmissionLedger,
    I: TaskIdentityResolver,
    W: TaskWorkflowCatalog,
{
    let identity = match resolve_task_identity(&state.identities, &headers).await {
        Ok(identity) => identity,
        Err(error) => return error.into_response(),
    };
    let record = match state
        .application
        .ledger
        .task_for_submitter(
            task_uid,
            &identity.service,
            identity.canonical_user_id.as_str(),
        )
        .await
    {
        Ok(Some(record)) => record,
        Ok(None) => return ApiError::Store(StoreError::TaskNotFound).into_response(),
        Err(error) => return ApiError::Store(error).into_response(),
    };
    match status_response(&state.application.ledger, record, Vec::new()).await {
        Ok(response) => (StatusCode::OK, Json(response)).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn put_task_inputs<L, I, W>(
    State(state): State<TaskApiState<L, I, W>>,
    Path(task_uid): Path<Uuid>,
    headers: HeaderMap,
    archive: Result<Bytes, BytesRejection>,
) -> Response
where
    L: AdmissionLedger + TaskSubmissionLedger,
    I: TaskIdentityResolver,
    W: TaskWorkflowCatalog,
{
    let archive = match archive {
        Ok(archive) => archive,
        Err(rejection) => {
            let status = rejection.status();
            let error = if status == StatusCode::PAYLOAD_TOO_LARGE {
                "Task input archive exceeds the 64 MiB limit"
            } else {
                "Task input archive body could not be read"
            };
            return (status, Json(serde_json::json!({"error": error}))).into_response();
        }
    };
    if headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        != Some("application/x-tar")
    {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Json(serde_json::json!({"error": "Content-Type must be application/x-tar"})),
        )
            .into_response();
    }
    let identity = match resolve_task_identity(&state.identities, &headers).await {
        Ok(identity) => identity,
        Err(error) => return error.into_response(),
    };
    match state
        .application
        .ledger
        .put_task_inputs(
            task_uid,
            &identity.service,
            identity.canonical_user_id.as_str(),
            &archive,
        )
        .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => ApiError::Store(error).into_response(),
    }
}

async fn submit_task<L, I, W>(
    State(state): State<TaskApiState<L, I, W>>,
    headers: HeaderMap,
    Json(request): Json<TaskCreateRequest>,
) -> Response
where
    L: AdmissionLedger + TaskSubmissionLedger,
    I: TaskIdentityResolver,
    W: TaskWorkflowCatalog,
{
    let idempotency_key = match headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
    {
        Some(value) => value,
        None => {
            return ApiError::Admission("Idempotency-Key is required".to_owned()).into_response();
        }
    };
    let identity = match resolve_task_identity(&state.identities, &headers).await {
        Ok(identity) => identity,
        Err(error) => return error.into_response(),
    };
    let result = match request {
        TaskCreateRequest::Existing(request) => {
            state
                .application
                .submit(idempotency_key, identity, &request)
                .await
        }
        TaskCreateRequest::Direct(request) => {
            state
                .application
                .submit_direct(idempotency_key, identity, &request)
                .await
        }
    };
    match result {
        Ok((status, response)) => (status, Json(response)).into_response(),
        Err(error) => error.into_response(),
    }
}

impl<L, W> TaskApplicationService<L, W>
where
    L: AdmissionLedger + TaskSubmissionLedger,
    W: TaskWorkflowCatalog,
{
    async fn submit_direct(
        &self,
        idempotency_key: &str,
        identity: TaskIdentity,
        request: &DirectTaskSubmission,
    ) -> Result<(StatusCode, TaskStatusResponse), ApiError> {
        request.validate().map_err(ApiError::Admission)?;
        let existing = self
            .ledger
            .task_by_idempotency(
                &identity.service,
                identity.canonical_user_id.as_str(),
                idempotency_key,
            )
            .await
            .map_err(ApiError::Store)?;
        let task_uid = existing
            .as_ref()
            .map_or_else(Uuid::new_v4, |record| record.task_uid);
        let pre_admission =
            resolve_direct_task_pre_admission(&self.ledger, &self.config, &identity, request)
                .await?;
        let evidence = direct_task_evidence(
            task_uid,
            identity
                .source_provenance
                .clone()
                .ok_or(ApiError::TaskAuthentication)?,
            &pre_admission,
        )?;
        let DirectTaskPreAdmission {
            definition,
            envelope,
            spec,
            command,
            execution_binding,
            ..
        } = pre_admission;
        if let Some(record) = existing {
            validate_direct_task_retry(&identity, &evidence, &record)?;
            return Ok((
                StatusCode::OK,
                status_response(&self.ledger, record, Vec::new()).await?,
            ));
        }
        if !self.config.orchestration_mode.is_active() {
            return Err(ApiError::TaskRuntimeContractUnavailable(
                "Task submission is disabled during the staged orchestration rollout".to_owned(),
            ));
        }
        let approved = envelope
            .approved_envelope
            .as_ref()
            .ok_or(ApiError::MissingEnvelope)?;
        let envelope_instance_id = envelope
            .envelope_instance_id
            .as_deref()
            .ok_or(ApiError::MissingEnvelope)?;
        let envelope_digest = envelope
            .envelope_digest
            .as_deref()
            .ok_or(ApiError::MissingEnvelope)?;
        let service_envelope = self
            .ledger
            .latest_service_envelope(&identity.service)
            .await
            .map_err(ApiError::Store)?
            .ok_or(ApiError::MissingEnvelope)?;
        let decision = evaluate_with_grants(&spec, &service_envelope, &[])
            .map_err(|error| ApiError::Admission(format!("{error:?}")))?;
        let operation_id = Uuid::new_v4();
        let runtime_name = stable_task_runtime_name(operation_id);
        let orchestration = task_orchestration_reservation(
            task_uid,
            operation_id,
            VERSIONED_WORKFLOW_NAMESPACE,
            &runtime_name,
            &spec,
            &service_envelope,
            Some(&execution_binding),
        )?;
        let workflow = format!("direct:{}@{}", definition.name.as_str(), definition.version);
        let reservation = self
            .ledger
            .reserve_task(TaskReservationRequest {
                task_uid,
                operation_id,
                idempotency_key,
                submitter_service: &identity.service,
                acting_user: identity.acting_user.as_ref().map(|email| email.0.as_str()),
                acting_user_id: identity
                    .acting_user
                    .as_ref()
                    .map(|_| identity.canonical_user_id.as_str()),
                owner: &identity.owner.0,
                owner_user_id: identity.canonical_user_id.as_str(),
                workflow: &workflow,
                workflow_name: None,
                workflow_version: None,
                workflow_digest: None,
                user_envelope_instance_id: Some(envelope_instance_id),
                user_envelope_revision: Some(approved.revision),
                user_envelope_digest: Some(envelope_digest),
                coding_agent_runtime: definition.runtime.agent_ref.as_str(),
                runtime_uid: None,
                runtime_namespace: VERSIONED_WORKFLOW_NAMESPACE,
                runtime_name: &runtime_name,
                runtime_ownership: RuntimeOwnership::Provisioned,
                runtime_spec: &spec,
                agent_command: &command,
                execution_binding: Some(&execution_binding),
                direct_task_evidence: Some(&evidence),
                envelope_revision: service_envelope.revision,
                service_envelope: &service_envelope,
                service_envelope_digest: &orchestration.service_envelope_digest,
                candidate_digest: &orchestration.candidate_digest,
                admission_decision: &decision,
                inert_manifest_digest: &orchestration.inert_manifest_digest,
                active_manifest_digest: &orchestration.active_manifest_digest,
            })
            .await;
        let deltas = admission_deltas(&decision);
        let record = match reservation {
            Ok(reservation) if reservation.inserted => {
                return task_response(&self.ledger, reservation.record, deltas).await;
            }
            Ok(reservation) => reservation.record,
            Err(StoreError::TaskIdempotencyConflict) => self
                .ledger
                .task_by_idempotency(
                    &identity.service,
                    identity.canonical_user_id.as_str(),
                    idempotency_key,
                )
                .await
                .map_err(ApiError::Store)?
                .ok_or(ApiError::Store(StoreError::TaskIdempotencyConflict))?,
            Err(error) => return Err(ApiError::Store(error)),
        };
        validate_direct_task_retry(&identity, &evidence, &record)?;
        Ok((
            StatusCode::OK,
            status_response(&self.ledger, record, deltas).await?,
        ))
    }

    async fn submit(
        &self,
        idempotency_key: &str,
        identity: TaskIdentity,
        request: &TaskSubmissionRequest,
    ) -> Result<(StatusCode, TaskStatusResponse), ApiError> {
        let reference = versioned_workflow_reference(
            &request.workflow,
            request.coding_agent_runtime.as_deref(),
        )?;
        if reference.is_none() && request.coding_agent_runtime.is_none() {
            return Err(ApiError::Admission(
                "legacy workflows require codingAgentRuntime".to_owned(),
            ));
        }
        if let Some(record) = self
            .ledger
            .task_by_idempotency(
                &identity.service,
                identity.canonical_user_id.as_str(),
                idempotency_key,
            )
            .await
            .map_err(ApiError::Store)?
        {
            return self
                .retry_existing_task(&identity, reference.as_ref(), request, record)
                .await;
        }
        if !self.config.orchestration_mode.is_active() {
            return Err(ApiError::TaskRuntimeContractUnavailable(
                "Task submission is disabled during the staged orchestration rollout".to_owned(),
            ));
        }
        if let Some(reference) = reference {
            return submit_versioned_task(self, idempotency_key, identity, reference, request)
                .await;
        }
        let workflow = self
            .workflows
            .workflow(&request.workflow)
            .ok_or(ApiError::TaskWorkflowNotFound)?;
        let coding_agent_runtime = request.coding_agent_runtime.as_deref().ok_or_else(|| {
            ApiError::Admission("legacy workflows require codingAgentRuntime".to_owned())
        })?;
        if coding_agent_runtime != workflow.coding_agent_runtime {
            return Err(ApiError::Admission(
                "codingAgentRuntime is not selected by the workflow".to_owned(),
            ));
        }
        let spec = AgentRuntimeSpec {
            principal: Principal::Service {
                name: identity.service.clone(),
                acting_user: identity.acting_user.clone(),
            },
            owner: identity.owner.clone(),
            canonical_authority: Some(
                CanonicalAuthorityBinding::new(
                    identity.canonical_user_id.clone(),
                    identity
                        .acting_user
                        .as_ref()
                        .map(|_| identity.canonical_user_id.clone()),
                )
                .map_err(ApiError::Admission)?,
            ),
            agent_type: steward_types::AgentType {
                name: workflow.coding_agent_runtime.clone(),
            },
            llms: workflow.llms.clone(),
            tools: workflow.tools.clone(),
            budget: workflow.budget.clone(),
            ttl: workflow.ttl.clone(),
            runner: steward_types::RunnerRequirements::default(),
            bindings: None,
        };
        let envelope = self
            .ledger
            .latest_service_envelope(&identity.service)
            .await
            .map_err(ApiError::Store)?
            .ok_or(ApiError::MissingEnvelope)?;
        let decision = evaluate_with_grants(&spec, &envelope, &[])
            .map_err(|error| ApiError::Admission(format!("{error:?}")))?;
        if let Some(runtime_uid) = request.agent_runtime_uid.as_deref() {
            if !matches!(decision, AdmissionDecision::Admit) {
                return Err(ApiError::Admission(
                    "adopted runtime is outside the current service envelope".to_owned(),
                ));
            }
            let resolver = self
                .config
                .legacy_runtime_resolver
                .as_ref()
                .ok_or_else(|| {
                    ApiError::TaskRuntimeContractUnavailable(
                        "legacy adopted-runtime resolution is unavailable".to_owned(),
                    )
                })?;
            let runtime = resolver
                .get_by_uid(runtime_uid)
                .await
                .map_err(ApiError::Runtime)?;
            let runtime_namespace = runtime
                .namespace()
                .ok_or_else(|| ApiError::Runtime("adopted runtime has no namespace".to_owned()))?;
            if runtime.metadata.uid.as_deref() != Some(runtime_uid)
                || runtime_namespace != workflow.namespace
                || runtime.spec != spec
                || runtime
                    .annotations()
                    .contains_key(PENDING_APPROVAL_ANNOTATION)
            {
                return Err(ApiError::Conflict(
                    "adopted runtime does not match the resolved workflow and principal".to_owned(),
                ));
            }
            let runtime_name = runtime.name_any();
            let task_uid = Uuid::new_v4();
            let operation_id = Uuid::new_v4();
            let orchestration = task_orchestration_reservation(
                task_uid,
                operation_id,
                &runtime_namespace,
                &runtime_name,
                &spec,
                &envelope,
                None,
            )?;
            let reservation = self
                .ledger
                .reserve_task(TaskReservationRequest {
                    task_uid,
                    operation_id,
                    idempotency_key,
                    submitter_service: &identity.service,
                    acting_user: identity.acting_user.as_ref().map(|email| email.0.as_str()),
                    acting_user_id: identity
                        .acting_user
                        .as_ref()
                        .map(|_| identity.canonical_user_id.as_str()),
                    owner: &identity.owner.0,
                    owner_user_id: identity.canonical_user_id.as_str(),
                    workflow: &workflow.name,
                    workflow_name: None,
                    workflow_version: None,
                    workflow_digest: None,
                    user_envelope_instance_id: None,
                    user_envelope_revision: None,
                    user_envelope_digest: None,
                    coding_agent_runtime: &workflow.coding_agent_runtime,
                    runtime_uid: Some(runtime_uid),
                    runtime_namespace: &runtime_namespace,
                    runtime_name: &runtime_name,
                    runtime_ownership: RuntimeOwnership::Adopted,
                    runtime_spec: &spec,
                    agent_command: &workflow.command,
                    execution_binding: None,
                    direct_task_evidence: None,
                    envelope_revision: envelope.revision,
                    service_envelope: &envelope,
                    service_envelope_digest: &orchestration.service_envelope_digest,
                    candidate_digest: &orchestration.candidate_digest,
                    admission_decision: &decision,
                    inert_manifest_digest: &orchestration.inert_manifest_digest,
                    active_manifest_digest: &orchestration.active_manifest_digest,
                })
                .await;
            return self
                .finish_task_reservation(
                    &identity,
                    None,
                    request,
                    idempotency_key,
                    reservation,
                    Vec::new(),
                )
                .await;
        }
        let task_uid = Uuid::new_v4();
        let operation_id = Uuid::new_v4();
        let runtime_name = stable_task_runtime_name(operation_id);
        let orchestration = task_orchestration_reservation(
            task_uid,
            operation_id,
            &workflow.namespace,
            &runtime_name,
            &spec,
            &envelope,
            None,
        )?;
        let reservation = self
            .ledger
            .reserve_task(TaskReservationRequest {
                task_uid,
                operation_id,
                idempotency_key,
                submitter_service: &identity.service,
                acting_user: identity.acting_user.as_ref().map(|email| email.0.as_str()),
                acting_user_id: identity
                    .acting_user
                    .as_ref()
                    .map(|_| identity.canonical_user_id.as_str()),
                owner: &identity.owner.0,
                owner_user_id: identity.canonical_user_id.as_str(),
                workflow: &workflow.name,
                workflow_name: None,
                workflow_version: None,
                workflow_digest: None,
                user_envelope_instance_id: None,
                user_envelope_revision: None,
                user_envelope_digest: None,
                coding_agent_runtime: &workflow.coding_agent_runtime,
                runtime_uid: None,
                runtime_namespace: &workflow.namespace,
                runtime_name: &runtime_name,
                runtime_ownership: RuntimeOwnership::Provisioned,
                runtime_spec: &spec,
                agent_command: &workflow.command,
                execution_binding: None,
                direct_task_evidence: None,
                envelope_revision: envelope.revision,
                service_envelope: &envelope,
                service_envelope_digest: &orchestration.service_envelope_digest,
                candidate_digest: &orchestration.candidate_digest,
                admission_decision: &decision,
                inert_manifest_digest: &orchestration.inert_manifest_digest,
                active_manifest_digest: &orchestration.active_manifest_digest,
            })
            .await;
        let deltas = admission_deltas(&decision);
        self.finish_task_reservation(
            &identity,
            None,
            request,
            idempotency_key,
            reservation,
            deltas,
        )
        .await
    }

    async fn finish_task_reservation(
        &self,
        identity: &TaskIdentity,
        reference: Option<&WorkflowReference>,
        request: &TaskSubmissionRequest,
        idempotency_key: &str,
        reservation: Result<steward_store::TaskReservation, StoreError>,
        deltas: Vec<AdmissionDelta>,
    ) -> Result<(StatusCode, TaskStatusResponse), ApiError> {
        let record = match reservation {
            Ok(reservation) if reservation.inserted => {
                return task_response(&self.ledger, reservation.record, deltas).await;
            }
            Ok(reservation) => reservation.record,
            Err(StoreError::TaskIdempotencyConflict) => self
                .ledger
                .task_by_idempotency(
                    &identity.service,
                    identity.canonical_user_id.as_str(),
                    idempotency_key,
                )
                .await
                .map_err(ApiError::Store)?
                .ok_or(ApiError::Store(StoreError::TaskIdempotencyConflict))?,
            Err(error) => return Err(ApiError::Store(error)),
        };
        self.retry_existing_task(identity, reference, request, record)
            .await
    }

    async fn retry_existing_task(
        &self,
        identity: &TaskIdentity,
        reference: Option<&WorkflowReference>,
        request: &TaskSubmissionRequest,
        record: TaskRecord,
    ) -> Result<(StatusCode, TaskStatusResponse), ApiError> {
        let expected_runtime_uid = if request.agent_runtime_uid.is_some() {
            if record.orchestration_version == 1 {
                record.runtime_uid.clone()
            } else {
                self.ledger
                    .task_runtime_operation(record.task_uid)
                    .await
                    .map_err(ApiError::Store)?
                    .and_then(|operation| operation.expected_runtime_uid)
            }
        } else {
            None
        };
        validate_task_retry(
            identity,
            reference,
            request,
            expected_runtime_uid.as_deref(),
            &record,
        )?;
        let deltas = record.original_admission_deltas.clone().unwrap_or_default();
        if reference.is_some() {
            Ok((
                StatusCode::OK,
                status_response(&self.ledger, record, deltas).await?,
            ))
        } else {
            task_response(&self.ledger, record, deltas).await
        }
    }
}

async fn resolve_direct_task_pre_admission<L>(
    ledger: &L,
    config: &TaskApiConfig,
    identity: &TaskIdentity,
    request: &DirectTaskSubmission,
) -> Result<DirectTaskPreAdmission, ApiError>
where
    L: TaskSubmissionLedger,
{
    let provenance = identity
        .source_provenance
        .as_ref()
        .ok_or(ApiError::TaskAuthentication)?;
    provenance
        .validate()
        .map_err(|_| ApiError::TaskAuthentication)?;
    let git = config.direct_git_resolver.as_ref().ok_or_else(|| {
        ApiError::TaskRuntimeContractUnavailable(
            "direct package Git source resolver is unavailable".to_owned(),
        )
    })?;
    let invocation_repository = RepositoryUrl::parse(format!(
        "https://github.com/{}.git",
        provenance.repository.name.as_str()
    ))
    .map_err(|_| ApiError::TaskAuthentication)?;
    let invocation_identity = git
        .resolve_repository(&invocation_repository)
        .await
        .map_err(source_port_error)?;
    if invocation_identity.repository != invocation_repository
        || invocation_identity.repository_id != provenance.repository.id
        || invocation_identity.repository_owner_id != provenance.repository.owner_id
    {
        return Err(ApiError::TaskAuthentication);
    }
    let invocation_request = GitFileRequest {
        repository: invocation_identity.clone(),
        commit: provenance.triggered_sha.clone(),
        path: request.invocation_path.clone(),
        max_bytes: steward_types::direct_package::MAX_PACKAGE_FILE_BYTES,
    };
    let invocation_file = git
        .read_file(&invocation_request)
        .await
        .map_err(source_port_error)?;
    let invocation_bytes = verified_git_file(invocation_file, &invocation_request)?;
    let manifest = serde_json::from_slice::<InvocationManifest>(&invocation_bytes)
        .map_err(|_| ApiError::Admission("invocation manifest is invalid".to_owned()))?;
    manifest
        .validate_for_invoking_repository(&invocation_repository)
        .map_err(ApiError::Admission)?;

    let package_identity = git
        .resolve_repository(&manifest.package.repository)
        .await
        .map_err(source_port_error)?;
    let same_repository = package_identity == invocation_identity;
    if !same_repository
        && !config.source_repository_is_authorized(&provenance.repository, &package_identity)
        && !ledger
            .active_source_repository_binding(&provenance.repository, &package_identity)
            .await
            .map_err(ApiError::Store)?
    {
        return Err(ApiError::TaskSourceUnauthorized(
            "source repository is not authorized".to_owned(),
        ));
    }
    let package_commit = match &manifest.package.commit {
        PackageCommit::Exact(commit) => commit.clone(),
        PackageCommit::Trigger if same_repository => provenance.triggered_sha.clone(),
        PackageCommit::Trigger => {
            return Err(ApiError::Admission(
                "git:trigger is valid only for the invoking repository".to_owned(),
            ));
        }
    };
    let definition_request = GitFileRequest {
        repository: package_identity.clone(),
        commit: package_commit,
        path: manifest.package.path.clone(),
        max_bytes: steward_types::direct_package::MAX_PACKAGE_FILE_BYTES,
    };
    let definition_file = git
        .read_file(&definition_request)
        .await
        .map_err(source_port_error)?;
    let definition_bytes = verified_git_file(definition_file, &definition_request)?;
    let definition = serde_json::from_slice::<DirectTaskDefinition>(&definition_bytes)
        .map_err(|_| ApiError::Admission("direct TaskDefinition is invalid".to_owned()))?;
    definition.validate().map_err(ApiError::Admission)?;
    let (prompt, closure, closure_digest) = resolve_package_closure(
        git.as_ref(),
        &package_identity,
        &definition_request.commit,
        &manifest.package.path,
        &definition,
        &definition_bytes,
    )
    .await?;

    let invocation = resolved_source(
        &invocation_identity,
        &invocation_request.commit,
        &invocation_request.path,
        &canonical_json_bytes(&manifest).map_err(ApiError::Admission)?,
    )?;
    let package = resolved_source(
        &package_identity,
        &definition_request.commit,
        &definition_request.path,
        &canonical_json_bytes(&definition).map_err(ApiError::Admission)?,
    )?;

    let explicit_envelope = manifest.envelope.is_some();
    let envelope = resolve_direct_user_envelope(
        ledger,
        &identity.canonical_user_id,
        manifest.envelope.as_ref(),
    )
    .await?;
    let approved = envelope.approved_envelope.as_ref().ok_or_else(|| {
        ApiError::Admission(if explicit_envelope {
            "the selected Envelope is not active".to_owned()
        } else {
            "the resolved Envelope is not active".to_owned()
        })
    })?;
    let effective_requirements = match &definition.requires {
        Some(requirements) => requirements.clone(),
        None => direct_requirements_from_envelope(&approved.spec)?,
    };
    let spec = direct_runtime_spec(identity, &definition, &effective_requirements)?;
    if !matches!(
        evaluate_with_grants(&spec, approved, &[])
            .map_err(|error| ApiError::Admission(format!("{error:?}")))?,
        AdmissionDecision::Admit
    ) {
        return Err(ApiError::Admission(if explicit_envelope {
            "direct package requirements exceed the selected Envelope".to_owned()
        } else {
            "direct package requirements exceed the resolved Envelope".to_owned()
        }));
    }
    let (command, execution_binding) =
        resolve_direct_execution_plan(config, &definition, &prompt, &spec)?;
    Ok(DirectTaskPreAdmission {
        definition,
        invocation,
        package,
        closure,
        closure_digest,
        diagnostics: manifest.effective_diagnostics(),
        envelope,
        effective_requirements,
        spec,
        command,
        execution_binding,
    })
}

async fn resolve_direct_user_envelope<L>(
    ledger: &L,
    owner_user_id: &CanonicalUserId,
    selector: Option<&EnvelopeDigest>,
) -> Result<EnvelopeRequestRecord, ApiError>
where
    L: TaskSubmissionLedger,
{
    let mut envelopes = match selector {
        Some(digest) => ledger
            .active_provisioned_user_envelopes_by_digest(owner_user_id, digest)
            .await
            .map_err(ApiError::Store)?,
        None => ledger
            .active_provisioned_user_envelopes(owner_user_id)
            .await
            .map_err(ApiError::Store)?,
    };
    if envelopes.len() != 1 {
        return match (selector, envelopes.is_empty()) {
            (Some(_), true) => Err(ApiError::Admission(
                "the selected Envelope is not active".to_owned(),
            )),
            (Some(_), false) => Err(ApiError::Conflict(
                "multiple active Envelopes have the selected digest".to_owned(),
            )),
            (None, true) => Err(ApiError::Admission(
                "the authenticated user has no active provisioned User Envelope".to_owned(),
            )),
            (None, false) => Err(ApiError::Conflict(
                "multiple active provisioned User Envelopes are ambiguous".to_owned(),
            )),
        };
    }
    let envelope = envelopes
        .pop()
        .ok_or_else(|| ApiError::Admission("User Envelope resolution failed closed".to_owned()))?;
    if envelope.owner_user_id != *owner_user_id
        || envelope.status != steward_store::EnvelopeRequestStatus::Provisioned
        || envelope.envelope_instance_id.is_none()
        || envelope.envelope_digest.is_none()
    {
        return Err(ApiError::Admission(if selector.is_some() {
            "the selected Envelope is not active".to_owned()
        } else {
            "the resolved Envelope is not active".to_owned()
        }));
    }
    Ok(envelope)
}

fn direct_task_evidence(
    task_uid: Uuid,
    source_provenance: SourceProvenance,
    pre_admission: &DirectTaskPreAdmission,
) -> Result<DirectTaskBindingEvidence, ApiError> {
    let approved = pre_admission
        .envelope
        .approved_envelope
        .as_ref()
        .ok_or(ApiError::MissingEnvelope)?;
    let revision = u64::try_from(approved.revision).map_err(|_| ApiError::MissingEnvelope)?;
    let digest = pre_admission
        .envelope
        .envelope_digest
        .as_deref()
        .ok_or(ApiError::MissingEnvelope)?;
    let evidence = DirectTaskBindingEvidence {
        schema_version: TASK_BINDING_EVIDENCE_SCHEMA.to_owned(),
        task_uid: steward_types::direct_package::Uuid::parse(task_uid.to_string())
            .map_err(ApiError::Admission)?,
        source_provenance,
        invocation: pre_admission.invocation.clone(),
        package: pre_admission.package.clone(),
        closure: pre_admission.closure.clone(),
        closure_digest: pre_admission.closure_digest.clone(),
        envelope: EnvelopeEvidence {
            uid: steward_types::direct_package::Uuid::parse(pre_admission.envelope.id.to_string())
                .map_err(ApiError::Admission)?,
            revision,
            digest: EnvelopeDigest::parse(format!("steward:{digest}"))
                .map_err(ApiError::Admission)?,
        },
        effective_requirements: pre_admission.effective_requirements.clone(),
        diagnostics: pre_admission.diagnostics,
    };
    evidence.validate().map_err(ApiError::Admission)?;
    Ok(evidence)
}

fn validate_direct_task_retry(
    identity: &TaskIdentity,
    evidence: &DirectTaskBindingEvidence,
    record: &TaskRecord,
) -> Result<(), ApiError> {
    let acting_user = identity.acting_user.as_ref().map(|email| email.0.as_str());
    let acting_user_id = identity
        .acting_user
        .as_ref()
        .map(|_| identity.canonical_user_id.as_str());
    if record.identity_binding_state != "bound"
        || record.submitter_service != identity.service
        || record.acting_user.as_deref() != acting_user
        || record.acting_user_id.as_deref() != acting_user_id
        || record.owner != identity.owner.0
        || record.owner_user_id.as_deref() != Some(identity.canonical_user_id.as_str())
        || record.runtime_ownership != RuntimeOwnership::Provisioned
        || record.workflow_name.is_some()
        || record.workflow_version.is_some()
        || record.workflow_digest.is_some()
        || record.direct_task_evidence.as_ref() != Some(evidence)
    {
        return Err(ApiError::Store(StoreError::TaskIdempotencyConflict));
    }
    Ok(())
}

async fn resolve_package_closure(
    git: &dyn DirectGitResolver,
    repository: &GitRepositoryIdentity,
    commit: &steward_types::direct_package::ExactGitCommit,
    entry_point: &RelativePath,
    definition: &DirectTaskDefinition,
    definition_bytes: &[u8],
) -> Result<(String, PackageClosure, ContentDigest), ApiError> {
    let package_root = containing_directory(entry_point.as_str());
    let mut entries = BTreeMap::<String, ClosureEntry>::new();
    let canonical_definition = canonical_json_bytes(definition).map_err(ApiError::Admission)?;
    insert_closure_entry(
        &mut entries,
        ClosureEntryKind::TaskDefinition,
        entry_point.clone(),
        &canonical_definition,
    )?;
    if definition_bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        return Err(ApiError::Admission(
            "direct TaskDefinition must not contain a UTF-8 BOM".to_owned(),
        ));
    }

    let prompt_path = resolve_package_relative_path(
        package_root,
        containing_directory(entry_point.as_str()),
        &definition.prompt,
    )?;
    let prompt_bytes = read_package_file(git, repository, commit, &prompt_path).await?;
    insert_closure_entry(
        &mut entries,
        ClosureEntryKind::Prompt,
        prompt_path,
        &prompt_bytes,
    )?;
    let prompt = std::str::from_utf8(&prompt_bytes)
        .map_err(|_| ApiError::Admission("direct package prompt must be UTF-8".to_owned()))?
        .to_owned();

    let mut rendered_prompt = prompt;
    for skill_reference in &definition.skills {
        let skill_path = resolve_package_relative_path(
            package_root,
            containing_directory(entry_point.as_str()),
            skill_reference,
        )?;
        let skill_bytes = read_package_file(git, repository, commit, &skill_path).await?;
        if skill_bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
            return Err(ApiError::Admission(
                "instruction skill descriptor must not contain a UTF-8 BOM".to_owned(),
            ));
        }
        let skill = serde_json::from_slice::<InstructionSkill>(&skill_bytes)
            .map_err(|_| ApiError::Admission("instruction skill is invalid".to_owned()))?;
        skill.validate().map_err(ApiError::Admission)?;
        let canonical_skill = canonical_json_bytes(&skill).map_err(ApiError::Admission)?;
        insert_closure_entry(
            &mut entries,
            ClosureEntryKind::InstructionSkill,
            skill_path.clone(),
            &canonical_skill,
        )?;

        let skill_root = containing_directory(skill_path.as_str());
        let instructions_path =
            resolve_package_relative_path(package_root, skill_root, &skill.instructions)?;
        let instructions = read_package_file(git, repository, commit, &instructions_path).await?;
        insert_closure_entry(
            &mut entries,
            ClosureEntryKind::Instructions,
            instructions_path,
            &instructions,
        )?;
        let instructions = std::str::from_utf8(&instructions).map_err(|_| {
            ApiError::Admission("instruction skill instructions must be UTF-8".to_owned())
        })?;
        rendered_prompt.push_str("\n\n## Instruction skill: ");
        rendered_prompt.push_str(skill.name.as_str());
        rendered_prompt.push_str("\n\n");
        rendered_prompt.push_str(skill.description.as_str());
        rendered_prompt.push_str("\n\n");
        rendered_prompt.push_str(instructions);

        for asset in &skill.assets {
            let asset_path = resolve_package_relative_path(package_root, skill_root, asset)?;
            let asset_bytes = read_package_file(git, repository, commit, &asset_path).await?;
            insert_closure_entry(
                &mut entries,
                ClosureEntryKind::Asset,
                asset_path,
                &asset_bytes,
            )?;
        }
    }

    let closure = PackageClosure {
        contract_version: PACKAGE_CLOSURE_CONTRACT_VERSION.to_owned(),
        entry_point: entry_point.clone(),
        entries: entries.into_values().collect(),
    };
    closure.validate().map_err(ApiError::Admission)?;
    let closure_digest =
        content_digest(&canonical_json_bytes(&closure).map_err(ApiError::Admission)?)?;
    Ok((rendered_prompt, closure, closure_digest))
}

fn containing_directory(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(directory, _)| directory)
}

fn resolve_package_relative_path(
    package_root: &str,
    reference_root: &str,
    reference: &RelativePath,
) -> Result<RelativePath, ApiError> {
    let resolved = if reference_root.is_empty() {
        reference.as_str().to_owned()
    } else {
        format!("{reference_root}/{}", reference.as_str())
    };
    let resolved = RelativePath::parse(resolved).map_err(ApiError::Admission)?;
    if !package_root.is_empty()
        && resolved.as_str() != package_root
        && !resolved.as_str().starts_with(&format!("{package_root}/"))
    {
        return Err(ApiError::Admission(
            "direct package dependency escapes the package root".to_owned(),
        ));
    }
    Ok(resolved)
}

async fn read_package_file(
    git: &dyn DirectGitResolver,
    repository: &GitRepositoryIdentity,
    commit: &steward_types::direct_package::ExactGitCommit,
    path: &RelativePath,
) -> Result<Vec<u8>, ApiError> {
    let request = GitFileRequest {
        repository: repository.clone(),
        commit: commit.clone(),
        path: path.clone(),
        max_bytes: steward_types::direct_package::MAX_PACKAGE_FILE_BYTES,
    };
    let file = git.read_file(&request).await.map_err(source_port_error)?;
    verified_git_file(file, &request)
}

fn insert_closure_entry(
    entries: &mut BTreeMap<String, ClosureEntry>,
    kind: ClosureEntryKind,
    path: RelativePath,
    bytes: &[u8],
) -> Result<(), ApiError> {
    let path_key = path.as_str().to_owned();
    let entry = ClosureEntry {
        kind,
        path,
        digest: content_digest(bytes)?,
        size_bytes: u64::try_from(bytes.len()).map_err(|_| {
            ApiError::Admission("direct package file size cannot be represented".to_owned())
        })?,
    };
    if entries.insert(path_key, entry).is_some() {
        return Err(ApiError::Admission(
            "direct package contains a duplicate logical path".to_owned(),
        ));
    }
    Ok(())
}

fn content_digest(bytes: &[u8]) -> Result<ContentDigest, ApiError> {
    ContentDigest::parse(format!("steward:sha256:{:x}", Sha256::digest(bytes)))
        .map_err(ApiError::Admission)
}

fn resolved_source(
    repository: &GitRepositoryIdentity,
    commit: &steward_types::direct_package::ExactGitCommit,
    path: &RelativePath,
    canonical_bytes: &[u8],
) -> Result<ResolvedSource, ApiError> {
    Ok(ResolvedSource {
        repository: repository.repository.clone(),
        repository_id: repository.repository_id.clone(),
        repository_owner_id: repository.repository_owner_id.clone(),
        commit: commit.clone(),
        path: path.clone(),
        content_digest: content_digest(canonical_bytes)?,
    })
}

fn resolve_direct_execution_plan(
    config: &TaskApiConfig,
    definition: &DirectTaskDefinition,
    prompt: &str,
    spec: &AgentRuntimeSpec,
) -> Result<(Vec<String>, TaskExecutionBinding), ApiError> {
    let agent_ref = definition.runtime.agent_ref.as_str();
    let [model] = spec.llms.as_slice() else {
        return Err(ApiError::Admission(
            "direct Tasks require exactly one approved model".to_owned(),
        ));
    };
    let binding = config
        .execution_bindings_active
        .then(|| config.execution_bindings.resolve(agent_ref))
        .flatten()
        .cloned()
        .ok_or_else(|| {
            ApiError::TaskRuntimeContractUnavailable(format!(
                "logical agent {agent_ref} has no deployment execution binding"
            ))
        })?;
    if binding.provider_profiles.inference.is_none() {
        return Err(ApiError::TaskRuntimeContractUnavailable(format!(
            "logical agent {agent_ref} has no inference provider profile"
        )));
    }
    if !spec.tools.is_empty() && binding.provider_profiles.tools.is_none() {
        return Err(ApiError::TaskRuntimeContractUnavailable(format!(
            "logical agent {agent_ref} has no tool provider profile"
        )));
    }
    let adapter = config
        .execution_adapters
        .get(&binding.adapter)
        .ok_or_else(|| {
            ApiError::TaskRuntimeContractUnavailable(format!(
                "logical agent {agent_ref} uses an unavailable execution adapter"
            ))
        })?;
    let tool_transport_endpoint = if spec.tools.is_empty() {
        None
    } else {
        Some(config.tool_transport_endpoint.as_deref().ok_or_else(|| {
            ApiError::TaskRuntimeContractUnavailable(
                "tool-bearing direct Task requires a tool transport endpoint".to_owned(),
            )
        })?)
    };
    let command = adapter
        .render(TaskExecutionPlanRequest {
            workflow_prompt: prompt,
            model,
            tools: &spec.tools,
            tool_transport_endpoint,
            binding: &binding,
        })
        .map_err(|error| {
            ApiError::TaskRuntimeContractUnavailable(format!(
                "logical agent {agent_ref} execution plan could not be rendered: {error:?}"
            ))
        })?
        .command;
    Ok((command, TaskExecutionBinding::Disposable(binding)))
}

fn source_port_error(error: steward_ports::PortError) -> ApiError {
    match error {
        steward_ports::PortError::Rejected { .. } => {
            ApiError::Admission("exact source object could not be resolved".to_owned())
        }
        steward_ports::PortError::Unsupported { .. } | steward_ports::PortError::Failed { .. } => {
            ApiError::TaskRuntimeContractUnavailable(
                "direct package Git source resolver is unavailable".to_owned(),
            )
        }
        _ => ApiError::TaskRuntimeContractUnavailable(
            "direct package Git source resolver is unavailable".to_owned(),
        ),
    }
}

fn verified_git_file(file: GitFile, request: &GitFileRequest) -> Result<Vec<u8>, ApiError> {
    if file.repository != request.repository
        || file.commit != request.commit
        || file.path != request.path
        || file.bytes.len() as u64 > request.max_bytes
    {
        return Err(ApiError::Admission(
            "exact source object does not match the requested Git object".to_owned(),
        ));
    }
    Ok(file.bytes)
}

fn direct_requirements_from_envelope(spec: &EnvelopeSpec) -> Result<DirectRequirements, ApiError> {
    let mut authority = serde_json::to_value(spec)
        .map_err(|error| ApiError::Admission(format!("Envelope cannot be projected: {error}")))?;
    let budget = authority
        .get_mut("budget")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or_else(|| ApiError::Admission("Envelope budget is invalid".to_owned()))?;
    budget
        .entry("singleRunLimit".to_owned())
        .or_insert(serde_json::Value::Null);
    let runner = authority
        .get_mut("runner")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or_else(|| {
            ApiError::Admission("Envelope runner requirements are invalid".to_owned())
        })?;
    for field in ["memory", "compute", "storage"] {
        runner
            .entry(field.to_owned())
            .or_insert(serde_json::Value::Null);
    }
    serde_json::from_value(serde_json::json!({"authority": authority}))
        .map_err(|error| ApiError::Admission(format!("Envelope cannot be projected: {error}")))
}

fn direct_runtime_spec(
    identity: &TaskIdentity,
    definition: &DirectTaskDefinition,
    requirements: &DirectRequirements,
) -> Result<AgentRuntimeSpec, ApiError> {
    let authority = serde_json::to_value(&requirements.authority).map_err(|error| {
        ApiError::Admission(format!("direct requirements cannot be projected: {error}"))
    })?;
    let envelope_spec = serde_json::from_value::<EnvelopeSpec>(authority).map_err(|error| {
        ApiError::Admission(format!("direct requirements cannot be projected: {error}"))
    })?;
    let canonical_authority = CanonicalAuthorityBinding::new(
        identity.canonical_user_id.clone(),
        identity
            .acting_user
            .as_ref()
            .map(|_| identity.canonical_user_id.clone()),
    )
    .map_err(ApiError::Admission)?;
    Ok(AgentRuntimeSpec {
        principal: Principal::Service {
            name: identity.service.clone(),
            acting_user: identity.acting_user.clone(),
        },
        owner: identity.owner.clone(),
        canonical_authority: Some(canonical_authority),
        agent_type: steward_types::AgentType {
            name: definition.runtime.agent_ref.as_str().to_owned(),
        },
        llms: envelope_spec.llms,
        tools: envelope_spec.tools,
        budget: envelope_spec.budget,
        ttl: envelope_spec.ttl,
        runner: envelope_spec.runner,
        bindings: None,
    })
}

async fn submit_versioned_task<L, W>(
    application: &TaskApplicationService<L, W>,
    idempotency_key: &str,
    identity: TaskIdentity,
    reference: WorkflowReference,
    request: &TaskSubmissionRequest,
) -> Result<(StatusCode, TaskStatusResponse), ApiError>
where
    L: AdmissionLedger + TaskSubmissionLedger,
    W: TaskWorkflowCatalog,
{
    if request.agent_runtime_uid.is_some() {
        return Err(ApiError::Admission(
            "versioned Workflows use a server-owned runtime path".to_owned(),
        ));
    }
    let workflow = application
        .ledger
        .workflow_revision(&reference.name, reference.version)
        .await
        .map_err(ApiError::Store)?
        .ok_or(ApiError::TaskWorkflowNotFound)?;
    let envelopes = application
        .ledger
        .active_provisioned_user_envelopes(&identity.canonical_user_id)
        .await
        .map_err(ApiError::Store)?;
    let plan = resolve_versioned_task_plan(&identity, workflow, envelopes, &application.config)?;
    let user_envelope = plan
        .envelope
        .approved_envelope
        .as_ref()
        .ok_or(ApiError::MissingEnvelope)?;
    if !matches!(
        evaluate_with_grants(&plan.spec, user_envelope, &[])
            .map_err(|error| ApiError::Admission(format!("{error:?}")))?,
        AdmissionDecision::Admit
    ) {
        return Err(ApiError::Admission(
            "Workflow runtime exceeds its pinned User Envelope".to_owned(),
        ));
    }
    let service_envelope = application
        .ledger
        .latest_service_envelope(&identity.service)
        .await
        .map_err(ApiError::Store)?
        .ok_or(ApiError::MissingEnvelope)?;
    let decision = evaluate_with_grants(&plan.spec, &service_envelope, &[])
        .map_err(|error| ApiError::Admission(format!("{error:?}")))?;
    let workflow_reference = format!("{}@{}", plan.workflow.name, plan.workflow.version);
    let task_uid = Uuid::new_v4();
    let operation_id = Uuid::new_v4();
    let runtime_name = stable_task_runtime_name(operation_id);
    let orchestration = task_orchestration_reservation(
        task_uid,
        operation_id,
        VERSIONED_WORKFLOW_NAMESPACE,
        &runtime_name,
        &plan.spec,
        &service_envelope,
        Some(&plan.execution_binding),
    )?;
    let envelope_instance_id = plan
        .envelope
        .envelope_instance_id
        .as_deref()
        .ok_or(ApiError::MissingEnvelope)?;
    let envelope_digest = plan
        .envelope
        .envelope_digest
        .as_deref()
        .ok_or(ApiError::MissingEnvelope)?;
    let reservation = application
        .ledger
        .reserve_task(TaskReservationRequest {
            task_uid,
            operation_id,
            idempotency_key,
            submitter_service: &identity.service,
            acting_user: identity.acting_user.as_ref().map(|email| email.0.as_str()),
            acting_user_id: identity
                .acting_user
                .as_ref()
                .map(|_| identity.canonical_user_id.as_str()),
            owner: &identity.owner.0,
            owner_user_id: identity.canonical_user_id.as_str(),
            workflow: &workflow_reference,
            workflow_name: Some(&plan.workflow.name),
            workflow_version: Some(plan.workflow.version),
            workflow_digest: Some(&plan.workflow.content_digest),
            user_envelope_instance_id: Some(envelope_instance_id),
            user_envelope_revision: Some(user_envelope.revision),
            user_envelope_digest: Some(envelope_digest),
            coding_agent_runtime: &plan.workflow.agent,
            runtime_uid: None,
            runtime_namespace: VERSIONED_WORKFLOW_NAMESPACE,
            runtime_name: &runtime_name,
            runtime_ownership: RuntimeOwnership::Provisioned,
            runtime_spec: &plan.spec,
            agent_command: &plan.command,
            execution_binding: Some(&plan.execution_binding),
            direct_task_evidence: None,
            envelope_revision: service_envelope.revision,
            service_envelope: &service_envelope,
            service_envelope_digest: &orchestration.service_envelope_digest,
            candidate_digest: &orchestration.candidate_digest,
            admission_decision: &decision,
            inert_manifest_digest: &orchestration.inert_manifest_digest,
            active_manifest_digest: &orchestration.active_manifest_digest,
        })
        .await;
    let deltas = admission_deltas(&decision);
    application
        .finish_task_reservation(
            &identity,
            Some(&reference),
            request,
            idempotency_key,
            reservation,
            deltas,
        )
        .await
}

fn validate_task_retry(
    identity: &TaskIdentity,
    reference: Option<&WorkflowReference>,
    request: &TaskSubmissionRequest,
    expected_runtime_uid: Option<&str>,
    record: &TaskRecord,
) -> Result<(), ApiError> {
    let acting_user = identity.acting_user.as_ref().map(|email| email.0.as_str());
    let acting_user_id = identity
        .acting_user
        .as_ref()
        .map(|_| identity.canonical_user_id.as_str());
    if record.identity_binding_state != "bound"
        || record.submitter_service != identity.service
        || record.acting_user.as_deref() != acting_user
        || record.acting_user_id.as_deref() != acting_user_id
        || record.owner != identity.owner.0
        || record.owner_user_id.as_deref() != Some(identity.canonical_user_id.as_str())
    {
        return Err(ApiError::Store(StoreError::TaskIdempotencyConflict));
    }
    match reference {
        Some(reference) => {
            let workflow_reference = format!("{}@{}", reference.name, reference.version);
            if request.agent_runtime_uid.is_some()
                || record.workflow != workflow_reference
                || record.workflow_name.as_deref() != Some(reference.name.as_str())
                || record.workflow_version != Some(reference.version)
                || record.runtime_ownership != RuntimeOwnership::Provisioned
            {
                return Err(ApiError::Store(StoreError::TaskIdempotencyConflict));
            }
        }
        None => {
            let requested_runtime = request.coding_agent_runtime.as_deref().ok_or_else(|| {
                ApiError::Admission("legacy workflows require codingAgentRuntime".to_owned())
            })?;
            let runtime_binding_matches = match request.agent_runtime_uid.as_deref() {
                Some(runtime_uid) => {
                    record.runtime_ownership == RuntimeOwnership::Adopted
                        && expected_runtime_uid == Some(runtime_uid)
                        && record
                            .runtime_uid
                            .as_deref()
                            .is_none_or(|observed_uid| observed_uid == runtime_uid)
                }
                None => {
                    expected_runtime_uid.is_none()
                        && record.runtime_ownership == RuntimeOwnership::Provisioned
                }
            };
            if record.workflow != request.workflow
                || record.workflow_name.is_some()
                || record.workflow_version.is_some()
                || record.coding_agent_runtime != requested_runtime
                || !runtime_binding_matches
            {
                return Err(ApiError::Store(StoreError::TaskIdempotencyConflict));
            }
        }
    }
    Ok(())
}

async fn resolve_task_identity<I: TaskIdentityResolver>(
    identities: &I,
    headers: &HeaderMap,
) -> Result<TaskIdentity, ApiError> {
    let assertion = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| !value.is_empty())
        .ok_or(ApiError::TaskAuthentication)?;
    identities
        .resolve(assertion)
        .await
        .map_err(|error| match error {
            TaskAuthenticationError::InvalidCredentials => ApiError::TaskAuthentication,
            TaskAuthenticationError::Unavailable => ApiError::TaskAuthenticationUnavailable,
        })
}

pub(crate) struct TaskOrchestrationReservation {
    pub(crate) candidate_digest: String,
    pub(crate) service_envelope_digest: String,
    pub(crate) inert_manifest_digest: String,
    pub(crate) active_manifest_digest: String,
}

pub(crate) fn task_orchestration_reservation(
    task_uid: Uuid,
    operation_id: Uuid,
    runtime_namespace: &str,
    runtime_name: &str,
    spec: &AgentRuntimeSpec,
    envelope: &Envelope,
    execution_binding: Option<&TaskExecutionBinding>,
) -> Result<TaskOrchestrationReservation, ApiError> {
    let mut inert = spec.clone();
    inert.llms.clear();
    inert.tools.clear();
    inert.budget.monthly_limit = "0".to_owned();
    inert.budget.single_run_limit = Some("0".to_owned());
    inert.budget.currency = envelope.spec.budget.currency.clone();
    let candidate_digest = format!("sha256:{}", spec_digest(spec)?);
    let digest = |mode, desired_spec| {
        serialized_digest(&serde_json::json!({
            "schemaVersion": "steward-task-runtime-manifest/v1",
            "taskUid": task_uid,
            "operationId": operation_id,
            "runtimeNamespace": runtime_namespace,
            "runtimeName": runtime_name,
            "mode": mode,
            "spec": desired_spec,
            "executionBinding": execution_binding,
        }))
    };
    Ok(TaskOrchestrationReservation {
        candidate_digest: candidate_digest.clone(),
        service_envelope_digest: serialized_digest(envelope)?,
        inert_manifest_digest: digest("inert", &inert)?,
        active_manifest_digest: digest("active", spec)?,
    })
}

fn serialized_digest<T: Serialize>(value: &T) -> Result<String, ApiError> {
    let bytes = serde_json::to_vec(value).map_err(|_| {
        ApiError::TaskRuntimeContractUnavailable(
            "durable Task orchestration input cannot be serialized".to_owned(),
        )
    })?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

async fn task_response<L: TaskSubmissionLedger>(
    ledger: &L,
    record: TaskRecord,
    deltas: Vec<AdmissionDelta>,
) -> Result<(StatusCode, TaskStatusResponse), ApiError> {
    let status = if record.runtime_uid.is_none() || record.phase == TaskPhase::Parked {
        StatusCode::ACCEPTED
    } else if record.finalized && record.workflow_name.is_some() {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((status, status_response(ledger, record, deltas).await?))
}

fn admission_deltas(decision: &AdmissionDecision) -> Vec<AdmissionDelta> {
    match decision {
        AdmissionDecision::Admit => Vec::new(),
        AdmissionDecision::Reject { deltas } => deltas.clone(),
    }
}

async fn status_response<L: TaskSubmissionLedger>(
    ledger: &L,
    record: TaskRecord,
    deltas: Vec<AdmissionDelta>,
) -> Result<TaskStatusResponse, ApiError> {
    // The frozen legacy caller requires an adopted target UID in every response.
    // This is a read-only projection of server-validated immutable intent, not
    // evidence of controller binding. M1 and durable runtime_uid stay unchanged.
    let runtime_uid = if record.runtime_uid.is_none()
        && record.runtime_ownership == RuntimeOwnership::Adopted
        && record.workflow_name.is_none()
    {
        let operation = ledger
            .task_runtime_operation(record.task_uid)
            .await
            .map_err(ApiError::Store)?
            .filter(|operation| {
                operation.task_uid == record.task_uid
                    && Some(operation.operation_id) == record.orchestration_operation_id
                    && operation.runtime_ownership == TaskRuntimeOwnership::Adopted
            })
            .ok_or(ApiError::Store(StoreError::InvalidTaskTransition))?;
        Some(
            operation
                .expected_runtime_uid
                .filter(|uid| !uid.is_empty())
                .ok_or(ApiError::Store(StoreError::InvalidTaskTransition))?,
        )
    } else {
        record.runtime_uid
    };
    if let Some(evidence) = record.direct_task_evidence {
        let task_uid = steward_types::direct_package::Uuid::parse(record.task_uid.to_string())
            .map_err(ApiError::TaskRuntimeContractUnavailable)?;
        let runtime_uid = runtime_uid
            .map(BoundedText::parse)
            .transpose()
            .map_err(ApiError::TaskRuntimeContractUnavailable)?;
        let failure_reason = record
            .failure_reason
            .map(BoundedText::parse)
            .transpose()
            .map_err(ApiError::TaskRuntimeContractUnavailable)?;
        let deltas = serde_json::from_value::<Vec<DirectAdmissionDelta>>(
            serde_json::to_value(deltas).map_err(|_| {
                ApiError::TaskRuntimeContractUnavailable(
                    "direct Task admission deltas cannot be projected".to_owned(),
                )
            })?,
        )
        .map_err(|_| {
            ApiError::TaskRuntimeContractUnavailable(
                "direct Task admission deltas cannot be projected".to_owned(),
            )
        })?;
        let response = DirectTaskStatusResponse {
            contract_version: steward_types::direct_package::DIRECT_TASK_CONTRACT_VERSION
                .to_owned(),
            task_uid,
            runtime_uid,
            phase: match record.phase {
                TaskPhase::Submitted => DirectTaskPhase::Submitted,
                TaskPhase::Parked => DirectTaskPhase::Parked,
                TaskPhase::Queued => DirectTaskPhase::Queued,
                TaskPhase::Running => DirectTaskPhase::Running,
                TaskPhase::Succeeded => DirectTaskPhase::Succeeded,
                TaskPhase::Failed => DirectTaskPhase::Failed,
                TaskPhase::Cancelled => DirectTaskPhase::Cancelled,
            },
            runtime_ownership: match record.runtime_ownership {
                RuntimeOwnership::Provisioned => DirectRuntimeOwnership::Provisioned,
                RuntimeOwnership::Adopted => DirectRuntimeOwnership::Adopted,
            },
            finalized: record.finalized,
            failure_reason,
            deltas,
            diagnostics: evidence.diagnostics,
            evidence,
        };
        response
            .validate()
            .map_err(ApiError::TaskRuntimeContractUnavailable)?;
        Ok(TaskStatusResponse::Direct(Box::new(response)))
    } else {
        Ok(TaskStatusResponse::Existing(LegacyTaskStatusResponse {
            task_uid: record.task_uid,
            runtime_uid,
            phase: record.phase,
            runtime_ownership: record.runtime_ownership,
            finalized: record.finalized,
            failure_reason: record.failure_reason,
            deltas,
        }))
    }
}

fn stable_task_runtime_name(operation_id: Uuid) -> String {
    format!("task-{}", operation_id.simple())
}

#[cfg(test)]
mod workflow_request_tests {
    use std::sync::Arc;

    use super::{
        StaticTaskWorkflowCatalog, TaskApiConfig, TaskWorkflowCatalog, resolve_versioned_task_plan,
        stable_task_runtime_name, task_orchestration_reservation, versioned_workflow_reference,
    };
    use crate::{ApiError, TaskIdentity};
    use steward_admission::{Envelope, EnvelopeSpec};
    use steward_ports::{
        PortError, TaskExecutionAdapter, TaskExecutionPlan, TaskExecutionPlanRequest,
    };
    use steward_store::{EnvelopeRequestRecord, EnvelopeRequestStatus, WorkflowRevisionRecord};
    use steward_types::{
        Budget, CanonicalUserId, Duration, Email, ModelRef, RunnerRequirements, ToolGrant,
    };
    use uuid::Uuid;

    struct ExampleExecutionAdapter;

    impl TaskExecutionAdapter for ExampleExecutionAdapter {
        fn contract(&self) -> &'static str {
            "example-v1"
        }

        fn render(
            &self,
            request: TaskExecutionPlanRequest<'_>,
        ) -> Result<TaskExecutionPlan, PortError> {
            let mut command = vec![
                "example-runner".to_owned(),
                "--prompt".to_owned(),
                request.workflow_prompt.to_owned(),
                "--model".to_owned(),
                format!("{}/{}", request.model.provider, request.model.model),
                "--executable".to_owned(),
                request.binding.executable.clone(),
                "--expected-version".to_owned(),
                request.binding.version_probe.expected_stdout.clone(),
            ];
            command.extend(request.binding.version_probe.arguments.iter().cloned());
            if let Some(endpoint) = request.tool_transport_endpoint {
                command.push("--tool-transport".to_owned());
                command.push(endpoint.to_owned());
            }
            Ok(TaskExecutionPlan { command })
        }
    }

    fn execution_catalog(agent_ref: &str, image_byte: char) -> Result<String, String> {
        let image = format!(
            "registry.example.test/steward/agent@sha256:{}",
            image_byte.to_string().repeat(64)
        );
        let executable = "/opt/steward/agent";
        let expected_version = format!(
            "example-agent {}",
            agent_ref
                .rsplit_once('@')
                .map_or("", |(_, version)| version)
        );
        serde_json::to_string(&serde_json::json!({
            "apiVersion": "steward.execution-bindings/v1",
            "bindings": [{
                "agentRef": agent_ref,
                "displayName": format!("Agent {agent_ref}"),
                "adapter": "example-v1",
                "image": image,
                "executable": executable,
                "versionProbe": {
                    "arguments": ["--version"],
                    "expectedStdout": expected_version
                },
                "providerProfiles": {
                    "tools": {
                        "id": "example-tools-profile-v7",
                        "digest": format!("sha256:{}", "c".repeat(64))
                    },
                    "inference": {
                        "id": "example-inference-profile-v7",
                        "digest": format!("sha256:{}", "d".repeat(64))
                    }
                }
            }]
        }))
        .map_err(|error| error.to_string())
    }

    fn task_config(endpoint: Option<&str>) -> Result<TaskApiConfig, String> {
        let workflow = workflow();
        TaskApiConfig::new(endpoint.map(str::to_owned))?
            .with_execution_adapter(Arc::new(ExampleExecutionAdapter))?
            .with_execution_bindings_json(Some(&execution_catalog(&workflow.agent, 'a')?))?
            .with_execution_bindings_active(true)
    }

    #[test]
    fn active_catalog_rejects_an_adapter_not_registered_by_the_apiserver() -> Result<(), String> {
        let mut catalog = serde_json::from_str::<serde_json::Value>(&execution_catalog(
            "example-agent@1.0.0",
            'a',
        )?)
        .map_err(|error| error.to_string())?;
        catalog["bindings"][0]["adapter"] = serde_json::json!("unavailable-v1");
        let result = TaskApiConfig::default()
            .with_execution_bindings_json(Some(&catalog.to_string()))?
            .with_execution_bindings_active(true);
        assert!(
            result.is_err_and(|reason| reason.contains("uses unavailable adapter")),
            "an active binding must not name an adapter absent from the running apiserver"
        );
        Ok(())
    }

    #[test]
    fn source_repository_binding_catalog_rejects_invalid_or_duplicate_entries() {
        let unknown_field = serde_json::json!({
            "contractVersion": "steward.source-repository-bindings/v1",
            "bindings": [{
                "caller": {"ownerId": "7890", "repositoryId": "123456"},
                "source": {"ownerId": "7890", "repositoryId": "654321", "name": "ignored"}
            }]
        })
        .to_string();
        assert!(
            TaskApiConfig::default()
                .with_source_repository_bindings_json(Some(&unknown_field))
                .is_err(),
            "unknown source identity fields must fail startup parsing"
        );

        let binding = serde_json::json!({
            "caller": {"ownerId": "7890", "repositoryId": "123456"},
            "source": {"ownerId": "7890", "repositoryId": "654321"}
        });
        let duplicate = serde_json::json!({
            "contractVersion": "steward.source-repository-bindings/v1",
            "bindings": [binding.clone(), binding]
        })
        .to_string();
        assert!(
            TaskApiConfig::default()
                .with_source_repository_bindings_json(Some(&duplicate))
                .is_err(),
            "duplicate repository authority must fail startup parsing"
        );
    }

    #[test]
    fn versioned_workflow_rejects_caller_selected_coding_runtime() {
        assert!(
            versioned_workflow_reference("repository-review@1", Some("base")).is_err(),
            "versioned Workflow requests must not let the caller select the coding runtime"
        );
    }

    #[test]
    fn task_orchestration_candidate_digest_is_a_valid_sha256_reference() -> Result<(), String> {
        let identity = identity("usr_0123456789abcdef0123456789abcdef")?;
        let plan = resolve_versioned_task_plan(
            &identity,
            workflow(),
            vec![provisioned_envelope(identity.canonical_user_id.as_str())?],
            &task_config(Some("https://mcp-gw.example.test/mcp"))?,
        )
        .map_err(|error| format!("resolve Task plan: {error:?}"))?;
        let envelope = plan
            .envelope
            .approved_envelope
            .as_ref()
            .ok_or_else(|| "resolved Task plan omitted its approved Envelope".to_owned())?;
        let operation_id = Uuid::new_v4();
        let runtime_name = stable_task_runtime_name(operation_id);
        let reservation = task_orchestration_reservation(
            Uuid::new_v4(),
            operation_id,
            "steward-workflows",
            &runtime_name,
            &plan.spec,
            envelope,
            Some(&plan.execution_binding),
        )
        .map_err(|error| format!("derive Task orchestration intent: {error:?}"))?;

        assert!(
            reservation.candidate_digest.starts_with("sha256:")
                && reservation.candidate_digest.len() == 71,
            "the persisted candidate digest must use the sha256:<64 lowercase hex> contract"
        );
        Ok(())
    }

    #[test]
    fn staged_execution_catalog_is_validated_but_not_advertised_or_selected() -> Result<(), String>
    {
        let workflow = workflow();
        let config = TaskApiConfig::new(Some("https://mcp-gw.example.test/mcp".to_owned()))?
            .with_execution_adapter(Arc::new(ExampleExecutionAdapter))?
            .with_execution_bindings_json(Some(&execution_catalog(&workflow.agent, 'a')?))?;
        assert!(config.execution_binding_refs().is_empty());
        assert!(config.execution_binding_advertisements().is_empty());
        assert!(
            resolve_versioned_task_plan(
                &identity("usr_0123456789abcdef0123456789abcdef")?,
                workflow,
                vec![provisioned_envelope(
                    "usr_0123456789abcdef0123456789abcdef",
                )?],
                &config,
            )
            .is_err(),
            "the first rollout stage must not let a new apiserver feed bindings to an old controller"
        );
        Ok(())
    }

    #[test]
    fn active_execution_catalog_rejects_an_unregistered_adapter() -> Result<(), String> {
        let workflow = workflow();
        let result = TaskApiConfig::new(None)?
            .with_execution_bindings_json(Some(&execution_catalog(&workflow.agent, 'a')?))?
            .with_execution_bindings_active(true);
        assert!(
            result.is_err(),
            "an active catalog must fail startup when no implementation owns its adapter contract"
        );
        Ok(())
    }

    #[test]
    fn malformed_versioned_workflow_does_not_fall_back_to_legacy_catalog() {
        for workflow in [
            "repository-review@latest",
            "repository-review@0",
            "repository-review@",
            "repository-review@1@2",
        ] {
            assert!(
                versioned_workflow_reference(workflow, None).is_err(),
                "malformed versioned reference {workflow:?} must not reach the legacy path"
            );
        }
    }

    #[test]
    fn empty_legacy_catalog_allows_versioned_only_deployments() -> Result<(), String> {
        let catalog = StaticTaskWorkflowCatalog::from_json("[]").map_err(|error| {
            format!("a versioned-Workflow deployment may omit legacy workflows: {error}")
        })?;
        assert!(catalog.workflow("legacy-smoke").is_none());
        Ok(())
    }

    #[test]
    fn task_tool_transport_endpoint_rejects_ambiguous_or_credentialed_urls() -> Result<(), String> {
        for endpoint in [
            " https://mcp-gw.example.test/mcp",
            "https://mcp-gw.example.test/m\tcp",
            "https://mcp-gw.example.test/m\ncp",
            "ftp://mcp-gw.example.test/mcp",
            "https://alice@mcp-gw.example.test/mcp",
            "https://mcp-gw.example.test/mcp?target=other",
            "https://mcp-gw.example.test/mcp#fragment",
            "https://mcp-gw.example.test:0/mcp",
        ] {
            assert!(
                TaskApiConfig::new(Some(endpoint.to_owned())).is_err(),
                "invalid task tool transport endpoint {endpoint:?} must fail configuration"
            );
        }

        let normalized = TaskApiConfig::new(Some("HTTPS://MCP-GW.EXAMPLE.TEST/mcp".to_owned()))?;
        assert_eq!(
            normalized.tool_transport_endpoint.as_deref(),
            Some("https://mcp-gw.example.test/mcp"),
            "the rendered contract must use the URL parser's normalized representation"
        );
        Ok(())
    }

    fn identity(user_id: &str) -> Result<TaskIdentity, String> {
        Ok(TaskIdentity {
            service: "steward-run".to_owned(),
            acting_user: Some(Email("alice@example.com".to_owned())),
            owner: Email("alice@example.com".to_owned()),
            canonical_user_id: CanonicalUserId::parse(user_id)?,
            source_provenance: None,
        })
    }

    fn workflow() -> WorkflowRevisionRecord {
        WorkflowRevisionRecord {
            name: "repository-review".to_owned(),
            version: 1,
            display_name: "Repository review".to_owned(),
            agent: "example-agent@1.0.0".to_owned(),
            prompt: "Review the repository state.".to_owned(),
            content_digest: "workflow-digest".to_owned(),
            published_by: "usr_abcdef0123456789abcdef0123456789".to_owned(),
            published_at: "2026-08-24T00:00:00.000000Z".to_owned(),
        }
    }

    fn provisioned_envelope(owner_user_id: &str) -> Result<EnvelopeRequestRecord, String> {
        let envelope = Envelope {
            revision: 7,
            spec: EnvelopeSpec {
                llms: vec![ModelRef {
                    provider: "openai".to_owned(),
                    model: "gpt-5.4".to_owned(),
                }],
                tools: vec![ToolGrant {
                    provider: "github".to_owned(),
                    resource: "repository".to_owned(),
                    action: "get_file_contents".to_owned(),
                }],
                budget: Budget {
                    monthly_limit: "10.00".to_owned(),
                    single_run_limit: Some("1.00".to_owned()),
                    currency: "USD".to_owned(),
                },
                ttl: Duration("15m".to_owned()),
                runner: RunnerRequirements::default(),
            },
        };
        Ok(EnvelopeRequestRecord {
            id: Uuid::new_v4(),
            owner_user_id: CanonicalUserId::parse(owner_user_id)?,
            template_id: "developer".to_owned(),
            template_revision: 2,
            requested_envelope: envelope.clone(),
            approved_envelope: Some(envelope),
            status: EnvelopeRequestStatus::Provisioned,
            approval_id: None,
            envelope_instance_id: Some("env_instance_01".to_owned()),
            envelope_digest: Some("envelope-digest".to_owned()),
            reason: None,
            status_actor: owner_user_id.to_owned(),
            status_template_revision: 2,
            created_at: "2026-08-24T00:00:00.000000Z".to_owned(),
            status_at: "2026-08-24T00:00:01.000000Z".to_owned(),
        })
    }

    #[test]
    fn versioned_workflow_cannot_use_another_users_envelope() -> Result<(), String> {
        let result = resolve_versioned_task_plan(
            &identity("usr_0123456789abcdef0123456789abcdef")?,
            workflow(),
            vec![provisioned_envelope(
                "usr_abcdef0123456789abcdef0123456789",
            )?],
            &TaskApiConfig::default(),
        );
        assert!(
            matches!(result, Err(ApiError::PrincipalMismatch)),
            "a versioned Task must reject an Envelope owned by another canonical user"
        );
        Ok(())
    }

    #[test]
    fn zero_or_ambiguous_provisioned_user_envelopes_fail_closed() -> Result<(), String> {
        let task_identity = identity("usr_0123456789abcdef0123456789abcdef")?;
        assert!(matches!(
            resolve_versioned_task_plan(
                &task_identity,
                workflow(),
                Vec::new(),
                &TaskApiConfig::default(),
            ),
            Err(ApiError::MissingEnvelope)
        ));
        assert!(matches!(
            resolve_versioned_task_plan(
                &task_identity,
                workflow(),
                vec![
                    provisioned_envelope("usr_0123456789abcdef0123456789abcdef")?,
                    provisioned_envelope("usr_0123456789abcdef0123456789abcdef")?,
                ],
                &TaskApiConfig::default(),
            ),
            Err(ApiError::Conflict(_))
        ));
        Ok(())
    }

    #[test]
    fn versioned_task_combines_workflow_agent_and_prompt_with_user_envelope_authority()
    -> Result<(), String> {
        let plan = resolve_versioned_task_plan(
            &identity("usr_0123456789abcdef0123456789abcdef")?,
            workflow(),
            vec![provisioned_envelope(
                "usr_0123456789abcdef0123456789abcdef",
            )?],
            &task_config(Some("https://mcp-gw.example.test/mcp"))?,
        )
        .map_err(|error| format!("one exact owned provisioned Envelope was rejected: {error:?}"))?;
        assert_eq!(plan.workflow.name, "repository-review");
        assert_eq!(plan.workflow.version, 1);
        assert_eq!(plan.spec.agent_type.name, "example-agent@1.0.0");
        assert_eq!(plan.spec.llms[0].model, "gpt-5.4");
        assert_eq!(plan.spec.tools[0].provider, "github");
        assert_eq!(plan.spec.budget.monthly_limit, "10.00");
        assert_eq!(plan.spec.budget.single_run_limit.as_deref(), Some("1.00"));
        assert_eq!(plan.spec.ttl.0, "15m");
        assert_eq!(plan.envelope.template_revision, 2);
        assert_eq!(
            plan.envelope.envelope_instance_id.as_deref(),
            Some("env_instance_01")
        );
        assert_eq!(
            plan.command.get(2).map(String::as_str),
            Some("Review the repository state."),
            "the immutable Workflow prompt must be a separate argument to the server-owned command"
        );
        assert!(
            plan.command
                .iter()
                .any(|argument| argument.contains("example-agent 1.0.0")),
            "the server-owned command must fail closed unless the sandbox exposes the exact configured version"
        );
        assert_eq!(
            plan.command.get(4).map(String::as_str),
            Some("openai/gpt-5.4"),
            "the approved provider and model must be passed as an opaque shell argument"
        );
        assert_eq!(
            plan.command.last().map(String::as_str),
            Some("https://mcp-gw.example.test/mcp"),
            "the registered adapter must receive the validated server-owned tool endpoint"
        );
        assert!(
            plan.command
                .iter()
                .all(|argument| !argument.contains("example-org")),
            "repository mechanics do not belong to the Workflow execution command"
        );
        Ok(())
    }

    #[test]
    fn versioned_task_persists_the_exact_deployment_binding_before_reservation()
    -> Result<(), String> {
        let mut selected_workflow = workflow();
        selected_workflow.agent = "example-agent@2.0.0".to_owned();
        let catalog = execution_catalog(&selected_workflow.agent, 'b')?;
        let config = TaskApiConfig::new(Some("https://mcp-gw.example.test/mcp".to_owned()))?
            .with_execution_adapter(Arc::new(ExampleExecutionAdapter))?
            .with_execution_bindings_json(Some(&catalog))?
            .with_execution_bindings_active(true)?;
        let plan = resolve_versioned_task_plan(
            &identity("usr_0123456789abcdef0123456789abcdef")?,
            selected_workflow,
            vec![provisioned_envelope(
                "usr_0123456789abcdef0123456789abcdef",
            )?],
            &config,
        )
        .map_err(|error| format!("approved exact binding was rejected: {error:?}"))?;

        let binding = plan
            .execution_binding
            .disposable()
            .ok_or_else(|| "resolved Task did not retain its deployment binding".to_owned())?;
        assert_eq!(binding.agent_ref, "example-agent@2.0.0");
        assert_eq!(plan.command.get(6), Some(&binding.executable));
        assert_eq!(
            plan.command.get(8),
            Some(&binding.version_probe.expected_stdout)
        );
        assert_eq!(plan.command.get(9).map(String::as_str), Some("--version"));
        assert_eq!(config.execution_binding_refs(), ["example-agent@2.0.0"]);

        let mut unavailable = workflow();
        unavailable.agent = "example-agent@3.0.0".to_owned();
        assert!(matches!(
            resolve_versioned_task_plan(
                &identity("usr_0123456789abcdef0123456789abcdef")?,
                unavailable,
                vec![provisioned_envelope(
                    "usr_0123456789abcdef0123456789abcdef",
                )?],
                &config,
            ),
            Err(ApiError::TaskRuntimeContractUnavailable(_))
        ));
        Ok(())
    }

    #[test]
    fn tool_less_versioned_task_plan_contains_no_tool_transport() -> Result<(), String> {
        let mut envelope = provisioned_envelope("usr_0123456789abcdef0123456789abcdef")?;
        envelope.requested_envelope.spec.tools.clear();
        envelope
            .approved_envelope
            .as_mut()
            .ok_or_else(|| "fixture must contain an approved Envelope".to_owned())?
            .spec
            .tools
            .clear();
        let plan = resolve_versioned_task_plan(
            &identity("usr_0123456789abcdef0123456789abcdef")?,
            workflow(),
            vec![envelope],
            &task_config(None)?,
        )
        .map_err(|error| format!("tool-less versioned Task was rejected: {error:?}"))?;
        assert!(
            !plan
                .command
                .iter()
                .any(|argument| argument == "--tool-transport"),
            "unused Envelope capacity must not add a tool transport"
        );
        Ok(())
    }

    #[test]
    fn tool_bearing_versioned_task_fails_without_tool_transport_contract() -> Result<(), String> {
        let result = resolve_versioned_task_plan(
            &identity("usr_0123456789abcdef0123456789abcdef")?,
            workflow(),
            vec![provisioned_envelope(
                "usr_0123456789abcdef0123456789abcdef",
            )?],
            &task_config(None)?,
        );
        assert!(
            matches!(result, Err(ApiError::TaskRuntimeContractUnavailable(_))),
            "tool-bearing plans must fail before execution when the tool transport is unavailable"
        );
        Ok(())
    }

    #[test]
    fn empty_execution_catalog_fails_before_versioned_task_reservation() -> Result<(), String> {
        let result = resolve_versioned_task_plan(
            &identity("usr_0123456789abcdef0123456789abcdef")?,
            workflow(),
            vec![provisioned_envelope(
                "usr_0123456789abcdef0123456789abcdef",
            )?],
            &TaskApiConfig::new(Some("https://mcp-gw.example.test/mcp".to_owned()))?,
        );
        assert!(matches!(
            result,
            Err(ApiError::TaskRuntimeContractUnavailable(_))
        ));
        Ok(())
    }

    #[test]
    fn missing_required_provider_profiles_fail_before_versioned_task_reservation()
    -> Result<(), String> {
        for profile in ["tools", "inference"] {
            let mut catalog = serde_json::from_str::<serde_json::Value>(&execution_catalog(
                &workflow().agent,
                'a',
            )?)
            .map_err(|error| error.to_string())?;
            catalog
                .pointer_mut("/bindings/0/providerProfiles")
                .and_then(serde_json::Value::as_object_mut)
                .ok_or_else(|| "fixture provider profiles are missing".to_owned())?
                .remove(profile);
            let config = TaskApiConfig::new(Some("https://mcp-gw.example.test/mcp".to_owned()))?
                .with_execution_adapter(Arc::new(ExampleExecutionAdapter))?
                .with_execution_bindings_json(Some(&catalog.to_string()))?
                .with_execution_bindings_active(true)?;
            let result = resolve_versioned_task_plan(
                &identity("usr_0123456789abcdef0123456789abcdef")?,
                workflow(),
                vec![provisioned_envelope(
                    "usr_0123456789abcdef0123456789abcdef",
                )?],
                &config,
            );
            assert!(
                matches!(result, Err(ApiError::TaskRuntimeContractUnavailable(_))),
                "missing {profile} profile must fail before Task reservation"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod identity_task_authentication_tests {
    use super::{
        IdentityTaskClaims, TaskAuthenticationError, task_identity_from_identity_claims,
        validate_identity_task_jwks, verify_identity_task_token,
    };
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use jsonwebtoken::jwk::JwkSet;
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
    use p256::SecretKey;
    use p256::elliptic_curve::sec1::ToEncodedPoint;
    use p256::pkcs8::EncodePrivateKey;
    use rand_core::OsRng;
    use serde::Serialize;

    const ISSUER: &str = "https://identity.localhost:18444";
    const AUDIENCE: &str = "steward-task-api";
    const KID: &str = "identity-task-current";

    #[derive(Serialize)]
    struct Claims<'a> {
        iss: &'a str,
        sub: &'a str,
        aud: Vec<&'a str>,
        exp: u64,
        iat: u64,
        nbf: u64,
        jti: &'a str,
        email: &'a str,
        email_verified: bool,
        groups: Vec<&'a str>,
        identity_contract: &'a str,
    }

    fn key_material() -> Result<(EncodingKey, JwkSet), String> {
        let private = SecretKey::random(&mut OsRng);
        let der = private
            .to_pkcs8_der()
            .map_err(|error| format!("encode P-256 test key: {error}"))?;
        let point = private.public_key().to_encoded_point(false);
        let x = point.x().ok_or("P-256 public key missing x")?;
        let y = point.y().ok_or("P-256 public key missing y")?;
        let jwks = serde_json::json!({
            "keys": [{
                "kty": "EC", "use": "sig", "alg": "ES256", "kid": KID,
                "crv": "P-256", "x": URL_SAFE_NO_PAD.encode(x), "y": URL_SAFE_NO_PAD.encode(y)
            }]
        });
        Ok((
            EncodingKey::from_ec_der(der.as_bytes()),
            serde_json::from_value(jwks).map_err(|error| format!("parse test JWKS: {error}"))?,
        ))
    }

    fn token(
        key: &EncodingKey,
        issuer: &str,
        audience: &str,
        contract: &str,
    ) -> Result<String, String> {
        let now = jsonwebtoken::get_current_timestamp();
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some(KID.to_owned());
        encode(
            &header,
            &Claims {
                iss: issuer,
                sub: "github-actions:actor:16106037",
                aud: vec![audience],
                exp: now + 60,
                iat: now,
                nbf: now.saturating_sub(1),
                jti: "identity-task-test-jti",
                email: "leo@apelogic.ai",
                email_verified: true,
                groups: vec![
                    "agents.apelogic.ai/acting-user:leo@apelogic.ai",
                    "agents.apelogic.ai/canonical-user:usr_528fc0fed6cf400abb93a3f327d9a809",
                    "agents.apelogic.ai/service-principal:steward-run",
                ],
                identity_contract: contract,
            },
            key,
        )
        .map_err(|error| format!("sign Identity task token: {error}"))
    }

    #[test]
    fn identity_task_token_verifies_exact_claims_and_maps_existing_group_contract()
    -> Result<(), String> {
        let (key, jwks) = key_material()?;
        validate_identity_task_jwks(&jwks)
            .map_err(|error| format!("valid JWKS rejected: {error:?}"))?;
        let assertion = token(&key, ISSUER, AUDIENCE, "steward-task-v2")?;
        let claims: IdentityTaskClaims =
            verify_identity_task_token(&assertion, &jwks, ISSUER, AUDIENCE)
                .map_err(|error| format!("valid Identity task credential rejected: {error:?}"))?;
        let identity = task_identity_from_identity_claims(claims)
            .map_err(|error| format!("valid ratified Identity groups rejected: {error:?}"))?;
        assert_eq!(identity.service, "steward-run");
        assert_eq!(identity.owner.0, "leo@apelogic.ai");
        assert_eq!(
            identity.canonical_user_id.as_str(),
            "usr_528fc0fed6cf400abb93a3f327d9a809"
        );
        Ok(())
    }

    #[test]
    fn identity_task_jwks_accepts_unselectable_workload_keys() -> Result<(), String> {
        let (key, jwks) = key_material()?;
        let mut mixed = serde_json::to_value(jwks)
            .map_err(|error| format!("serialize task JWKS fixture: {error}"))?;
        mixed["keys"]
            .as_array_mut()
            .ok_or("task JWKS fixture keys is not an array")?
            .push(serde_json::json!({
                "kty": "RSA", "use": "sig", "alg": "RS256", "kid": "identity-workload-current",
                "n": "not-used-for-task-authentication", "e": "AQAB"
            }));
        let mixed = serde_json::from_value(mixed)
            .map_err(|error| format!("parse mixed Identity JWKS fixture: {error}"))?;

        validate_identity_task_jwks(&mixed)
            .map_err(|error| format!("mixed Identity JWKS rejected: {error:?}"))?;
        let assertion = token(&key, ISSUER, AUDIENCE, "steward-task-v2")?;
        verify_identity_task_token(&assertion, &mixed, ISSUER, AUDIENCE)
            .map_err(|error| format!("ES256 task token rejected from mixed JWKS: {error:?}"))?;
        Ok(())
    }

    #[test]
    fn identity_task_token_rejects_wrong_issuer_audience_or_contract() -> Result<(), String> {
        let (key, jwks) = key_material()?;
        for (issuer, audience, contract) in [
            (
                "https://other.identity.invalid",
                AUDIENCE,
                "steward-task-v2",
            ),
            (ISSUER, "other-audience", "steward-task-v2"),
            (ISSUER, AUDIENCE, "steward-task-v1"),
        ] {
            let assertion = token(&key, issuer, audience, contract)?;
            assert!(
                matches!(
                    verify_identity_task_token(&assertion, &jwks, ISSUER, AUDIENCE),
                    Err(TaskAuthenticationError::InvalidCredentials)
                ),
                "unratified Identity task credential must fail closed"
            );
        }
        Ok(())
    }
}
