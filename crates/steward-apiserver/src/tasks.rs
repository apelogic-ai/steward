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
use axum::{Extension, Json, Router};
use futures::StreamExt as _;
use jsonwebtoken::jwk::{Jwk, JwkSet, KeyAlgorithm, PublicKeyUse};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
#[cfg(test)]
use k8s_openapi::api::authentication::v1::TokenReviewStatus;
use k8s_openapi::api::authentication::v1::{TokenReview, UserInfo};
use kube::Client;
use kube::api::{Api, PostParams};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use steward_admission::{
    AdmissionDecision, AdmissionDelta, Envelope, EnvelopeSpec, evaluate_with_grants,
};
use steward_ports::{
    GitFile, GitFileRequest, GitHostingPlane, GitPack, GitPackRequest, GitRepositoryDescription,
    GitRepositoryIdentity, GitRepositoryReference, GitRevisionRequest, GitTree, GitTreeRequest,
    MAX_TASK_INPUT_ARCHIVE_BYTES, TaskExecutionAdapter, TaskExecutionPlanRequest,
};
use steward_store::{
    EnvelopeRequestRecord, FederatedSubjectObservation, FederatedSubjectRecord, PgStore,
    StoreError, TaskOrchestrationMode, TaskOutputTranscript, TaskRecord, TaskReservationRequest,
    TaskRuntimeOperationRecord, TaskRuntimeOwnership, WorkflowRevisionRecord,
};
use steward_types::direct_package::{
    BoundedText, BrowserTaskEvidence, BrowserTaskSubmission, ClosureEntry, ClosureEntryKind,
    ContentDigest, DirectAdmissionDelta, DirectRequirements, DirectRuntimeOwnership,
    DirectTaskBindingEvidence, DirectTaskDefinition, DirectTaskPhase, DirectTaskStatusResponse,
    DirectTaskSubmission, EnvelopeDigest, EnvelopeEvidence, ExecutionLogMode, InstructionSkill,
    InvocationKind, InvocationManifest, PACKAGE_CLOSURE_CONTRACT_VERSION, PackageClosure,
    PackageCommit, PromptSourceKind, RelativePath, RepositoryUrl, ResolvedSource,
    ResolvedWorkspaceEntry, ResolvedWorkspaceSubmodule, SourceProvenance, StableProviderId,
    TASK_BINDING_EVIDENCE_SCHEMA, TaskOrigin, TriggerRepository, WorkspaceEntry, WorkspaceEvidence,
    WorkspaceGitHistory, WorkspaceGitRepository, WorkspaceName, WorkspacePath,
    WorkspaceSubmoduleMode, WorkspaceSubmoduleStatus, canonical_json_bytes,
};
use steward_types::task_input_archive::{frame_task_input_archive, split_task_input_archive};
use steward_types::task_output_archive::{
    TASK_OUTPUT_ARCHIVE_CONTRACT, TaskOutputTranscriptError,
    task_output_archive_with_execution_transcript,
};
use steward_types::{
    AgentRuntimeSpec, CanonicalAuthorityBinding, CanonicalPrincipal, CanonicalUserId, Email,
    InferenceMode, ModelRef, Principal, RuntimeOwnership, TaskExecutionBinding, TaskPhase,
    ToolGrant,
};
use uuid::Uuid;

use crate::WorkflowReference;
use crate::browser_auth::{
    BrowserAuthService, BrowserMutationProof, BrowserSessionContext, protect_browser_routes,
};
use crate::execution_bindings::ExecutionBindingCatalog;
use crate::task_auth::{FEDERATED_TASK_TOKEN_CONTRACT, LEGACY_TASK_TOKEN_CONTRACT};
use crate::{
    AdmissionLedger, ApiError, BoxFuture, DIRECT_PACKAGE_SOURCE_DISABLED_CODE,
    DIRECT_PACKAGE_SOURCE_DISABLED_REASON, KubernetesTokenReviewAudience,
    authenticated_token_review_user, spec_digest, token_review_request,
};

const SERVICE_GROUP_PREFIX: &str = "agents.apelogic.ai/service-principal:";
const ACTING_USER_GROUP_PREFIX: &str = "agents.apelogic.ai/acting-user:";
const TASK_OWNER_GROUP_PREFIX: &str = "agents.apelogic.ai/task-owner:";
const CANONICAL_USER_GROUP_PREFIX: &str = "agents.apelogic.ai/canonical-user:";
const VERSIONED_WORKFLOW_NAMESPACE: &str = "steward-workflows";
const IDENTITY_TASK_CONTRACT: &str = LEGACY_TASK_TOKEN_CONTRACT;
const FEDERATED_TASK_CONTRACT: &str = FEDERATED_TASK_TOKEN_CONTRACT;
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
type TaskSubmissionFailureReporter = Arc<dyn Fn(&str) + Send + Sync>;

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
    invocation_kind: InvocationKind,
    invocation: ResolvedSource,
    package: ResolvedSource,
    closure: PackageClosure,
    closure_digest: ContentDigest,
    diagnostics: steward_types::direct_package::DiagnosticsRequest,
    prompt_source: PromptSourceKind,
    envelope: EnvelopeRequestRecord,
    effective_requirements: DirectRequirements,
    spec: AgentRuntimeSpec,
    command: Vec<String>,
    execution_binding: TaskExecutionBinding,
    workspace: Option<WorkspaceEvidence>,
}

struct BrowserTaskPreAdmission {
    definition: DirectTaskDefinition,
    evidence: BrowserTaskEvidence,
    envelope: EnvelopeRequestRecord,
    spec: AgentRuntimeSpec,
    command: Vec<String>,
    execution_binding: TaskExecutionBinding,
}

struct WorkspaceResolutionContext<'a> {
    self_repository: Option<&'a GitRepositoryIdentity>,
    trigger_commit: Option<&'a steward_types::direct_package::ExactGitCommit>,
    caller: Option<&'a TriggerRepository>,
    browser: bool,
}

/// `(index, result)` pairs in completion order, at most one per requested repository.
pub(crate) type DescribedRepositories<'a> = std::pin::Pin<
    Box<
        dyn futures::Stream<
                Item = (
                    usize,
                    Result<GitRepositoryDescription, steward_ports::PortError>,
                ),
            > + Send
            + 'a,
    >,
>;

trait DirectGitResolver: Send + Sync {
    fn describe_repositories<'a>(
        &'a self,
        repositories: &'a [GitRepositoryReference],
    ) -> DescribedRepositories<'a>;

    fn resolve_repository<'a>(
        &'a self,
        repository: &'a steward_types::direct_package::RepositoryUrl,
    ) -> BoxFuture<'a, Result<GitRepositoryIdentity, steward_ports::PortError>>;

    fn read_file<'a>(
        &'a self,
        request: &'a GitFileRequest,
    ) -> BoxFuture<'a, Result<GitFile, steward_ports::PortError>>;

    fn resolve_revision<'a>(
        &'a self,
        request: &'a GitRevisionRequest,
    ) -> BoxFuture<
        'a,
        Result<steward_types::direct_package::ExactGitCommit, steward_ports::PortError>,
    >;

    fn read_pack<'a>(
        &'a self,
        request: &'a GitPackRequest,
    ) -> BoxFuture<'a, Result<GitPack, steward_ports::PortError>>;

    fn read_tree<'a>(
        &'a self,
        request: &'a GitTreeRequest,
    ) -> BoxFuture<'a, Result<GitTree, steward_ports::PortError>>;
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkspaceDeploymentPolicyDocument {
    enabled_types: BTreeSet<String>,
    max_total_size: String,
    scratch_volume_size: String,
    max_files: u64,
    max_history_depth: u32,
    max_submodule_depth: usize,
}

#[derive(Clone)]
struct WorkspaceDeploymentPolicy {
    enabled_types: BTreeSet<String>,
    max_total_bytes: u64,
    scratch_volume_bytes: u64,
    max_files: u64,
    max_history_depth: u32,
    max_submodule_depth: usize,
}

impl Default for WorkspaceDeploymentPolicy {
    fn default() -> Self {
        Self {
            enabled_types: BTreeSet::from(["git".to_owned(), "scratch".to_owned()]),
            max_total_bytes: 2 * 1024 * 1024 * 1024,
            scratch_volume_bytes: 2 * 1024 * 1024 * 1024,
            max_files: 100_000,
            max_history_depth: 1_000,
            max_submodule_depth: 4,
        }
    }
}

impl TryFrom<WorkspaceDeploymentPolicyDocument> for WorkspaceDeploymentPolicy {
    type Error = String;

    fn try_from(value: WorkspaceDeploymentPolicyDocument) -> Result<Self, Self::Error> {
        if value.enabled_types.is_empty()
            || value
                .enabled_types
                .iter()
                .any(|entry| !matches!(entry.as_str(), "git" | "scratch"))
            || value.max_files == 0
            || value.max_history_depth == 0
            || value.max_submodule_depth > 16
        {
            return Err("workspace deployment policy is invalid".to_owned());
        }
        let max_total_bytes = parse_workspace_size_bytes(&value.max_total_size)
            .ok_or_else(|| "workspace deployment policy is invalid".to_owned())?;
        let scratch_volume_bytes = parse_workspace_size_bytes(&value.scratch_volume_size)
            .filter(|size| *size <= max_total_bytes)
            .ok_or_else(|| "workspace deployment policy is invalid".to_owned())?;
        Ok(Self {
            enabled_types: value.enabled_types,
            max_total_bytes,
            scratch_volume_bytes,
            max_files: value.max_files,
            max_history_depth: value.max_history_depth,
            max_submodule_depth: value.max_submodule_depth,
        })
    }
}

impl<G> DirectGitResolver for G
where
    G: GitHostingPlane,
{
    fn describe_repositories<'a>(
        &'a self,
        repositories: &'a [GitRepositoryReference],
    ) -> DescribedRepositories<'a> {
        Box::pin(GitHostingPlane::describe_repositories(self, repositories))
    }

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

    fn resolve_revision<'a>(
        &'a self,
        request: &'a GitRevisionRequest,
    ) -> BoxFuture<
        'a,
        Result<steward_types::direct_package::ExactGitCommit, steward_ports::PortError>,
    > {
        Box::pin(GitHostingPlane::resolve_revision(self, request))
    }

    fn read_pack<'a>(
        &'a self,
        request: &'a GitPackRequest,
    ) -> BoxFuture<'a, Result<GitPack, steward_ports::PortError>> {
        Box::pin(GitHostingPlane::read_pack(self, request))
    }

    fn read_tree<'a>(
        &'a self,
        request: &'a GitTreeRequest,
    ) -> BoxFuture<'a, Result<GitTree, steward_ports::PortError>> {
        Box::pin(GitHostingPlane::read_tree(self, request))
    }
}

#[derive(Clone)]
pub struct TaskApiConfig {
    tool_transport_endpoint: Option<String>,
    execution_bindings: ExecutionBindingCatalog,
    execution_adapters: BTreeMap<String, Arc<dyn TaskExecutionAdapter>>,
    execution_bindings_active: bool,
    orchestration_mode: TaskOrchestrationMode,
    direct_git_resolver: Option<Arc<dyn DirectGitResolver>>,
    source_repository_bindings: BTreeSet<SourceRepositoryBindingKey>,
    failure_reporter: TaskSubmissionFailureReporter,
    inference_mode: InferenceMode,
    workspace_policy: WorkspaceDeploymentPolicy,
}

impl Default for TaskApiConfig {
    fn default() -> Self {
        Self {
            tool_transport_endpoint: None,
            execution_bindings: ExecutionBindingCatalog::default(),
            execution_adapters: BTreeMap::new(),
            execution_bindings_active: false,
            orchestration_mode: TaskOrchestrationMode::Staged,
            direct_git_resolver: None,
            source_repository_bindings: BTreeSet::new(),
            failure_reporter: Arc::new(|line| eprintln!("{line}")),
            inference_mode: InferenceMode::Stock,
            workspace_policy: WorkspaceDeploymentPolicy::default(),
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

    pub fn with_inference_mode(mut self, mode: InferenceMode) -> Self {
        self.inference_mode = mode;
        self
    }

    pub fn with_git_hosting_plane<G>(mut self, resolver: G) -> Self
    where
        G: GitHostingPlane,
    {
        self.direct_git_resolver = Some(Arc::new(resolver));
        self
    }

    #[cfg(test)]
    pub(crate) fn with_failure_reporter(mut self, reporter: TaskSubmissionFailureReporter) -> Self {
        self.failure_reporter = reporter;
        self
    }

    fn report_direct_package_source_disabled(&self) {
        (self.failure_reporter)(&format!(
            "task submission rejected code={DIRECT_PACKAGE_SOURCE_DISABLED_CODE} failureReason={DIRECT_PACKAGE_SOURCE_DISABLED_REASON}"
        ));
    }

    fn report_browser_task_persistence_failure(&self, error: &StoreError) {
        (self.failure_reporter)(&format!(
            "browser task submission failed code=task.persistence_failed reason={error:?}"
        ));
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

    pub fn with_workspace_policy_json(mut self, value: Option<&str>) -> Result<Self, String> {
        let Some(value) = value else {
            return Ok(self);
        };
        if value.len() > 16 * 1024 {
            return Err("workspace deployment policy is invalid".to_owned());
        }
        let document = serde_json::from_str::<WorkspaceDeploymentPolicyDocument>(value)
            .map_err(|_| "workspace deployment policy is invalid".to_owned())?;
        self.workspace_policy = document.try_into()?;
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

    fn browser_source_repository_is_authorized(&self, source: &GitRepositoryIdentity) -> bool {
        self.source_repository_bindings.iter().any(
            |(_, _, source_owner_id, source_repository_id)| {
                source_owner_id == source.repository_owner_id.as_str()
                    && source_repository_id == source.repository_id.as_str()
            },
        )
    }

    /// Distinct admitted source repositories in stable-ID order. Empty unless the source
    /// GitHub App is configured, because only that App can resolve them for display.
    pub(crate) fn admitted_source_repository_references(&self) -> Vec<GitRepositoryReference> {
        if self.direct_git_resolver.is_none() {
            return Vec::new();
        }
        self.source_repository_bindings
            .iter()
            .filter_map(|(_, _, source_owner_id, source_repository_id)| {
                Some(GitRepositoryReference {
                    repository_owner_id: StableProviderId::parse(source_owner_id.as_str()).ok()?,
                    repository_id: StableProviderId::parse(source_repository_id.as_str()).ok()?,
                })
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub(crate) fn describe_admitted_source_repositories<'a>(
        &'a self,
        repositories: &'a [GitRepositoryReference],
    ) -> Option<DescribedRepositories<'a>> {
        self.direct_git_resolver
            .as_ref()
            .map(|resolver| resolver.describe_repositories(repositories))
    }

    pub(crate) fn browser_source_repository_ids_are_authorized(
        &self,
        owner_id: &str,
        repository_id: &str,
    ) -> bool {
        self.source_repository_bindings.iter().any(
            |(_, _, source_owner_id, source_repository_id)| {
                source_owner_id == owner_id && source_repository_id == repository_id
            },
        )
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
    UnknownUser,
    Unassociated { issuer: String, subject: String },
    Disabled { issuer: String, subject: String },
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
                .map_err(map_canonical_identity_error)?;
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
    canonical_identities: Arc<dyn IdentityTaskStore>,
    federated_subjects_enabled: bool,
}

trait IdentityTaskStore: Send + Sync {
    fn resolve_canonical_principal<'a>(
        &'a self,
        user_id: &'a CanonicalUserId,
        current_verified_email: &'a Email,
    ) -> BoxFuture<'a, Result<CanonicalPrincipal, StoreError>>;

    fn seed_federated_subject_association<'a>(
        &'a self,
        observation: FederatedSubjectObservation<'a>,
        canonical_user_id: &'a CanonicalUserId,
        actor: &'a str,
    ) -> BoxFuture<'a, Result<FederatedSubjectRecord, StoreError>>;

    fn observe_federated_subject<'a>(
        &'a self,
        observation: FederatedSubjectObservation<'a>,
    ) -> BoxFuture<'a, Result<FederatedSubjectRecord, StoreError>>;

    fn resolve_federated_subject<'a>(
        &'a self,
        issuer: &'a str,
        subject: &'a str,
    ) -> BoxFuture<'a, Result<CanonicalPrincipal, StoreError>>;
}

impl IdentityTaskStore for PgStore {
    fn resolve_canonical_principal<'a>(
        &'a self,
        user_id: &'a CanonicalUserId,
        current_verified_email: &'a Email,
    ) -> BoxFuture<'a, Result<CanonicalPrincipal, StoreError>> {
        Box::pin(PgStore::resolve_canonical_principal(
            self,
            user_id,
            current_verified_email,
        ))
    }

    fn seed_federated_subject_association<'a>(
        &'a self,
        observation: FederatedSubjectObservation<'a>,
        canonical_user_id: &'a CanonicalUserId,
        actor: &'a str,
    ) -> BoxFuture<'a, Result<FederatedSubjectRecord, StoreError>> {
        Box::pin(PgStore::seed_federated_subject_association(
            self,
            observation,
            canonical_user_id,
            actor,
        ))
    }

    fn observe_federated_subject<'a>(
        &'a self,
        observation: FederatedSubjectObservation<'a>,
    ) -> BoxFuture<'a, Result<FederatedSubjectRecord, StoreError>> {
        Box::pin(PgStore::observe_federated_subject(self, observation))
    }

    fn resolve_federated_subject<'a>(
        &'a self,
        issuer: &'a str,
        subject: &'a str,
    ) -> BoxFuture<'a, Result<CanonicalPrincipal, StoreError>> {
        Box::pin(PgStore::resolve_federated_subject(self, issuer, subject))
    }
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
        federated_subjects_enabled: bool,
    ) -> Result<Self, TaskAuthenticationError> {
        Ok(Self::Identity(
            IdentityTaskIdentityResolver::from_jwks_file(
                issuer,
                audience,
                jwks_file,
                canonical_identities,
                federated_subjects_enabled,
            )?,
        ))
    }

    /// Resolve ratified Identity claims into the authenticated username and groups without
    /// performing Task admission. The administrator authenticator applies ordinary RBAC to the
    /// resulting identity; Task identity alone never grants administrator authority.
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
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    email_verified: Option<bool>,
    #[serde(default)]
    groups: Option<Vec<String>>,
    #[serde(default)]
    actor_login: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    canonical_user_id: Option<String>,
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
        federated_subjects_enabled: bool,
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
            canonical_identities: Arc::new(canonical_identities),
            federated_subjects_enabled,
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
        if claims.identity_contract != IDENTITY_TASK_CONTRACT {
            return Err(TaskAuthenticationError::InvalidCredentials);
        }
        Ok(UserInfo {
            username: claims.email,
            groups: claims.groups,
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
            match claims.identity_contract.as_str() {
                IDENTITY_TASK_CONTRACT => {
                    let identity = task_identity_from_identity_claims(claims.clone())?;
                    self.canonical_identities
                        .resolve_canonical_principal(&identity.canonical_user_id, &identity.owner)
                        .await
                        .map_err(map_canonical_identity_error)?;
                    if self.federated_subjects_enabled {
                        // v2 remains authoritative on its existing verified canonical-user
                        // claims. Transition seeding is deliberately best-effort: a disabled or
                        // conflicting v3 association, or an unavailable observation store, must
                        // not add a new authentication or admission condition to v2.
                        if let Err(error) = self
                            .canonical_identities
                            .seed_federated_subject_association(
                                FederatedSubjectObservation {
                                    issuer: &claims.iss,
                                    subject: &claims.sub,
                                    actor_login: None,
                                    display_name: None,
                                },
                                &identity.canonical_user_id,
                                "task-auth-v2",
                            )
                            .await
                        {
                            eprintln!(
                                "best-effort v2 federated-subject seeding failed: category={}",
                                v2_seed_failure_category(&error)
                            );
                        }
                    }
                    Ok(identity)
                }
                FEDERATED_TASK_CONTRACT if self.federated_subjects_enabled => {
                    let observation = FederatedSubjectObservation {
                        issuer: &claims.iss,
                        subject: &claims.sub,
                        actor_login: claims.actor_login.as_deref(),
                        display_name: claims.display_name.as_deref(),
                    };
                    let principal = if claims.email.is_some() {
                        let compatibility = compatibility_task_identity_from_claims(&claims)?;
                        let principal = self
                            .canonical_identities
                            .resolve_canonical_principal(
                                &compatibility.canonical_user_id,
                                &compatibility.owner,
                            )
                            .await
                            .map_err(map_canonical_identity_error)?;
                        self.canonical_identities
                            .seed_federated_subject_association(
                                observation,
                                &principal.user_id,
                                "task-auth-v2-compatibility",
                            )
                            .await
                            .map_err(|error| {
                                map_federated_subject_error(error, &claims.iss, &claims.sub)
                            })?;
                        principal
                    } else {
                        self.canonical_identities
                            .observe_federated_subject(observation)
                            .await
                            .map_err(|error| {
                                map_federated_subject_error(error, &claims.iss, &claims.sub)
                            })?;
                        self.canonical_identities
                            .resolve_federated_subject(&claims.iss, &claims.sub)
                            .await
                            .map_err(|error| {
                                map_federated_subject_error(error, &claims.iss, &claims.sub)
                            })?
                    };
                    Ok(TaskIdentity {
                        service: "steward-run".to_owned(),
                        acting_user: Some(principal.display_email.clone()),
                        owner: principal.display_email,
                        canonical_user_id: principal.user_id,
                        source_provenance: claims.source_provenance,
                    })
                }
                _ => Err(TaskAuthenticationError::InvalidCredentials),
            }
        })
    }
}

fn v2_seed_failure_category(error: &StoreError) -> &'static str {
    match error {
        StoreError::FederatedSubjectDisabled => "disabled",
        StoreError::FederatedSubjectConflict => "conflict",
        StoreError::Database(_) => "unavailable",
        _ => "rejected",
    }
}

fn map_canonical_identity_error(error: StoreError) -> TaskAuthenticationError {
    match error {
        StoreError::CanonicalIdentityNotFound => TaskAuthenticationError::UnknownUser,
        StoreError::Database(_) | StoreError::InvalidFederatedSubjectRecord => {
            TaskAuthenticationError::Unavailable
        }
        _ => TaskAuthenticationError::InvalidCredentials,
    }
}

fn map_federated_subject_error(
    error: StoreError,
    issuer: &str,
    subject: &str,
) -> TaskAuthenticationError {
    match error {
        StoreError::FederatedSubjectNotFound | StoreError::FederatedSubjectUnassociated => {
            TaskAuthenticationError::Unassociated {
                issuer: issuer.to_owned(),
                subject: subject.to_owned(),
            }
        }
        StoreError::FederatedSubjectDisabled => TaskAuthenticationError::Disabled {
            issuer: issuer.to_owned(),
            subject: subject.to_owned(),
        },
        StoreError::Database(_) | StoreError::InvalidFederatedSubjectRecord => {
            TaskAuthenticationError::Unavailable
        }
        _ => TaskAuthenticationError::InvalidCredentials,
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
    let valid_legacy_identity = claims.identity_contract == IDENTITY_TASK_CONTRACT
        && claims.email_verified == Some(true)
        && claims.email.as_deref().is_some_and(valid_email)
        && claims
            .groups
            .as_ref()
            .is_some_and(|groups| groups.len() <= 16);
    let compatibility_identity_absent =
        claims.email.is_none() && claims.email_verified.is_none() && claims.groups.is_none();
    let compatibility_identity_complete = claims.email_verified == Some(true)
        && claims.email.as_deref().is_some_and(valid_email)
        && claims
            .groups
            .as_ref()
            .is_some_and(|groups| groups.len() <= 16)
        && compatibility_task_identity_from_claims(claims).is_ok();
    let valid_federated_identity = claims.identity_contract == FEDERATED_TASK_CONTRACT
        && valid_github_actions_subject(&claims.sub)
        && claims.canonical_user_id.is_none()
        && claims
            .actor_login
            .as_deref()
            .is_none_or(|value| bounded_display_metadata(value, 128))
        && claims
            .display_name
            .as_deref()
            .is_none_or(|value| bounded_display_metadata(value, 256))
        && (compatibility_identity_absent || compatibility_identity_complete);
    if claims.iss != issuer
        || !audience_matches
        || (!valid_legacy_identity && !valid_federated_identity)
        || !bounded_non_whitespace(&claims.sub, 255)
        || !bounded_non_whitespace(&claims.jti, 128)
        || claims.exp.saturating_add(IDENTITY_CLOCK_SKEW_SECONDS) <= now
        || claims.iat > now.saturating_add(IDENTITY_CLOCK_SKEW_SECONDS)
        || now
            > claims
                .iat
                .saturating_add(MAX_IDENTITY_TASK_TOKEN_AGE_SECONDS)
        || claims.nbf > now.saturating_add(IDENTITY_CLOCK_SKEW_SECONDS)
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
    if claims.identity_contract != IDENTITY_TASK_CONTRACT {
        return Err(TaskAuthenticationError::InvalidCredentials);
    }
    let source_provenance = claims.source_provenance;
    let user = UserInfo {
        username: claims.email,
        groups: claims.groups,
        ..UserInfo::default()
    };
    task_identity_from_kubernetes_user(&user).map(|mut identity| {
        identity.source_provenance = source_provenance;
        identity
    })
}

fn compatibility_task_identity_from_claims(
    claims: &IdentityTaskClaims,
) -> Result<TaskIdentity, TaskAuthenticationError> {
    if claims.email_verified != Some(true) {
        return Err(TaskAuthenticationError::InvalidCredentials);
    }
    task_identity_from_kubernetes_user(&UserInfo {
        username: claims.email.clone(),
        groups: claims.groups.clone(),
        ..UserInfo::default()
    })
}

fn valid_identity_issuer(value: &str) -> bool {
    value.starts_with("https://") && value.len() <= 2_048 && !value.chars().any(char::is_whitespace)
}

fn valid_github_actions_subject(value: &str) -> bool {
    value
        .strip_prefix("github-actions:actor:")
        .is_some_and(|actor_id| {
            !actor_id.is_empty()
                && actor_id.len() <= 20
                && actor_id.bytes().all(|byte| byte.is_ascii_digit())
                && actor_id != "0"
                && !actor_id.starts_with('0')
        })
}

fn bounded_non_whitespace(value: &str, maximum: usize) -> bool {
    !value.is_empty() && value.len() <= maximum && !value.chars().any(char::is_whitespace)
}

fn bounded_display_metadata(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value.trim() == value
        && !value.chars().any(char::is_control)
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

pub trait TaskSubmissionLedger: Clone + Send + Sync + 'static {
    fn has_managed_inference_credential<'a>(
        &'a self,
        _owner_user_id: &'a CanonicalUserId,
    ) -> BoxFuture<'a, Result<bool, StoreError>> {
        Box::pin(async { Ok(false) })
    }

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

    fn envelope_template_allows_inline_browser_tasks<'a>(
        &'a self,
        _template_id: &'a str,
        _revision: i64,
    ) -> BoxFuture<'a, Result<Option<bool>, StoreError>> {
        Box::pin(async { Ok(None) })
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

    /// Contract marker and successful-attempt transcript for runner output delivery.
    fn task_output_transcript<'a>(
        &'a self,
        task_uid: Uuid,
        submitter_service: &'a str,
        owner_user_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<TaskOutputTranscript>, StoreError>>;

    fn request_task_finalization<'a>(
        &'a self,
        task_uid: Uuid,
        submitter_service: &'a str,
        owner_user_id: &'a str,
    ) -> BoxFuture<'a, Result<TaskRecord, StoreError>>;
}

impl TaskSubmissionLedger for PgStore {
    fn has_managed_inference_credential<'a>(
        &'a self,
        owner_user_id: &'a CanonicalUserId,
    ) -> BoxFuture<'a, Result<bool, StoreError>> {
        Box::pin(
            async move { PgStore::has_managed_inference_credential(self, owner_user_id).await },
        )
    }

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
            PgStore::active_provisioned_user_envelopes_by_digest(self, owner_user_id, store_digest)
                .await
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
        Box::pin(
            async move { PgStore::active_provisioned_user_envelopes(self, owner_user_id).await },
        )
    }

    fn envelope_template_allows_inline_browser_tasks<'a>(
        &'a self,
        template_id: &'a str,
        revision: i64,
    ) -> BoxFuture<'a, Result<Option<bool>, StoreError>> {
        Box::pin(async move {
            PgStore::envelope_template_allows_inline_browser_tasks(self, template_id, revision)
                .await
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

    fn task_output_transcript<'a>(
        &'a self,
        task_uid: Uuid,
        submitter_service: &'a str,
        owner_user_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<TaskOutputTranscript>, StoreError>> {
        Box::pin(async move {
            PgStore::task_output_transcript_for_submitter(
                self,
                task_uid,
                submitter_service,
                owner_user_id,
            )
            .await
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
    /// Optional exact active User Envelope selector. The digest is content identity scoped to
    /// the authenticated canonical owner; it is not bearer authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envelope_digest: Option<EnvelopeDigest>,
    /// Optional compatibility assertion for an unversioned legacy Workflow.
    /// Steward selects the runtime from its own Workflow catalog when this is
    /// omitted and rejects any supplied value that does not match the catalog.
    /// Versioned Workflows always reject this field.
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FederatedTaskIdentityErrorResponse {
    pub error: String,
    pub issuer: String,
    pub subject: String,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UnknownTaskIdentityErrorResponse {
    pub error: String,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(untagged)]
pub enum TaskIdentityErrorResponse {
    Unknown(UnknownTaskIdentityErrorResponse),
    Federated(FederatedTaskIdentityErrorResponse),
}

#[derive(utoipa::ToSchema)]
#[schema(
    value_type = String,
    format = Binary,
    description = "Opaque tar archive, limited to 67,108,864 raw bytes"
)]
pub struct TaskArchive(pub Vec<u8>);

#[derive(Clone)]
struct TaskApiState<L, I> {
    identities: I,
    application: TaskApplicationService<L>,
}

/// The single internal application boundary for Task resolution, admission, reservation,
/// and immutable execution-plan snapshotting.
#[derive(Clone)]
struct TaskApplicationService<L> {
    ledger: L,
    config: TaskApiConfig,
}

#[derive(Clone)]
pub(crate) struct BrowserTaskState<L> {
    application: TaskApplicationService<L>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserTaskRerunError {
    Unsupported,
    EnvelopeUnavailable,
    InferenceKeyMissing,
    Rejected,
    Unavailable,
}

pub trait BrowserTaskRerunner: Send + Sync {
    fn rerun<'a>(
        &'a self,
        session: &'a BrowserSessionContext,
        source: &'a TaskRecord,
        idempotency_key: &'a str,
    ) -> BoxFuture<'a, Result<Uuid, BrowserTaskRerunError>>;
}

#[derive(Clone)]
struct TaskApplicationBrowserRerunner<L> {
    application: TaskApplicationService<L>,
}

pub fn browser_task_rerunner<L>(ledger: L, config: TaskApiConfig) -> Arc<dyn BrowserTaskRerunner>
where
    L: AdmissionLedger + TaskSubmissionLedger,
{
    Arc::new(TaskApplicationBrowserRerunner {
        application: TaskApplicationService { ledger, config },
    })
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BrowserRunSubmissionResponse {
    #[schema(value_type = String, format = "uuid")]
    pub task_uid: Uuid,
    pub phase: TaskPhase,
    pub origin: TaskOrigin,
    pub package: BrowserResolvedPackage,
    pub envelope: BrowserResolvedEnvelope,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BrowserResolvedPackage {
    pub source: String,
    pub revision: String,
    pub path: RelativePath,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BrowserResolvedEnvelope {
    pub instance_id: String,
    pub revision: i64,
    pub digest: String,
}

pub fn browser_task_router<L>(
    ledger: L,
    config: TaskApiConfig,
    browser_auth: BrowserAuthService,
) -> Router
where
    L: AdmissionLedger + TaskSubmissionLedger,
{
    let routes = Router::new()
        .route("/app/api/v1/runs", post(submit_browser_run::<L>))
        .layer(DefaultBodyLimit::max(
            steward_types::direct_package::MAX_INLINE_PACKAGE_BYTES
                + steward_types::direct_package::MAX_BROWSER_INPUT_BYTES
                + 16 * 1024,
        ))
        .with_state(BrowserTaskState {
            application: TaskApplicationService { ledger, config },
        });
    protect_browser_routes(routes, browser_auth)
}

#[utoipa::path(
    post,
    path = "/app/api/v1/runs",
    request_body = BrowserTaskSubmission,
    params(
        ("Idempotency-Key" = String, Header),
        ("X-Steward-CSRF" = String, Header)
    ),
    responses(
        (status = 202, body = BrowserRunSubmissionResponse),
        (status = 400, description = "Browser package request is invalid"),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 409, description = "Envelope or idempotency selection is ambiguous"),
        (status = 422, description = "Package authority exceeds the selected Envelope"),
        (status = 503, description = "Task submission is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn submit_browser_run<L>(
    session: Option<Extension<BrowserSessionContext>>,
    mutation: Option<Extension<BrowserMutationProof>>,
    State(state): State<BrowserTaskState<L>>,
    headers: HeaderMap,
    Json(request): Json<BrowserTaskSubmission>,
) -> Response
where
    L: AdmissionLedger + TaskSubmissionLedger,
{
    let (Some(Extension(session)), Some(Extension(_))) = (session, mutation) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Some(idempotency_key) = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty() && value.len() <= 255)
    else {
        return ApiError::Admission("Idempotency-Key is required".to_owned()).into_response();
    };
    let identity = TaskIdentity {
        service: "steward-browser".to_owned(),
        acting_user: Some(session.principal.display_email.clone()),
        owner: session.principal.display_email,
        canonical_user_id: session.principal.canonical_user_id,
        source_provenance: None,
    };
    match state
        .application
        .submit_browser(idempotency_key, identity, &request)
        .await
    {
        Ok(response) => (StatusCode::ACCEPTED, Json(response)).into_response(),
        Err(error) => error.into_response(),
    }
}

pub fn task_router<L, I>(ledger: L, identities: I, config: TaskApiConfig) -> Router
where
    L: AdmissionLedger + TaskSubmissionLedger,
    I: TaskIdentityResolver,
{
    Router::new()
        .route("/v1/tasks", post(submit_task::<L, I>))
        .route("/v1/tasks/{task_uid}/inputs", put(put_task_inputs::<L, I>))
        .route("/v1/tasks/{task_uid}/execute", post(execute_task::<L, I>))
        .route(
            "/v1/tasks/{task_uid}/outputs",
            get(get_task_outputs::<L, I>),
        )
        .route(
            "/v1/tasks/{task_uid}",
            get(get_task::<L, I>).delete(delete_task::<L, I>),
        )
        .layer(DefaultBodyLimit::max(MAX_TASK_INPUT_ARCHIVE_BYTES))
        .with_state(TaskApiState {
            identities,
            application: TaskApplicationService { ledger, config },
        })
}

async fn get_task_outputs<L, I>(
    State(state): State<TaskApiState<L, I>>,
    Path(task_uid): Path<Uuid>,
    headers: HeaderMap,
) -> Response
where
    L: AdmissionLedger + TaskSubmissionLedger,
    I: TaskIdentityResolver,
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
    let Some(archive) = record.output_archive else {
        return ApiError::TaskOutputNotReady.into_response();
    };
    let execution_log = record
        .direct_task_evidence
        .as_ref()
        .map(|evidence| evidence.diagnostics.execution_log);
    let archive = if execution_log == Some(ExecutionLogMode::Full) {
        let transcript = match state
            .application
            .ledger
            .task_output_transcript(
                task_uid,
                &identity.service,
                identity.canonical_user_id.as_str(),
            )
            .await
        {
            Ok(Some(transcript)) => transcript,
            Ok(None) => return ApiError::TaskOutputNotReady.into_response(),
            Err(error) => return ApiError::Store(error).into_response(),
        };
        match runner_output_archive_with_transcript(archive, transcript) {
            Ok(archive) => archive,
            Err(error) => return error.into_response(),
        }
    } else {
        archive
    };
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/x-tar")],
        archive,
    )
        .into_response()
}

/// Deliver the runner-facing archive for a Task whose snapshotted diagnostics requested
/// `executionLog: full`.
///
/// The authenticated runner download is the only place the reserved transcript re-enters an
/// output archive: storage, the controller's `out/`-only validation, and the browser output
/// views keep seeing the stored archive. A `steward.task-output/v1` archive is rebuilt from its
/// validated entries with Steward-written headers and gains the successful attempt's
/// transcript; a historical archive without that marker already embeds its transcript and is
/// delivered unchanged. A missing or oversized transcript, or a stored archive that violates
/// its contract, is a permanent condition: it fails with a bounded, non-retryable error rather
/// than an archive the runner would reject after a successful Task.
fn runner_output_archive_with_transcript(
    archive: Vec<u8>,
    transcript: TaskOutputTranscript,
) -> Result<Vec<u8>, ApiError> {
    if transcript.output_archive_contract.as_deref() != Some(TASK_OUTPUT_ARCHIVE_CONTRACT) {
        return Ok(archive);
    }
    let (Some(stdout), Some(stderr)) = (transcript.execution_stdout, transcript.execution_stderr)
    else {
        return Err(ApiError::TaskOutputDeliveryFailed(
            "execution_transcript_unavailable",
        ));
    };
    task_output_archive_with_execution_transcript(archive, &stdout, &stderr).map_err(|error| {
        ApiError::TaskOutputDeliveryFailed(match error {
            TaskOutputTranscriptError::TranscriptTooLarge => "execution_transcript_too_large",
            TaskOutputTranscriptError::Archive(_) => "output_archive_contract_violation",
        })
    })
}

async fn delete_task<L, I>(
    State(state): State<TaskApiState<L, I>>,
    Path(task_uid): Path<Uuid>,
    headers: HeaderMap,
) -> Response
where
    L: AdmissionLedger + TaskSubmissionLedger,
    I: TaskIdentityResolver,
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

async fn execute_task<L, I>(
    State(state): State<TaskApiState<L, I>>,
    Path(task_uid): Path<Uuid>,
    headers: HeaderMap,
) -> Response
where
    L: AdmissionLedger + TaskSubmissionLedger,
    I: TaskIdentityResolver,
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

async fn get_task<L, I>(
    State(state): State<TaskApiState<L, I>>,
    Path(task_uid): Path<Uuid>,
    headers: HeaderMap,
) -> Response
where
    L: AdmissionLedger + TaskSubmissionLedger,
    I: TaskIdentityResolver,
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

async fn put_task_inputs<L, I>(
    State(state): State<TaskApiState<L, I>>,
    Path(task_uid): Path<Uuid>,
    headers: HeaderMap,
    archive: Result<Bytes, BytesRejection>,
) -> Response
where
    L: AdmissionLedger + TaskSubmissionLedger,
    I: TaskIdentityResolver,
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
    if let Some(existing) = record.input_archive.as_deref() {
        let matches = split_task_input_archive(existing)
            .is_ok_and(|parts| parts.caller_archive == archive.as_ref());
        return if matches {
            StatusCode::NO_CONTENT.into_response()
        } else {
            ApiError::Store(StoreError::InvalidTaskTransition).into_response()
        };
    }
    let workspace = record
        .direct_task_evidence
        .as_ref()
        .and_then(|evidence| evidence.workspace.as_ref())
        .or_else(|| {
            record
                .browser_task_evidence
                .as_ref()
                .and_then(|evidence| evidence.workspace.as_ref())
        });
    let archive =
        match prepare_task_input_archive(&state.application.config, &archive, workspace).await {
            Ok(archive) => archive,
            Err(error) => return error.into_response(),
        };
    match state
        .application
        .ledger
        .put_task_inputs(
            task_uid,
            &identity.service,
            identity.canonical_user_id.as_str(),
            archive.as_ref(),
        )
        .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => ApiError::Store(error).into_response(),
    }
}

async fn submit_task<L, I>(
    State(state): State<TaskApiState<L, I>>,
    headers: HeaderMap,
    Json(request): Json<TaskCreateRequest>,
) -> Response
where
    L: AdmissionLedger + TaskSubmissionLedger,
    I: TaskIdentityResolver,
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

impl<L> TaskApplicationService<L>
where
    L: AdmissionLedger + TaskSubmissionLedger,
{
    async fn submit_browser(
        &self,
        idempotency_key: &str,
        identity: TaskIdentity,
        request: &BrowserTaskSubmission,
    ) -> Result<BrowserRunSubmissionResponse, ApiError> {
        request.validate().map_err(ApiError::Admission)?;
        if !self.config.orchestration_mode.is_active() {
            return Err(ApiError::TaskRuntimeContractUnavailable(
                "Task submission is disabled during the staged orchestration rollout".to_owned(),
            ));
        }
        let (mut record, evidence) = if request.package.source.starts_with("steward:registry/") {
            self.submit_browser_registry(idempotency_key, identity.clone(), request)
                .await
                .map_err(|error| self.browser_task_persistence_error(error))?
        } else {
            self.submit_browser_package(idempotency_key, identity.clone(), request)
                .await
                .map_err(|error| self.browser_task_persistence_error(error))?
        };
        let caller_input_archive = browser_inputs_archive(&request.inputs)?;
        let input_archive = prepare_task_input_archive(
            &self.config,
            &caller_input_archive,
            evidence.workspace.as_ref(),
        )
        .await?;
        if record.browser_task_evidence.as_ref() != Some(&evidence) {
            return Err(ApiError::Store(StoreError::TaskIdempotencyConflict));
        }
        match record.input_archive.as_deref() {
            Some(existing) if existing != input_archive => {
                return Err(ApiError::Store(StoreError::TaskIdempotencyConflict));
            }
            Some(_) => {}
            None => {
                record = self
                    .ledger
                    .put_task_inputs(
                        record.task_uid,
                        &identity.service,
                        identity.canonical_user_id.as_str(),
                        &input_archive,
                    )
                    .await
                    .map_err(|error| self.browser_task_store_error(error))?;
            }
        }
        if !record.execute_requested {
            record = self
                .ledger
                .request_task_execution(
                    record.task_uid,
                    &identity.service,
                    identity.canonical_user_id.as_str(),
                )
                .await
                .map_err(|error| self.browser_task_store_error(error))?;
        }
        browser_submission_response(record)
            .map_err(|error| self.browser_task_persistence_error(error))
    }

    async fn submit_browser_registry(
        &self,
        idempotency_key: &str,
        identity: TaskIdentity,
        request: &BrowserTaskSubmission,
    ) -> Result<(TaskRecord, BrowserTaskEvidence), ApiError> {
        let name = request
            .package
            .source
            .strip_prefix("steward:registry/")
            .ok_or_else(|| ApiError::Admission("registry package source is invalid".to_owned()))?;
        let version = request
            .package
            .revision
            .as_deref()
            .and_then(|value| value.strip_prefix("steward:version:"))
            .and_then(|value| value.parse::<i64>().ok())
            .filter(|value| *value > 0)
            .ok_or_else(|| {
                ApiError::Admission("registry package revision is invalid".to_owned())
            })?;
        let workflow = self
            .ledger
            .workflow_revision(name, version)
            .await
            .map_err(ApiError::Store)?
            .ok_or(ApiError::TaskWorkflowNotFound)?;
        let closure_digest = ContentDigest::parse(format!("steward:{}", workflow.content_digest))
            .map_err(ApiError::Admission)?;
        let evidence = BrowserTaskEvidence {
            source: request.package.source.clone(),
            revision: request.package.revision.clone().ok_or_else(|| {
                ApiError::Admission("registry package revision is required".to_owned())
            })?,
            path: request.package.path.clone(),
            closure: None,
            closure_digest,
            inline_files: None,
            diagnostics: request.diagnostics,
            workspace: None,
            prompt_source: PromptSourceKind::Path,
        };
        evidence.validate().map_err(ApiError::Admission)?;
        let task_request = TaskSubmissionRequest {
            workflow: format!("{name}@{version}"),
            envelope_digest: request.envelope_digest.clone(),
            coding_agent_runtime: None,
            agent_runtime_uid: None,
        };
        let reference = WorkflowReference {
            name: name.to_owned(),
            version,
        };
        submit_versioned_task(
            self,
            idempotency_key,
            identity.clone(),
            reference,
            &task_request,
            Some(&evidence),
        )
        .await
        .map_err(|error| self.browser_task_persistence_error(error))?;
        let record = self
            .ledger
            .task_by_idempotency(
                &identity.service,
                identity.canonical_user_id.as_str(),
                idempotency_key,
            )
            .await
            .map_err(ApiError::Store)?
            .ok_or(ApiError::Store(StoreError::TaskNotFound))?;
        Ok((record, evidence))
    }

    async fn submit_browser_package(
        &self,
        idempotency_key: &str,
        identity: TaskIdentity,
        request: &BrowserTaskSubmission,
    ) -> Result<(TaskRecord, BrowserTaskEvidence), ApiError> {
        let existing = self
            .ledger
            .task_by_idempotency(
                &identity.service,
                identity.canonical_user_id.as_str(),
                idempotency_key,
            )
            .await
            .map_err(ApiError::Store)?;
        let resolved =
            resolve_browser_package_pre_admission(&self.ledger, &self.config, &identity, request)
                .await?;
        if let Some(record) = existing {
            let envelope_digest = resolved
                .envelope
                .envelope_digest
                .as_deref()
                .ok_or(ApiError::MissingEnvelope)?;
            validate_browser_task_retry(&identity, &resolved.evidence, envelope_digest, &record)?;
            return Ok((record, resolved.evidence));
        }
        let approved = resolved
            .envelope
            .approved_envelope
            .as_ref()
            .ok_or(ApiError::MissingEnvelope)?;
        let envelope_instance_id = resolved
            .envelope
            .envelope_instance_id
            .as_deref()
            .ok_or(ApiError::MissingEnvelope)?;
        let envelope_digest = resolved
            .envelope
            .envelope_digest
            .as_deref()
            .ok_or(ApiError::MissingEnvelope)?;
        let decision = AdmissionDecision::Admit;
        require_managed_inference_credential(self, &identity, &resolved.spec).await?;
        let task_uid = Uuid::new_v4();
        let operation_id = Uuid::new_v4();
        let runtime_name = stable_task_runtime_name(operation_id);
        let orchestration = task_orchestration_reservation(
            task_uid,
            operation_id,
            VERSIONED_WORKFLOW_NAMESPACE,
            &runtime_name,
            &resolved.spec,
            approved,
            Some(&resolved.execution_binding),
        )?;
        let workflow = format!(
            "direct:{}@{}",
            resolved.definition.name.as_str(),
            resolved.definition.version
        );
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
                coding_agent_runtime: resolved.definition.runtime.agent_ref.as_str(),
                runtime_uid: None,
                runtime_namespace: VERSIONED_WORKFLOW_NAMESPACE,
                runtime_name: &runtime_name,
                runtime_ownership: RuntimeOwnership::Provisioned,
                runtime_spec: &resolved.spec,
                agent_command: &resolved.command,
                execution_binding: Some(&resolved.execution_binding),
                source_provenance: None,
                direct_task_evidence: None,
                task_origin: TaskOrigin::Browser,
                browser_task_evidence: Some(&resolved.evidence),
                user_envelope_snapshot: Some(approved),
                candidate_digest: &orchestration.candidate_digest,
                admission_decision: &decision,
                inert_manifest_digest: &orchestration.inert_manifest_digest,
                active_manifest_digest: &orchestration.active_manifest_digest,
            })
            .await
            .map_err(|error| self.browser_task_store_error(error))?;
        Ok((reservation.record, resolved.evidence))
    }

    fn browser_task_store_error(&self, error: StoreError) -> ApiError {
        if matches!(error, StoreError::Database(_)) {
            self.config.report_browser_task_persistence_failure(&error);
            ApiError::TaskPersistenceFailed
        } else {
            ApiError::Store(error)
        }
    }

    fn browser_task_persistence_error(&self, error: ApiError) -> ApiError {
        match error {
            ApiError::Store(error @ StoreError::Database(_)) => {
                self.browser_task_store_error(error)
            }
            error => error,
        }
    }

    async fn submit_direct(
        &self,
        idempotency_key: &str,
        identity: TaskIdentity,
        request: &DirectTaskSubmission,
    ) -> Result<(StatusCode, TaskStatusResponse), ApiError> {
        if request.invocation_path.is_some() == request.package_path.is_some() {
            return Err(ApiError::InvalidRequest(
                "exactly one of invocationPath or packagePath is required".to_owned(),
            ));
        }
        request.validate().map_err(ApiError::Admission)?;
        if self.config.direct_git_resolver.is_none() {
            self.config.report_direct_package_source_disabled();
            return Err(ApiError::DirectPackageSourceDisabled);
        }
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
        let decision = evaluate_with_grants(&spec, approved, &[])
            .map_err(|error| ApiError::Admission(format!("{error:?}")))?;
        if !matches!(decision, AdmissionDecision::Admit) {
            return Err(ApiError::Admission(
                "Task requirements exceed the provisioned User Envelope".to_owned(),
            ));
        }
        require_managed_inference_credential(self, &identity, &spec).await?;
        let operation_id = Uuid::new_v4();
        let runtime_name = stable_task_runtime_name(operation_id);
        let orchestration = task_orchestration_reservation(
            task_uid,
            operation_id,
            VERSIONED_WORKFLOW_NAMESPACE,
            &runtime_name,
            &spec,
            approved,
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
                source_provenance: Some(&evidence.source_provenance),
                direct_task_evidence: Some(&evidence),
                task_origin: TaskOrigin::GithubActions,
                browser_task_evidence: None,
                user_envelope_snapshot: Some(approved),
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
        if reference.is_none() {
            return Err(ApiError::Admission(
                "governed Tasks require a provisioned User Envelope and a supported direct-package or versioned workflow contract"
                    .to_owned(),
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
            return submit_versioned_task(
                self,
                idempotency_key,
                identity,
                reference,
                request,
                None,
            )
            .await;
        }
        Err(ApiError::Admission(
            "governed Tasks require a provisioned User Envelope and a supported direct-package or versioned workflow contract"
                .to_owned(),
        ))
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

impl<L> BrowserTaskRerunner for TaskApplicationBrowserRerunner<L>
where
    L: AdmissionLedger + TaskSubmissionLedger,
{
    fn rerun<'a>(
        &'a self,
        session: &'a BrowserSessionContext,
        source: &'a TaskRecord,
        idempotency_key: &'a str,
    ) -> BoxFuture<'a, Result<Uuid, BrowserTaskRerunError>> {
        Box::pin(async move {
            if source.task_origin != TaskOrigin::Browser {
                return Err(BrowserTaskRerunError::Unsupported);
            }
            let evidence = source
                .browser_task_evidence
                .as_ref()
                .ok_or(BrowserTaskRerunError::Unsupported)?;
            let input_archive = source
                .input_archive
                .as_deref()
                .ok_or(BrowserTaskRerunError::Unsupported)?;
            let inputs = browser_inputs_from_archive(input_archive)
                .map_err(|_| BrowserTaskRerunError::Unsupported)?;
            let envelope_instance_id = source
                .user_envelope_instance_id
                .as_deref()
                .ok_or(BrowserTaskRerunError::EnvelopeUnavailable)?;
            let active = self
                .application
                .ledger
                .active_provisioned_user_envelopes(&session.principal.canonical_user_id)
                .await
                .map_err(|_| BrowserTaskRerunError::Unavailable)?;
            let mut matching = active.into_iter().filter(|record| {
                record.envelope_instance_id.as_deref() == Some(envelope_instance_id)
            });
            let current = matching
                .next()
                .ok_or(BrowserTaskRerunError::EnvelopeUnavailable)?;
            if matching.next().is_some() {
                return Err(BrowserTaskRerunError::EnvelopeUnavailable);
            }
            let digest = current
                .envelope_digest
                .ok_or(BrowserTaskRerunError::EnvelopeUnavailable)?;
            let envelope_digest = EnvelopeDigest::parse(if digest.starts_with("steward:") {
                digest
            } else {
                format!("steward:{digest}")
            })
            .map_err(|_| BrowserTaskRerunError::EnvelopeUnavailable)?;
            let request = browser_rerun_submission(evidence, inputs, envelope_digest);
            let identity = TaskIdentity {
                service: "steward-browser".to_owned(),
                acting_user: Some(session.principal.display_email.clone()),
                owner: session.principal.display_email.clone(),
                canonical_user_id: session.principal.canonical_user_id.clone(),
                source_provenance: None,
            };
            self.application
                .submit_browser(idempotency_key, identity, &request)
                .await
                .map(|response| response.task_uid)
                .map_err(|error| match error {
                    ApiError::MissingEnvelope
                    | ApiError::Conflict(_)
                    | ApiError::Store(StoreError::StaleEnvelope) => {
                        BrowserTaskRerunError::EnvelopeUnavailable
                    }
                    ApiError::Admission(_)
                    | ApiError::BrowserTaskSourceUnauthorized
                    | ApiError::TaskSourceUnauthorized(_)
                    | ApiError::TaskWorkflowNotFound
                    | ApiError::DirectPackageSourceDisabled => BrowserTaskRerunError::Rejected,
                    ApiError::InferenceKeyMissing => BrowserTaskRerunError::InferenceKeyMissing,
                    _ => BrowserTaskRerunError::Unavailable,
                })
        })
    }
}

fn browser_rerun_submission(
    evidence: &BrowserTaskEvidence,
    inputs: serde_json::Value,
    envelope_digest: EnvelopeDigest,
) -> BrowserTaskSubmission {
    BrowserTaskSubmission {
        package: steward_types::direct_package::BrowserPackageLocator {
            source: evidence.source.clone(),
            revision: Some(evidence.revision.clone()),
            path: evidence.path.clone(),
            files: evidence.inline_files.clone(),
        },
        envelope_digest: Some(envelope_digest),
        inputs,
        diagnostics: evidence.diagnostics,
    }
}

async fn resolve_workspace_evidence<L>(
    ledger: &L,
    config: &TaskApiConfig,
    entries: &[WorkspaceEntry],
    context: WorkspaceResolutionContext<'_>,
) -> Result<Option<WorkspaceEvidence>, ApiError>
where
    L: TaskSubmissionLedger,
{
    if entries.is_empty() {
        return Ok(None);
    }
    let git = config.direct_git_resolver.as_ref();
    let mut resolved = Vec::with_capacity(entries.len());
    let mut names = BTreeSet::new();
    let mut materialized_bytes = 0_u64;
    let mut materialized_files = 0_u64;
    for entry in entries {
        validate_workspace_deployment_policy(&config.workspace_policy, entry)?;
        let resolved_entry = match entry {
            WorkspaceEntry::Git(entry) => {
                let git = git.ok_or_else(|| {
                    ApiError::TaskRuntimeContractUnavailable(
                        "workspace_git_source_resolver_unavailable".to_owned(),
                    )
                })?;
                let (repository, is_self) = match &entry.git.repository {
                    WorkspaceGitRepository::SelfRepository => (
                        context.self_repository.cloned().ok_or_else(|| {
                            ApiError::Admission("workspace_git_self_unavailable".to_owned())
                        })?,
                        true,
                    ),
                    WorkspaceGitRepository::Explicit(reference) => {
                        let reference = GitRepositoryReference {
                            repository_owner_id: reference.owner_id.clone(),
                            repository_id: reference.repository_id.clone(),
                        };
                        (
                            describe_workspace_repository(git.as_ref(), &reference)
                                .await?
                                .0,
                            false,
                        )
                    }
                };
                if !is_self
                    && !workspace_repository_is_authorized(
                        ledger,
                        config,
                        context.caller,
                        context.browser,
                        &repository,
                    )
                    .await?
                {
                    return Err(ApiError::Admission(
                        "workspace_git_repository_not_admitted".to_owned(),
                    ));
                }
                let (_, description) = describe_workspace_repository(
                    git.as_ref(),
                    &GitRepositoryReference {
                        repository_owner_id: repository.repository_owner_id.clone(),
                        repository_id: repository.repository_id.clone(),
                    },
                )
                .await?;
                let name = match &entry.git.name {
                    Some(name) => name.clone(),
                    None => WorkspaceName::parse(description.name.clone()).map_err(|_| {
                        ApiError::Admission(
                            "workspace_git_name_requires_explicit_valid_name".to_owned(),
                        )
                    })?,
                };
                let commit = match &entry.git.git_ref {
                    steward_types::direct_package::WorkspaceGitRef::Exact(commit) => commit.clone(),
                    steward_types::direct_package::WorkspaceGitRef::Trigger
                        if is_self && context.trigger_commit.is_some() =>
                    {
                        context.trigger_commit.cloned().ok_or_else(|| {
                            ApiError::Admission("workspace_git_trigger_unavailable".to_owned())
                        })?
                    }
                    steward_types::direct_package::WorkspaceGitRef::Trigger => git
                        .resolve_revision(&GitRevisionRequest {
                            repository: repository.clone(),
                            reference: description.default_branch.clone(),
                        })
                        .await
                        .map_err(source_port_error)?,
                };
                let resolution = if entry.git.submodules.mode == WorkspaceSubmoduleMode::Admitted {
                    resolve_workspace_submodules(
                        ledger,
                        config,
                        git.as_ref(),
                        &repository,
                        &commit,
                        &entry.git.paths,
                        &entry.git.submodules.required,
                        entry.git.submodules.recursive,
                        config.workspace_policy.max_submodule_depth,
                        context.caller,
                        context.browser,
                    )
                    .await?
                } else {
                    let tree = read_workspace_tree(git.as_ref(), &repository, &commit).await?;
                    WorkspaceSubmoduleResolution {
                        entries: Vec::new(),
                        inventory: workspace_tree_inventory(&tree, &entry.git.paths)?,
                    }
                };
                let entry_size_limit = workspace_entry_size_limit(
                    entry.git.limits.as_ref(),
                    config.workspace_policy.max_total_bytes,
                )?;
                let entry_file_limit = entry
                    .git
                    .limits
                    .as_ref()
                    .and_then(|limits| limits.files)
                    .unwrap_or(config.workspace_policy.max_files);
                if resolution.inventory.bytes > entry_size_limit
                    || resolution.inventory.files > entry_file_limit
                {
                    return Err(ApiError::Admission("workspace_limit_exceeded".to_owned()));
                }
                materialized_bytes = materialized_bytes
                    .checked_add(resolution.inventory.bytes)
                    .ok_or_else(|| ApiError::Admission("workspace_limit_exceeded".to_owned()))?;
                materialized_files = materialized_files
                    .checked_add(resolution.inventory.files)
                    .ok_or_else(|| ApiError::Admission("workspace_limit_exceeded".to_owned()))?;
                if materialized_bytes > config.workspace_policy.max_total_bytes
                    || materialized_bytes > config.workspace_policy.scratch_volume_bytes
                    || materialized_files > config.workspace_policy.max_files
                {
                    return Err(ApiError::Admission("workspace_limit_exceeded".to_owned()));
                }
                let submodules = resolution.entries;
                let content_digest = workspace_entry_digest(&serde_json::json!({
                    "type": "git",
                    "name": name,
                    "access": entry.git.access,
                    "repository": repository.repository,
                    "repositoryId": repository.repository_id,
                    "repositoryOwnerId": repository.repository_owner_id,
                    "commit": commit,
                    "history": entry.git.history,
                    "paths": entry.git.paths,
                    "submodules": submodules,
                    "limits": entry.git.limits,
                }))?;
                ResolvedWorkspaceEntry::Git {
                    name,
                    access: entry.git.access,
                    repository: repository.repository,
                    repository_id: repository.repository_id,
                    repository_owner_id: repository.repository_owner_id,
                    commit,
                    history: entry.git.history.clone(),
                    paths: entry.git.paths.clone(),
                    submodules,
                    content_digest,
                }
            }
            WorkspaceEntry::Scratch(entry) => {
                let name = entry
                    .scratch
                    .name
                    .clone()
                    .unwrap_or(WorkspaceName::parse("scratch").map_err(ApiError::Admission)?);
                let content_digest = workspace_entry_digest(&serde_json::json!({
                    "type": "scratch",
                    "name": name,
                    "size": entry.scratch.size,
                    "limits": entry.scratch.limits,
                }))?;
                ResolvedWorkspaceEntry::Scratch {
                    name,
                    size: entry.scratch.size.clone(),
                    content_digest,
                }
            }
        };
        if !names.insert(resolved_entry.name().as_str().to_owned()) {
            return Err(ApiError::Admission(
                "workspace_entry_name_conflict".to_owned(),
            ));
        }
        resolved.push(resolved_entry);
    }
    let workspace_digest = workspace_entry_digest(&resolved)?;
    let evidence = WorkspaceEvidence {
        entries: resolved,
        workspace_digest,
    };
    evidence.validate().map_err(ApiError::Admission)?;
    Ok(Some(evidence))
}

fn parse_workspace_size_bytes(value: &str) -> Option<u64> {
    let (amount, multiplier) = [
        ("Ti", 1024_u64.pow(4)),
        ("Gi", 1024_u64.pow(3)),
        ("Mi", 1024_u64.pow(2)),
        ("Ki", 1024_u64),
        ("T", 1000_u64.pow(4)),
        ("G", 1000_u64.pow(3)),
        ("M", 1000_u64.pow(2)),
        ("K", 1000_u64),
    ]
    .into_iter()
    .find_map(|(suffix, multiplier)| {
        value
            .strip_suffix(suffix)
            .map(|amount| (amount, multiplier))
    })
    .unwrap_or((value, 1));
    amount
        .parse::<u64>()
        .ok()
        .filter(|amount| *amount > 0)
        .and_then(|amount| amount.checked_mul(multiplier))
}

fn validate_workspace_deployment_policy(
    policy: &WorkspaceDeploymentPolicy,
    entry: &WorkspaceEntry,
) -> Result<(), ApiError> {
    let (entry_type, limits) = match entry {
        WorkspaceEntry::Git(entry) => {
            match entry.git.history {
                WorkspaceGitHistory::Depth(depth) if depth > policy.max_history_depth => {
                    return Err(ApiError::Admission("workspace_limit_exceeded".to_owned()));
                }
                WorkspaceGitHistory::Full => {
                    return Err(ApiError::Admission("workspace_limit_exceeded".to_owned()));
                }
                _ => {}
            }
            if entry.git.submodules.recursive && policy.max_submodule_depth == 0 {
                return Err(ApiError::Admission("workspace_limit_exceeded".to_owned()));
            }
            ("git", entry.git.limits.as_ref())
        }
        WorkspaceEntry::Scratch(entry) => {
            let size = parse_workspace_size_bytes(entry.scratch.size.as_str())
                .ok_or_else(|| ApiError::Admission("workspace_limit_invalid".to_owned()))?;
            if size > policy.max_total_bytes {
                return Err(ApiError::Admission("workspace_limit_exceeded".to_owned()));
            }
            if size != policy.scratch_volume_bytes {
                return Err(ApiError::Admission(
                    "workspace_scratch_size_unavailable".to_owned(),
                ));
            }
            if let Some(limit) = entry
                .scratch
                .limits
                .as_ref()
                .and_then(|limits| limits.size.as_ref())
            {
                let limit = parse_workspace_size_bytes(limit.as_str())
                    .ok_or_else(|| ApiError::Admission("workspace_limit_invalid".to_owned()))?;
                if size > limit {
                    return Err(ApiError::Admission("workspace_limit_exceeded".to_owned()));
                }
            }
            ("scratch", entry.scratch.limits.as_ref())
        }
    };
    if !policy.enabled_types.contains(entry_type) {
        return Err(ApiError::Admission(
            "workspace_entry_type_not_allowed".to_owned(),
        ));
    }
    if let Some(files) = limits.and_then(|limits| limits.files)
        && files > policy.max_files
    {
        return Err(ApiError::Admission("workspace_limit_exceeded".to_owned()));
    }
    if let Some(size) = limits.and_then(|limits| limits.size.as_ref()) {
        let size = parse_workspace_size_bytes(size.as_str())
            .ok_or_else(|| ApiError::Admission("workspace_limit_invalid".to_owned()))?;
        if size > policy.max_total_bytes {
            return Err(ApiError::Admission("workspace_limit_exceeded".to_owned()));
        }
    }
    Ok(())
}

fn workspace_entry_size_limit(
    limits: Option<&steward_types::direct_package::WorkspaceLimits>,
    deployment_limit: u64,
) -> Result<u64, ApiError> {
    limits
        .and_then(|limits| limits.size.as_ref())
        .map(|size| {
            parse_workspace_size_bytes(size.as_str())
                .ok_or_else(|| ApiError::Admission("workspace_limit_invalid".to_owned()))
                .and_then(|size| {
                    if size <= deployment_limit {
                        Ok(size)
                    } else {
                        Err(ApiError::Admission("workspace_limit_exceeded".to_owned()))
                    }
                })
        })
        .transpose()
        .map(|limit| limit.unwrap_or(deployment_limit))
}

const MAX_WORKSPACE_GIT_TREE_ENTRIES: usize = 100_000;
const MAX_GITMODULES_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Copy, Default)]
struct WorkspaceTreeInventory {
    files: u64,
    bytes: u64,
}

impl WorkspaceTreeInventory {
    fn add(&mut self, other: Self) -> Result<(), ApiError> {
        self.files = self
            .files
            .checked_add(other.files)
            .ok_or_else(|| ApiError::Admission("workspace_limit_exceeded".to_owned()))?;
        self.bytes = self
            .bytes
            .checked_add(other.bytes)
            .ok_or_else(|| ApiError::Admission("workspace_limit_exceeded".to_owned()))?;
        Ok(())
    }
}

struct WorkspaceSubmoduleResolution {
    entries: Vec<ResolvedWorkspaceSubmodule>,
    inventory: WorkspaceTreeInventory,
}

async fn read_workspace_tree(
    git: &dyn DirectGitResolver,
    repository: &GitRepositoryIdentity,
    commit: &steward_types::direct_package::ExactGitCommit,
) -> Result<GitTree, ApiError> {
    let tree = git
        .read_tree(&GitTreeRequest {
            repository: repository.clone(),
            commit: commit.clone(),
            max_entries: MAX_WORKSPACE_GIT_TREE_ENTRIES,
        })
        .await
        .map_err(source_port_error)?;
    if tree.repository != *repository || tree.commit != *commit {
        return Err(ApiError::Admission(
            "workspace_git_tree_identity_mismatch".to_owned(),
        ));
    }
    Ok(tree)
}

fn workspace_tree_inventory(
    tree: &GitTree,
    paths: &[WorkspacePath],
) -> Result<WorkspaceTreeInventory, ApiError> {
    let mut inventory = WorkspaceTreeInventory::default();
    for entry in tree.entries.iter().filter(|entry| entry.kind == "blob") {
        if !paths.is_empty()
            && !paths.iter().any(|path| {
                let selected = path.as_str().strip_suffix('/').unwrap_or(path.as_str());
                entry.path.as_str() == selected
                    || entry
                        .path
                        .as_str()
                        .strip_prefix(selected)
                        .is_some_and(|suffix| suffix.starts_with('/'))
            })
        {
            continue;
        }
        inventory.files = inventory
            .files
            .checked_add(1)
            .ok_or_else(|| ApiError::Admission("workspace_limit_exceeded".to_owned()))?;
        inventory.bytes = inventory
            .bytes
            .checked_add(
                entry
                    .size
                    .ok_or_else(|| ApiError::Admission("workspace_git_tree_invalid".to_owned()))?,
            )
            .ok_or_else(|| ApiError::Admission("workspace_limit_exceeded".to_owned()))?;
    }
    Ok(inventory)
}

#[derive(Clone)]
struct PendingWorkspaceSubmodules {
    repository: GitRepositoryIdentity,
    commit: steward_types::direct_package::ExactGitCommit,
    prefix: String,
    depth: usize,
    paths: Vec<WorkspacePath>,
    discover_submodules: bool,
}

async fn resolve_workspace_submodules<L>(
    ledger: &L,
    config: &TaskApiConfig,
    git: &dyn DirectGitResolver,
    repository: &GitRepositoryIdentity,
    commit: &steward_types::direct_package::ExactGitCommit,
    paths: &[WorkspacePath],
    required: &[RelativePath],
    recursive: bool,
    max_depth: usize,
    caller: Option<&TriggerRepository>,
    browser: bool,
) -> Result<WorkspaceSubmoduleResolution, ApiError>
where
    L: TaskSubmissionLedger,
{
    let mut pending = vec![PendingWorkspaceSubmodules {
        repository: repository.clone(),
        commit: commit.clone(),
        prefix: String::new(),
        depth: 0,
        paths: paths.to_vec(),
        discover_submodules: true,
    }];
    let mut resolved = Vec::new();
    let mut inventory = WorkspaceTreeInventory::default();
    while let Some(parent) = pending.pop() {
        let tree = read_workspace_tree(git, &parent.repository, &parent.commit).await?;
        inventory.add(workspace_tree_inventory(&tree, &parent.paths)?)?;
        if !parent.discover_submodules {
            continue;
        }
        let gitmodules_path = RelativePath::parse(".gitmodules").map_err(ApiError::Admission)?;
        let has_gitmodules = tree.entries.iter().any(|entry| {
            entry.path == gitmodules_path
                && entry.kind == "blob"
                && matches!(entry.mode.as_str(), "100644" | "100755")
        });
        let modules = if has_gitmodules {
            let file = git
                .read_file(&GitFileRequest {
                    repository: parent.repository.clone(),
                    commit: parent.commit.clone(),
                    path: gitmodules_path,
                    max_bytes: MAX_GITMODULES_BYTES,
                })
                .await
                .map_err(source_port_error)?;
            if file.repository != parent.repository || file.commit != parent.commit {
                return Err(ApiError::Admission(
                    "workspace_git_submodule_manifest_identity_mismatch".to_owned(),
                ));
            }
            parse_gitmodules(&file.bytes)?
        } else {
            Vec::new()
        };
        let gitlinks = tree
            .entries
            .iter()
            .filter(|entry| entry.kind == "commit" && entry.mode == "160000")
            .map(|entry| (entry.path.as_str(), entry.object.as_str()))
            .collect::<BTreeMap<_, _>>();
        for (module_path, module_url) in modules {
            let full_path = prefixed_workspace_path(&parent.prefix, module_path.as_str())?;
            if !workspace_submodule_is_selected(&parent.paths, &module_path) {
                resolved.push(skipped_workspace_submodule(full_path, "path_not_selected")?);
                continue;
            }
            let Some(object) = gitlinks.get(module_path.as_str()) else {
                resolved.push(skipped_workspace_submodule(full_path, "gitlink_missing")?);
                continue;
            };
            let child_commit =
                steward_types::direct_package::ExactGitCommit::parse(format!("git:sha1:{object}"))
                    .map_err(|_| {
                        ApiError::Admission("workspace_git_submodule_gitlink_invalid".to_owned())
                    })?;
            let child_url = match workspace_submodule_repository_url(
                &parent.repository.repository,
                &module_url,
            ) {
                Ok(value) => value,
                Err(reason) => {
                    resolved.push(skipped_workspace_submodule(full_path, reason)?);
                    continue;
                }
            };
            let child_repository = match git.resolve_repository(&child_url).await {
                Ok(value) => value,
                Err(_) => {
                    resolved.push(skipped_workspace_submodule(
                        full_path,
                        "repository_unavailable",
                    )?);
                    continue;
                }
            };
            if !workspace_repository_is_authorized(
                ledger,
                config,
                caller,
                browser,
                &child_repository,
            )
            .await?
            {
                resolved.push(skipped_workspace_submodule(
                    full_path,
                    "repository_not_admitted",
                )?);
                continue;
            }
            resolved.push(ResolvedWorkspaceSubmodule {
                path: full_path.clone(),
                status: WorkspaceSubmoduleStatus::Materialized,
                repository: Some(child_repository.repository.clone()),
                repository_id: Some(child_repository.repository_id.clone()),
                repository_owner_id: Some(child_repository.repository_owner_id.clone()),
                commit: Some(child_commit.clone()),
                reason: None,
            });
            if recursive {
                if parent.depth >= max_depth {
                    return Err(ApiError::Admission(
                        "workspace_git_submodule_depth_exceeded".to_owned(),
                    ));
                }
                pending.push(PendingWorkspaceSubmodules {
                    repository: child_repository,
                    commit: child_commit,
                    prefix: full_path.as_str().to_owned(),
                    depth: parent.depth + 1,
                    paths: Vec::new(),
                    discover_submodules: true,
                });
            } else {
                pending.push(PendingWorkspaceSubmodules {
                    repository: child_repository,
                    commit: child_commit,
                    prefix: full_path.as_str().to_owned(),
                    depth: parent.depth + 1,
                    paths: Vec::new(),
                    discover_submodules: false,
                });
            }
        }
    }
    resolved.sort_by(|left, right| left.path.as_str().cmp(right.path.as_str()));
    let materialized = resolved
        .iter()
        .filter(|entry| entry.status == WorkspaceSubmoduleStatus::Materialized)
        .map(|entry| entry.path.as_str())
        .collect::<BTreeSet<_>>();
    if required
        .iter()
        .any(|required| !materialized.contains(required.as_str()))
    {
        return Err(ApiError::Admission(
            "workspace_git_required_submodule_unavailable".to_owned(),
        ));
    }
    Ok(WorkspaceSubmoduleResolution {
        entries: resolved,
        inventory,
    })
}

fn workspace_submodule_is_selected(paths: &[WorkspacePath], module_path: &RelativePath) -> bool {
    paths.is_empty()
        || paths.iter().any(|path| {
            let selected = path.as_str().strip_suffix('/').unwrap_or(path.as_str());
            module_path.as_str() == selected
                || module_path
                    .as_str()
                    .strip_prefix(selected)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        })
}

fn parse_gitmodules(bytes: &[u8]) -> Result<Vec<(RelativePath, String)>, ApiError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| ApiError::Admission("workspace_git_submodule_manifest_invalid".to_owned()))?;
    let mut modules = Vec::new();
    let mut path = None;
    let mut url = None;
    let mut in_section = false;
    let flush = |modules: &mut Vec<(RelativePath, String)>,
                 path: &mut Option<RelativePath>,
                 url: &mut Option<String>,
                 in_section: bool|
     -> Result<(), ApiError> {
        if !in_section {
            return Ok(());
        }
        let path = path.take().ok_or_else(|| {
            ApiError::Admission("workspace_git_submodule_manifest_invalid".to_owned())
        })?;
        let url = url.take().ok_or_else(|| {
            ApiError::Admission("workspace_git_submodule_manifest_invalid".to_owned())
        })?;
        modules.push((path, url));
        Ok(())
    };
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if line.starts_with("[submodule \"") && line.ends_with("\"]") {
            flush(&mut modules, &mut path, &mut url, in_section)?;
            in_section = true;
            continue;
        }
        if !in_section {
            return Err(ApiError::Admission(
                "workspace_git_submodule_manifest_invalid".to_owned(),
            ));
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(ApiError::Admission(
                "workspace_git_submodule_manifest_invalid".to_owned(),
            ));
        };
        match key.trim() {
            "path" if path.is_none() => {
                path = Some(RelativePath::parse(value.trim().to_owned()).map_err(|_| {
                    ApiError::Admission("workspace_git_submodule_manifest_invalid".to_owned())
                })?);
            }
            "url" if url.is_none() && !value.trim().is_empty() => {
                url = Some(value.trim().to_owned());
            }
            "branch" | "update" | "ignore" | "shallow" => {}
            _ => {
                return Err(ApiError::Admission(
                    "workspace_git_submodule_manifest_invalid".to_owned(),
                ));
            }
        }
    }
    flush(&mut modules, &mut path, &mut url, in_section)?;
    let mut paths = BTreeSet::new();
    if modules
        .iter()
        .any(|(path, _)| !paths.insert(path.as_str().to_owned()))
    {
        return Err(ApiError::Admission(
            "workspace_git_submodule_manifest_invalid".to_owned(),
        ));
    }
    Ok(modules)
}

fn prefixed_workspace_path(prefix: &str, path: &str) -> Result<RelativePath, ApiError> {
    let value = if prefix.is_empty() {
        path.to_owned()
    } else {
        format!("{prefix}/{path}")
    };
    RelativePath::parse(value)
        .map_err(|_| ApiError::Admission("workspace_git_submodule_path_invalid".to_owned()))
}

fn skipped_workspace_submodule(
    path: RelativePath,
    reason: &str,
) -> Result<ResolvedWorkspaceSubmodule, ApiError> {
    Ok(ResolvedWorkspaceSubmodule {
        path,
        status: WorkspaceSubmoduleStatus::Skipped,
        repository: None,
        repository_id: None,
        repository_owner_id: None,
        commit: None,
        reason: Some(BoundedText::parse(reason.to_owned()).map_err(ApiError::Admission)?),
    })
}

fn workspace_submodule_repository_url(
    parent: &RepositoryUrl,
    value: &str,
) -> Result<RepositoryUrl, &'static str> {
    let parent = parent
        .as_str()
        .strip_prefix("https://")
        .and_then(|value| value.strip_suffix(".git"))
        .ok_or("repository_url_invalid")?;
    let (host, parent_path) = parent.split_once('/').ok_or("repository_url_invalid")?;
    let candidate = if let Some(rest) = value.strip_prefix("https://") {
        let (candidate_host, _) = rest.split_once('/').ok_or("url_not_supported")?;
        if candidate_host != host {
            return Err("foreign_host");
        }
        value.to_owned()
    } else if let Some(rest) = value.strip_prefix("git@") {
        let (candidate_host, path) = rest.split_once(':').ok_or("url_not_supported")?;
        if candidate_host != host {
            return Err("foreign_host");
        }
        format!("https://{host}/{path}")
    } else if value.starts_with("./") || value.starts_with("../") {
        let mut components = parent_path
            .split('/')
            .map(str::to_owned)
            .collect::<Vec<_>>();
        components.pop();
        for component in value.split('/') {
            match component {
                "." | "" => {}
                ".." => {
                    components.pop().ok_or("url_not_supported")?;
                }
                component => components.push(component.to_owned()),
            }
        }
        format!("https://{host}/{}", components.join("/"))
    } else {
        return Err("url_not_supported");
    };
    RepositoryUrl::parse(candidate).map_err(|_| "url_not_supported")
}

async fn describe_workspace_repository(
    git: &dyn DirectGitResolver,
    reference: &GitRepositoryReference,
) -> Result<(GitRepositoryIdentity, GitRepositoryDescription), ApiError> {
    let references = [reference.clone()];
    let mut descriptions = git.describe_repositories(&references);
    let (index, description) = descriptions
        .next()
        .await
        .ok_or_else(|| ApiError::Admission("workspace_git_repository_unresolved".to_owned()))?;
    if index != 0 || descriptions.next().await.is_some() {
        return Err(ApiError::Admission(
            "workspace_git_repository_unresolved".to_owned(),
        ));
    }
    let description = description.map_err(source_port_error)?;
    if description.repository_owner_id != reference.repository_owner_id
        || description.repository_id != reference.repository_id
    {
        return Err(ApiError::Admission(
            "workspace_git_repository_identity_mismatch".to_owned(),
        ));
    }
    let repository = RepositoryUrl::parse(format!("{}.git", description.web_url))
        .map_err(|_| ApiError::Admission("workspace_git_repository_unresolved".to_owned()))?;
    Ok((
        GitRepositoryIdentity {
            repository,
            repository_id: description.repository_id.clone(),
            repository_owner_id: description.repository_owner_id.clone(),
        },
        description,
    ))
}

async fn workspace_repository_is_authorized<L>(
    ledger: &L,
    config: &TaskApiConfig,
    caller: Option<&TriggerRepository>,
    browser: bool,
    repository: &GitRepositoryIdentity,
) -> Result<bool, ApiError>
where
    L: TaskSubmissionLedger,
{
    if browser {
        return Ok(config.browser_source_repository_is_authorized(repository));
    }
    let caller = caller.ok_or(ApiError::TaskAuthentication)?;
    Ok(config.source_repository_is_authorized(caller, repository)
        || ledger
            .active_source_repository_binding(caller, repository)
            .await
            .map_err(ApiError::Store)?)
}

fn workspace_entry_digest(value: &impl Serialize) -> Result<ContentDigest, ApiError> {
    let bytes = canonical_json_bytes(value).map_err(ApiError::Admission)?;
    let digest = Sha256::digest(bytes);
    ContentDigest::parse(format!("steward:sha256:{digest:x}")).map_err(ApiError::Admission)
}

async fn resolve_browser_package_pre_admission<L>(
    ledger: &L,
    config: &TaskApiConfig,
    identity: &TaskIdentity,
    request: &BrowserTaskSubmission,
) -> Result<BrowserTaskPreAdmission, ApiError>
where
    L: TaskSubmissionLedger,
{
    let (
        definition,
        prompt,
        closure,
        closure_digest,
        revision,
        inline_files,
        workspace_self_repository,
    ) = if request.package.source == "inline" {
        let files = request
            .package
            .files
            .as_ref()
            .ok_or_else(|| ApiError::Admission("inline packages require files".to_owned()))?;
        let definition_bytes = files
            .get(request.package.path.as_str())
            .ok_or_else(|| ApiError::Admission("inline package entry point is missing".to_owned()))?
            .as_bytes();
        let definition = serde_json::from_slice::<DirectTaskDefinition>(definition_bytes)
            .map_err(|_| ApiError::Admission("direct TaskDefinition is invalid".to_owned()))?;
        definition.validate().map_err(ApiError::Admission)?;
        let (prompt, closure, closure_digest) = resolve_inline_package_closure(
            &request.package.path,
            &definition,
            definition_bytes,
            files,
        )?;
        if let Some(expected) = request.package.revision.as_deref()
            && expected != closure_digest.as_str()
        {
            return Err(ApiError::Admission(
                "inline package revision does not match its computed digest".to_owned(),
            ));
        }
        (
            definition,
            prompt,
            closure,
            closure_digest.clone(),
            closure_digest.as_str().to_owned(),
            Some(files.clone()),
            None,
        )
    } else {
        let git = config.direct_git_resolver.as_ref().ok_or_else(|| {
            ApiError::TaskRuntimeContractUnavailable(
                "direct package Git source resolver is unavailable".to_owned(),
            )
        })?;
        let repository =
            RepositoryUrl::parse(request.package.source.clone()).map_err(ApiError::Admission)?;
        let repository = git
            .resolve_repository(&repository)
            .await
            .map_err(|_| ApiError::BrowserTaskSourceUnauthorized)?;
        if !config.browser_source_repository_is_authorized(&repository) {
            return Err(ApiError::BrowserTaskSourceUnauthorized);
        }
        let requested_revision = request.package.revision.as_deref().ok_or_else(|| {
            ApiError::Admission("repository package revision is required".to_owned())
        })?;
        let commit = if requested_revision.starts_with("git:sha1:") {
            steward_types::direct_package::ExactGitCommit::parse(requested_revision.to_owned())
                .map_err(ApiError::Admission)?
        } else {
            git.resolve_revision(&GitRevisionRequest {
                repository: repository.clone(),
                reference: requested_revision.to_owned(),
            })
            .await
            .map_err(source_port_error)?
        };
        let definition_request = GitFileRequest {
            repository: repository.clone(),
            commit: commit.clone(),
            path: request.package.path.clone(),
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
            &repository,
            &commit,
            &request.package.path,
            &definition,
            &definition_bytes,
        )
        .await?;
        (
            definition,
            prompt,
            closure,
            closure_digest,
            commit.as_str().to_owned(),
            None,
            Some(repository),
        )
    };

    let envelope = resolve_direct_user_envelope(
        ledger,
        &identity.canonical_user_id,
        request.envelope_digest.as_ref(),
    )
    .await?;
    if request.package.source == "inline"
        && let (Some(template_id), Some(template_revision)) =
            (envelope.template_id.as_deref(), envelope.template_revision)
    {
        match ledger
            .envelope_template_allows_inline_browser_tasks(template_id, template_revision)
            .await
            .map_err(ApiError::Store)?
        {
            Some(true) => {}
            Some(false) => {
                return Err(ApiError::Admission(
                    "inline browser Tasks are disabled for the selected Envelope template"
                        .to_owned(),
                ));
            }
            None => {
                return Err(ApiError::Admission(
                    "the selected Envelope template revision is unavailable".to_owned(),
                ));
            }
        }
    }
    let approved = envelope
        .approved_envelope
        .as_ref()
        .ok_or(ApiError::MissingEnvelope)?;
    let mut effective_requirements = match &definition.requires {
        Some(requirements) => requirements.clone(),
        None => direct_requirements_from_envelope(&approved.spec)?,
    };
    let selected_model = match definition.runtime.model.as_ref() {
        Some(model) => model.clone(),
        None => match effective_requirements.authority.llms.as_slice() {
            [model] => model.clone(),
            _ => {
                return Err(ApiError::Admission(
                    "direct TaskDefinition must select a runtime model when multiple models are available"
                        .to_owned(),
                ));
            }
        },
    };
    if definition.requires.is_none()
        && !approved.spec.llms.iter().any(|model| {
            model.provider == selected_model.provider.as_str()
                && model.model == selected_model.model.as_str()
        })
    {
        return Err(ApiError::Admission(
            "selected model is not allowed by the selected Envelope".to_owned(),
        ));
    }
    let requested_spec = direct_runtime_spec(identity, &definition, &effective_requirements)?;
    if !matches!(
        evaluate_with_grants(&requested_spec, approved, &[])
            .map_err(|error| ApiError::Admission(format!("{error:?}")))?,
        AdmissionDecision::Admit
    ) {
        return Err(ApiError::Admission(
            "browser package requirements exceed the selected Envelope".to_owned(),
        ));
    }
    effective_requirements.authority.llms = vec![selected_model.clone()];
    let spec = direct_runtime_spec(identity, &definition, &effective_requirements)?;
    let model = ModelRef {
        provider: selected_model.provider.as_str().to_owned(),
        model: selected_model.model.as_str().to_owned(),
    };
    let (command, execution_binding) =
        resolve_direct_execution_plan(config, &definition, &prompt, &spec, &model)?;
    let workspace = resolve_workspace_evidence(
        ledger,
        config,
        &definition.workspace,
        WorkspaceResolutionContext {
            self_repository: workspace_self_repository.as_ref(),
            trigger_commit: None,
            caller: None,
            browser: true,
        },
    )
    .await?;
    let evidence = BrowserTaskEvidence {
        source: request.package.source.clone(),
        revision,
        path: request.package.path.clone(),
        closure: Some(closure),
        closure_digest,
        inline_files,
        diagnostics: request.diagnostics,
        workspace,
        prompt_source: PromptSourceKind::for_definition(&definition),
    };
    evidence.validate().map_err(ApiError::Admission)?;
    Ok(BrowserTaskPreAdmission {
        definition,
        evidence,
        envelope,
        spec,
        command,
        execution_binding,
    })
}

pub(crate) fn resolve_inline_package_closure(
    entry_point: &RelativePath,
    definition: &DirectTaskDefinition,
    definition_bytes: &[u8],
    files: &BTreeMap<String, String>,
) -> Result<(String, PackageClosure, ContentDigest), ApiError> {
    if definition_bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        return Err(ApiError::Admission(
            "direct TaskDefinition must not contain a UTF-8 BOM".to_owned(),
        ));
    }
    let package_root = containing_directory(entry_point.as_str());
    let mut entries = BTreeMap::<String, ClosureEntry>::new();
    let canonical_definition = canonical_json_bytes(definition).map_err(ApiError::Admission)?;
    insert_closure_entry(
        &mut entries,
        ClosureEntryKind::TaskDefinition,
        entry_point.clone(),
        &canonical_definition,
    )?;
    let mut rendered_prompt = match (&definition.prompt, &definition.prompt_text) {
        (Some(prompt), None) => {
            let prompt_path = resolve_package_relative_path(
                package_root,
                containing_directory(entry_point.as_str()),
                prompt,
            )?;
            let prompt_bytes = inline_file(files, &prompt_path)?;
            insert_closure_entry(
                &mut entries,
                ClosureEntryKind::Prompt,
                prompt_path,
                prompt_bytes,
            )?;
            std::str::from_utf8(prompt_bytes)
                .map_err(|_| ApiError::Admission("direct package prompt must be UTF-8".to_owned()))?
                .to_owned()
        }
        (None, Some(prompt)) => prompt.clone(),
        _ => {
            return Err(ApiError::Admission(
                "TaskDefinition requires exactly one of prompt or promptText".to_owned(),
            ));
        }
    };
    for skill_reference in &definition.skills {
        let skill_path = resolve_package_relative_path(
            package_root,
            containing_directory(entry_point.as_str()),
            skill_reference,
        )?;
        let skill_bytes = inline_file(files, &skill_path)?;
        let skill = serde_json::from_slice::<InstructionSkill>(skill_bytes)
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
        let instructions = inline_file(files, &instructions_path)?;
        insert_closure_entry(
            &mut entries,
            ClosureEntryKind::Instructions,
            instructions_path,
            instructions,
        )?;
        let instructions = std::str::from_utf8(instructions).map_err(|_| {
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
            let asset_bytes = inline_file(files, &asset_path)?;
            insert_closure_entry(
                &mut entries,
                ClosureEntryKind::Asset,
                asset_path,
                asset_bytes,
            )?;
        }
    }
    if entries.len() != files.len() {
        return Err(ApiError::Admission(
            "inline package contains unreferenced files".to_owned(),
        ));
    }
    let closure = PackageClosure {
        contract_version: PACKAGE_CLOSURE_CONTRACT_VERSION.to_owned(),
        entry_point: entry_point.clone(),
        entries: entries.into_values().collect(),
    };
    closure.validate().map_err(ApiError::Admission)?;
    let digest = Sha256::digest(canonical_json_bytes(&closure).map_err(ApiError::Admission)?);
    let digest =
        ContentDigest::parse(format!("steward:sha256:{digest:x}")).map_err(ApiError::Admission)?;
    Ok((rendered_prompt, closure, digest))
}

fn inline_file<'a>(
    files: &'a BTreeMap<String, String>,
    path: &RelativePath,
) -> Result<&'a [u8], ApiError> {
    files
        .get(path.as_str())
        .map(String::as_bytes)
        .ok_or_else(|| ApiError::Admission(format!("package file {} is missing", path.as_str())))
}

async fn prepare_task_input_archive(
    config: &TaskApiConfig,
    caller_archive: &[u8],
    workspace: Option<&WorkspaceEvidence>,
) -> Result<Vec<u8>, ApiError> {
    let Some(workspace) = workspace else {
        return Ok(caller_archive.to_vec());
    };
    let mut material = Vec::new();
    let manifest = canonical_json_bytes(workspace).map_err(ApiError::Admission)?;
    append_workspace_tar_file(&mut material, "manifest.json", &manifest)?;
    for (index, entry) in workspace.entries.iter().enumerate() {
        let ResolvedWorkspaceEntry::Git {
            repository,
            repository_id,
            repository_owner_id,
            commit,
            history,
            submodules,
            ..
        } = entry
        else {
            continue;
        };
        let git = config.direct_git_resolver.as_ref().ok_or_else(|| {
            ApiError::TaskRuntimeContractUnavailable(
                "workspace_git_source_resolver_unavailable".to_owned(),
            )
        })?;
        let depth = match history {
            WorkspaceGitHistory::None => Some(1),
            WorkspaceGitHistory::Depth(depth) => Some(*depth),
            WorkspaceGitHistory::Full => None,
        };
        append_workspace_git_pack(
            git.as_ref(),
            caller_archive.len(),
            &mut material,
            &format!("packs/{index}.pack"),
            &format!("shallow/{index}"),
            GitPackRequest {
                repository: GitRepositoryIdentity {
                    repository: repository.clone(),
                    repository_id: repository_id.clone(),
                    repository_owner_id: repository_owner_id.clone(),
                },
                commit: commit.clone(),
                depth,
                max_bytes: 1,
            },
        )
        .await?;
        for (submodule_index, submodule) in submodules.iter().enumerate() {
            if submodule.status != WorkspaceSubmoduleStatus::Materialized {
                continue;
            }
            append_workspace_git_pack(
                git.as_ref(),
                caller_archive.len(),
                &mut material,
                &format!("submodules/{index}/{submodule_index}.pack"),
                &format!("submodules/{index}/{submodule_index}.shallow"),
                GitPackRequest {
                    repository: GitRepositoryIdentity {
                        repository: submodule.repository.clone().ok_or_else(|| {
                            ApiError::Admission(
                                "workspace submodule evidence is invalid".to_owned(),
                            )
                        })?,
                        repository_id: submodule.repository_id.clone().ok_or_else(|| {
                            ApiError::Admission(
                                "workspace submodule evidence is invalid".to_owned(),
                            )
                        })?,
                        repository_owner_id: submodule.repository_owner_id.clone().ok_or_else(
                            || {
                                ApiError::Admission(
                                    "workspace submodule evidence is invalid".to_owned(),
                                )
                            },
                        )?,
                    },
                    commit: submodule.commit.clone().ok_or_else(|| {
                        ApiError::Admission("workspace submodule evidence is invalid".to_owned())
                    })?,
                    depth: Some(1),
                    max_bytes: 1,
                },
            )
            .await?;
        }
    }
    material.extend_from_slice(&[0_u8; 1024]);
    let framed = frame_task_input_archive(caller_archive, &material)
        .map_err(|_| ApiError::Admission("workspace_limit_exceeded".to_owned()))?;
    if framed.len() > MAX_TASK_INPUT_ARCHIVE_BYTES {
        return Err(ApiError::Admission("workspace_limit_exceeded".to_owned()));
    }
    Ok(framed)
}

async fn append_workspace_git_pack(
    git: &dyn DirectGitResolver,
    caller_archive_len: usize,
    material: &mut Vec<u8>,
    pack_path: &str,
    shallow_path: &str,
    mut request: GitPackRequest,
) -> Result<(), ApiError> {
    let reserved = caller_archive_len
        .checked_add(material.len())
        .and_then(|value| value.checked_add(4096))
        .ok_or_else(|| ApiError::Admission("workspace_limit_exceeded".to_owned()))?;
    let remaining = MAX_TASK_INPUT_ARCHIVE_BYTES
        .checked_sub(reserved)
        .filter(|value| *value > 0)
        .ok_or_else(|| ApiError::Admission("workspace_limit_exceeded".to_owned()))?;
    request.max_bytes = remaining as u64;
    let pack = git.read_pack(&request).await.map_err(source_port_error)?;
    if pack.repository != request.repository || pack.commit != request.commit {
        return Err(ApiError::Admission(
            "workspace_git_pack_identity_mismatch".to_owned(),
        ));
    }
    if pack.bytes.len() as u64 > request.max_bytes {
        return Err(ApiError::Admission("workspace_limit_exceeded".to_owned()));
    }
    append_workspace_tar_file(material, pack_path, &pack.bytes)?;
    if !pack.shallow.is_empty() {
        let shallow = pack
            .shallow
            .iter()
            .map(|commit| {
                commit
                    .as_str()
                    .strip_prefix("git:sha1:")
                    .unwrap_or(commit.as_str())
            })
            .collect::<Vec<_>>()
            .join("\n");
        append_workspace_tar_file(material, shallow_path, format!("{shallow}\n").as_bytes())?;
    }
    Ok(())
}

fn append_workspace_tar_file(
    archive: &mut Vec<u8>,
    name: &str,
    contents: &[u8],
) -> Result<(), ApiError> {
    if name.is_empty() || name.len() > 100 || name.starts_with('/') || name.contains("..") {
        return Err(ApiError::Admission(
            "workspace material path is invalid".to_owned(),
        ));
    }
    let mut header = vec![0_u8; 512];
    header[..name.len()].copy_from_slice(name.as_bytes());
    write_tar_octal(&mut header[100..108], 0o600)?;
    write_tar_octal(&mut header[108..116], 0)?;
    write_tar_octal(&mut header[116..124], 0)?;
    write_tar_octal(&mut header[124..136], contents.len() as u64)?;
    write_tar_octal(&mut header[136..148], 0)?;
    header[148..156].fill(b' ');
    header[156] = b'0';
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    let checksum: u64 = header.iter().map(|byte| u64::from(*byte)).sum();
    write_tar_checksum(&mut header[148..156], checksum)?;
    archive.extend_from_slice(&header);
    archive.extend_from_slice(contents);
    let padding = (512 - contents.len() % 512) % 512;
    archive.resize(archive.len() + padding, 0);
    Ok(())
}

fn browser_inputs_archive(inputs: &serde_json::Value) -> Result<Vec<u8>, ApiError> {
    let bytes = canonical_json_bytes(inputs).map_err(ApiError::Admission)?;
    let mut archive = vec![0_u8; 512];
    let name = b"in/inputs.json";
    archive[..name.len()].copy_from_slice(name);
    write_tar_octal(&mut archive[100..108], 0o644)?;
    write_tar_octal(&mut archive[108..116], 0)?;
    write_tar_octal(&mut archive[116..124], 0)?;
    write_tar_octal(&mut archive[124..136], bytes.len() as u64)?;
    write_tar_octal(&mut archive[136..148], 0)?;
    archive[148..156].fill(b' ');
    archive[156] = b'0';
    archive[257..263].copy_from_slice(b"ustar\0");
    archive[263..265].copy_from_slice(b"00");
    let checksum: u64 = archive.iter().map(|byte| u64::from(*byte)).sum();
    write_tar_checksum(&mut archive[148..156], checksum)?;
    archive.extend_from_slice(&bytes);
    let padding = (512 - bytes.len() % 512) % 512;
    archive.resize(archive.len() + padding + 1024, 0);
    Ok(archive)
}

fn browser_inputs_from_archive(archive: &[u8]) -> Result<serde_json::Value, ApiError> {
    let archive = split_task_input_archive(archive)
        .map_err(|_| ApiError::Admission("browser Task input archive is invalid".to_owned()))?
        .caller_archive;
    let header = archive
        .get(..512)
        .ok_or_else(|| ApiError::Admission("browser Task input archive is invalid".to_owned()))?;
    let name_end = header[..100]
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(100);
    if &header[..name_end] != b"in/inputs.json" {
        return Err(ApiError::Admission(
            "browser Task input archive entry is invalid".to_owned(),
        ));
    }
    let size = std::str::from_utf8(&header[124..136])
        .ok()
        .map(|value| value.trim_matches(['\0', ' ']))
        .filter(|value| !value.is_empty())
        .and_then(|value| usize::from_str_radix(value, 8).ok())
        .ok_or_else(|| ApiError::Admission("browser Task input size is invalid".to_owned()))?;
    let bytes = archive
        .get(512..512_usize.saturating_add(size))
        .ok_or_else(|| ApiError::Admission("browser Task input archive is truncated".to_owned()))?;
    let inputs: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|_| ApiError::Admission("browser Task inputs are invalid".to_owned()))?;
    if !inputs.is_object() {
        return Err(ApiError::Admission(
            "browser Task inputs must be a JSON object".to_owned(),
        ));
    }
    Ok(inputs)
}

fn write_tar_octal(field: &mut [u8], value: u64) -> Result<(), ApiError> {
    let width = field.len().saturating_sub(1);
    let encoded = format!("{value:0width$o}");
    if encoded.len() != width {
        return Err(ApiError::Admission(
            "Task inputs cannot be archived".to_owned(),
        ));
    }
    field[..width].copy_from_slice(encoded.as_bytes());
    field[width] = 0;
    Ok(())
}

fn write_tar_checksum(field: &mut [u8], value: u64) -> Result<(), ApiError> {
    let encoded = format!("{value:06o}\0 ");
    if encoded.len() != field.len() {
        return Err(ApiError::Admission(
            "Task inputs cannot be archived".to_owned(),
        ));
    }
    field.copy_from_slice(encoded.as_bytes());
    Ok(())
}

fn browser_submission_response(
    record: TaskRecord,
) -> Result<BrowserRunSubmissionResponse, ApiError> {
    let evidence = record
        .browser_task_evidence
        .ok_or(ApiError::Store(StoreError::InvalidTaskTransition))?;
    let instance_id = record
        .user_envelope_instance_id
        .ok_or(ApiError::MissingEnvelope)?;
    let revision = record
        .user_envelope_revision
        .ok_or(ApiError::MissingEnvelope)?;
    let digest = record
        .user_envelope_digest
        .ok_or(ApiError::MissingEnvelope)?;
    Ok(BrowserRunSubmissionResponse {
        task_uid: record.task_uid,
        phase: record.phase,
        origin: TaskOrigin::Browser,
        package: BrowserResolvedPackage {
            source: evidence.source,
            revision: evidence.revision,
            path: evidence.path,
        },
        envelope: BrowserResolvedEnvelope {
            instance_id,
            revision,
            digest: format!("steward:{digest}"),
        },
    })
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
    let (manifest, invocation_kind, invocation_path) =
        match (&request.invocation_path, &request.package_path) {
            (Some(invocation_path), None) => {
                let invocation_request = GitFileRequest {
                    repository: invocation_identity.clone(),
                    commit: provenance.triggered_sha.clone(),
                    path: invocation_path.clone(),
                    max_bytes: steward_types::direct_package::MAX_PACKAGE_FILE_BYTES,
                };
                let invocation_file = git
                    .read_file(&invocation_request)
                    .await
                    .map_err(source_port_error)?;
                let invocation_bytes = verified_git_file(invocation_file, &invocation_request)?;
                let manifest = serde_json::from_slice::<InvocationManifest>(&invocation_bytes)
                    .map_err(|_| {
                        ApiError::Admission("invocation manifest is invalid".to_owned())
                    })?;
                (manifest, InvocationKind::Manifest, invocation_path.clone())
            }
            (None, Some(package_path)) => (
                InvocationManifest {
                    contract_version: steward_types::direct_package::DIRECT_TASK_CONTRACT_VERSION
                        .to_owned(),
                    package: steward_types::direct_package::PackageReference {
                        repository: invocation_repository.clone(),
                        commit: PackageCommit::Trigger,
                        path: package_path.clone(),
                    },
                    envelope: None,
                    diagnostics: (!request.diagnostics.is_off()).then_some(request.diagnostics),
                },
                InvocationKind::Implicit,
                package_path.clone(),
            ),
            _ => {
                return Err(ApiError::InvalidRequest(
                    "exactly one of invocationPath or packagePath is required".to_owned(),
                ));
            }
        };
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
        &provenance.triggered_sha,
        &invocation_path,
        &canonical_json_bytes(&manifest).map_err(ApiError::Admission)?,
    )?;
    let package = resolved_source(
        &package_identity,
        &definition_request.commit,
        &definition_request.path,
        &canonical_json_bytes(&definition).map_err(ApiError::Admission)?,
    )?;

    let envelope_selector = match (&request.envelope_digest, &manifest.envelope) {
        (Some(request_digest), Some(manifest_digest)) if request_digest != manifest_digest => {
            return Err(ApiError::Admission(
                "Task submission and invocation manifest select different Envelopes".to_owned(),
            ));
        }
        (Some(digest), _) | (None, Some(digest)) => Some(digest),
        (None, None) => None,
    };
    let explicit_envelope = envelope_selector.is_some();
    let envelope =
        resolve_direct_user_envelope(ledger, &identity.canonical_user_id, envelope_selector)
            .await?;
    let approved = envelope.approved_envelope.as_ref().ok_or_else(|| {
        ApiError::Admission(if explicit_envelope {
            "the selected Envelope is not active".to_owned()
        } else {
            "the resolved Envelope is not active".to_owned()
        })
    })?;
    let mut effective_requirements = match &definition.requires {
        Some(requirements) => requirements.clone(),
        None => direct_requirements_from_envelope(&approved.spec)?,
    };
    let selected_model = match definition.runtime.model.as_ref() {
        Some(model) => model.clone(),
        None => match effective_requirements.authority.llms.as_slice() {
            [model] => model.clone(),
            _ => {
                return Err(ApiError::Admission(
                    "direct TaskDefinition must select a runtime model when multiple models are available"
                        .to_owned(),
                ));
            }
        },
    };
    if definition.requires.is_none()
        && !approved.spec.llms.iter().any(|model| {
            model.provider == selected_model.provider.as_str()
                && model.model == selected_model.model.as_str()
        })
    {
        return Err(ApiError::Admission(if explicit_envelope {
            "selected model is not allowed by the selected Envelope".to_owned()
        } else {
            "selected model is not allowed by the resolved Envelope".to_owned()
        }));
    }
    let requested_spec = direct_runtime_spec(identity, &definition, &effective_requirements)?;
    if !matches!(
        evaluate_with_grants(&requested_spec, approved, &[])
            .map_err(|error| ApiError::Admission(format!("{error:?}")))?,
        AdmissionDecision::Admit
    ) {
        return Err(ApiError::Admission(if explicit_envelope {
            "direct package requirements exceed the selected Envelope".to_owned()
        } else {
            "direct package requirements exceed the resolved Envelope".to_owned()
        }));
    }
    // A package can declare a wider required capability set, but a single
    // execution receives only its selected model. The source closure still
    // records the exact declaration that passed admission above.
    effective_requirements.authority.llms = vec![selected_model.clone()];
    let spec = direct_runtime_spec(identity, &definition, &effective_requirements)?;
    let model = steward_types::ModelRef {
        provider: selected_model.provider.as_str().to_owned(),
        model: selected_model.model.as_str().to_owned(),
    };
    let (command, execution_binding) =
        resolve_direct_execution_plan(config, &definition, &prompt, &spec, &model)?;
    let prompt_source = PromptSourceKind::for_definition(&definition);
    let workspace = resolve_workspace_evidence(
        ledger,
        config,
        &definition.workspace,
        WorkspaceResolutionContext {
            self_repository: Some(&invocation_identity),
            trigger_commit: Some(&provenance.triggered_sha),
            caller: Some(&provenance.repository),
            browser: false,
        },
    )
    .await?;
    Ok(DirectTaskPreAdmission {
        definition,
        invocation_kind,
        invocation,
        package,
        closure,
        closure_digest,
        diagnostics: manifest.effective_diagnostics(),
        prompt_source,
        envelope,
        effective_requirements,
        spec,
        command,
        execution_binding,
        workspace,
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
        invocation_kind: pre_admission.invocation_kind,
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
        workspace: pre_admission.workspace.clone(),
        prompt_source: pre_admission.prompt_source,
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

fn validate_browser_task_retry(
    identity: &TaskIdentity,
    evidence: &BrowserTaskEvidence,
    envelope_digest: &str,
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
        || record.task_origin != TaskOrigin::Browser
        || record.user_envelope_digest.as_deref() != Some(envelope_digest)
        || record.browser_task_evidence.as_ref() != Some(evidence)
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

    let mut rendered_prompt = match (&definition.prompt, &definition.prompt_text) {
        (Some(prompt), None) => {
            let prompt_path = resolve_package_relative_path(
                package_root,
                containing_directory(entry_point.as_str()),
                prompt,
            )?;
            let prompt_bytes = read_package_file(git, repository, commit, &prompt_path).await?;
            insert_closure_entry(
                &mut entries,
                ClosureEntryKind::Prompt,
                prompt_path,
                &prompt_bytes,
            )?;
            std::str::from_utf8(&prompt_bytes)
                .map_err(|_| ApiError::Admission("direct package prompt must be UTF-8".to_owned()))?
                .to_owned()
        }
        (None, Some(prompt)) => prompt.clone(),
        _ => {
            return Err(ApiError::Admission(
                "TaskDefinition requires exactly one of prompt or promptText".to_owned(),
            ));
        }
    };
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
    model: &steward_types::ModelRef,
) -> Result<(Vec<String>, TaskExecutionBinding), ApiError> {
    let agent_ref = definition.runtime.agent_ref.as_str();
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
    authority
        .as_object_mut()
        .ok_or_else(|| ApiError::Admission("Envelope authority is invalid".to_owned()))?
        .remove("runtimeMinutesLimit");
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

async fn require_managed_inference_credential<L: TaskSubmissionLedger>(
    application: &TaskApplicationService<L>,
    identity: &TaskIdentity,
    spec: &AgentRuntimeSpec,
) -> Result<(), ApiError> {
    if application.config.inference_mode != InferenceMode::Managed || spec.llms.is_empty() {
        return Ok(());
    }
    if application
        .ledger
        .has_managed_inference_credential(&identity.canonical_user_id)
        .await
        .map_err(ApiError::Store)?
    {
        Ok(())
    } else {
        Err(ApiError::InferenceKeyMissing)
    }
}

async fn submit_versioned_task<L>(
    application: &TaskApplicationService<L>,
    idempotency_key: &str,
    identity: TaskIdentity,
    reference: WorkflowReference,
    request: &TaskSubmissionRequest,
    browser_task_evidence: Option<&BrowserTaskEvidence>,
) -> Result<(StatusCode, TaskStatusResponse), ApiError>
where
    L: AdmissionLedger + TaskSubmissionLedger,
{
    if request.agent_runtime_uid.is_some() {
        return Err(ApiError::Admission(
            "versioned Workflows use a server-owned runtime path".to_owned(),
        ));
    }
    if let Some(record) = application
        .ledger
        .task_by_idempotency(
            &identity.service,
            identity.canonical_user_id.as_str(),
            idempotency_key,
        )
        .await
        .map_err(ApiError::Store)?
    {
        return application
            .retry_existing_task(&identity, Some(&reference), request, record)
            .await;
    }
    let workflow = application
        .ledger
        .workflow_revision(&reference.name, reference.version)
        .await
        .map_err(ApiError::Store)?
        .ok_or(ApiError::TaskWorkflowNotFound)?;
    let envelope = resolve_direct_user_envelope(
        &application.ledger,
        &identity.canonical_user_id,
        request.envelope_digest.as_ref(),
    )
    .await?;
    let plan =
        resolve_versioned_task_plan(&identity, workflow, vec![envelope], &application.config)?;
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
    require_managed_inference_credential(application, &identity, &plan.spec).await?;
    let decision = AdmissionDecision::Admit;
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
        user_envelope,
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
            source_provenance: identity.source_provenance.as_ref(),
            direct_task_evidence: None,
            task_origin: if browser_task_evidence.is_some() {
                TaskOrigin::Browser
            } else {
                TaskOrigin::GithubActions
            },
            browser_task_evidence,
            user_envelope_snapshot: Some(user_envelope),
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
            let requested_envelope_digest = request
                .envelope_digest
                .as_ref()
                .and_then(|digest| digest.as_str().strip_prefix("steward:"));
            if request.agent_runtime_uid.is_some()
                || record.workflow != workflow_reference
                || record.workflow_name.as_deref() != Some(reference.name.as_str())
                || record.workflow_version != Some(reference.version)
                || record.runtime_ownership != RuntimeOwnership::Provisioned
                || requested_envelope_digest
                    .is_some_and(|digest| record.user_envelope_digest.as_deref() != Some(digest))
            {
                return Err(ApiError::Store(StoreError::TaskIdempotencyConflict));
            }
        }
        None => {
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
                || request
                    .coding_agent_runtime
                    .as_deref()
                    .is_some_and(|runtime| record.coding_agent_runtime != runtime)
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
            TaskAuthenticationError::UnknownUser => ApiError::TaskIdentityUnknownUser,
            TaskAuthenticationError::Unassociated { issuer, subject } => {
                ApiError::TaskIdentityUnassociated { issuer, subject }
            }
            TaskAuthenticationError::Disabled { issuer, subject } => {
                ApiError::TaskIdentityDisabled { issuer, subject }
            }
            TaskAuthenticationError::Unavailable => ApiError::TaskAuthenticationUnavailable,
        })
}

pub(crate) struct TaskOrchestrationReservation {
    pub(crate) candidate_digest: String,
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

pub(crate) fn stable_task_runtime_name(operation_id: Uuid) -> String {
    format!("task-{}", operation_id.simple())
}

#[cfg(test)]
mod workflow_request_tests {
    use std::sync::Arc;

    use super::{
        TaskApiConfig, TaskCreateRequest, TaskSubmissionRequest, browser_inputs_archive,
        browser_inputs_from_archive, browser_rerun_submission, prepare_task_input_archive,
        resolve_versioned_task_plan, stable_task_runtime_name, task_orchestration_reservation,
        validate_workspace_deployment_policy, versioned_workflow_reference,
        workspace_submodule_is_selected,
    };
    use crate::{ApiError, TaskIdentity};
    use steward_admission::{Envelope, EnvelopeSpec};
    use steward_ports::{
        PortError, TaskExecutionAdapter, TaskExecutionPlan, TaskExecutionPlanRequest,
    };
    use steward_store::{EnvelopeRequestRecord, EnvelopeRequestStatus, WorkflowRevisionRecord};
    use steward_types::direct_package::{WorkspaceEntry, WorkspaceEvidence};
    use steward_types::{
        Budget, CanonicalUserId, Duration, Email, ModelRef, RunnerRequirements, ToolGrant,
    };
    use uuid::Uuid;

    struct ExampleExecutionAdapter;

    #[test]
    fn workspace_policy_rejects_disabled_types_and_over_cap_history() -> Result<(), String> {
        let config = TaskApiConfig::default().with_workspace_policy_json(Some(
            &serde_json::json!({
                "enabledTypes": ["git"],
                "maxTotalSize": "2Gi",
                "scratchVolumeSize": "2Gi",
                "maxFiles": 100,
                "maxHistoryDepth": 20,
                "maxSubmoduleDepth": 2
            })
            .to_string(),
        ))?;
        let scratch: WorkspaceEntry = serde_json::from_value(serde_json::json!({
            "scratch": {"size": "2Gi"}
        }))
        .map_err(|error| error.to_string())?;
        assert!(matches!(
            validate_workspace_deployment_policy(&config.workspace_policy, &scratch),
            Err(ApiError::Admission(ref reason)) if reason == "workspace_entry_type_not_allowed"
        ));
        let git: WorkspaceEntry = serde_json::from_value(serde_json::json!({
            "git": {"history": {"depth": 21}}
        }))
        .map_err(|error| error.to_string())?;
        assert!(matches!(
            validate_workspace_deployment_policy(&config.workspace_policy, &git),
            Err(ApiError::Admission(ref reason)) if reason == "workspace_limit_exceeded"
        ));
        Ok(())
    }

    #[test]
    fn sparse_workspace_materializes_only_selected_submodules() -> Result<(), String> {
        let selected = [steward_types::direct_package::WorkspacePath::parse("src/")?];
        let inside = steward_types::direct_package::RelativePath::parse("src/vendor/proto")?;
        let outside = steward_types::direct_package::RelativePath::parse("vendor/other")?;

        assert!(workspace_submodule_is_selected(&selected, &inside));
        assert!(!workspace_submodule_is_selected(&selected, &outside));
        assert!(workspace_submodule_is_selected(&[], &outside));
        Ok(())
    }

    #[tokio::test]
    async fn scratch_only_workspace_does_not_require_a_git_resolver() -> Result<(), String> {
        let evidence: WorkspaceEvidence = serde_json::from_value(serde_json::json!({
            "entries": [{
                "type": "scratch",
                "name": "scratch",
                "size": "2Gi",
                "contentDigest": format!("steward:sha256:{}", "a".repeat(64))
            }],
            "workspaceDigest": format!("steward:sha256:{}", "b".repeat(64))
        }))
        .map_err(|error| error.to_string())?;

        let archive =
            prepare_task_input_archive(&TaskApiConfig::default(), b"caller", Some(&evidence))
                .await
                .map_err(|error| format!("prepare scratch-only workspace: {error:?}"))?;
        let parts = steward_types::task_input_archive::split_task_input_archive(&archive)
            .map_err(|error| format!("split scratch-only workspace frame: {error:?}"))?;
        assert_eq!(parts.caller_archive, b"caller");
        let material = parts
            .workspace_archive
            .ok_or_else(|| "scratch-only workspace manifest was not framed".to_owned())?;
        assert!(
            material
                .windows(b"manifest.json".len())
                .any(|window| window == b"manifest.json")
        );
        assert!(
            !material
                .windows(b"packs/".len())
                .any(|window| window == b"packs/")
        );
        Ok(())
    }

    #[test]
    fn browser_inputs_round_trip_through_the_persisted_archive() -> Result<(), String> {
        let inputs = serde_json::json!({
            "release": "v1.2.3",
            "nested": { "enabled": true },
        });
        let archive = browser_inputs_archive(&inputs).map_err(|error| error.to_string())?;
        assert_eq!(
            browser_inputs_from_archive(&archive).map_err(|error| error.to_string())?,
            inputs
        );
        Ok(())
    }

    #[test]
    fn browser_rerun_preserves_the_exact_package_inputs_and_diagnostics() -> Result<(), String> {
        let evidence = serde_json::from_value(serde_json::json!({
            "source": "inline",
            "revision": "steward:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "path": "task-definition.json",
            "closureDigest": "steward:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "inlineFiles": {
                "task-definition.json": "{}",
                "prompt.md": "Inspect the repository."
            },
            "diagnostics": { "executionLog": "full" }
        }))
        .map_err(|error| error.to_string())?;
        let inputs = serde_json::json!({ "repository": "example-org/repository" });
        let current_digest = steward_types::direct_package::EnvelopeDigest::parse(format!(
            "steward:sha256:{}",
            "b".repeat(64)
        ))?;

        let request = browser_rerun_submission(&evidence, inputs.clone(), current_digest.clone());

        assert_eq!(request.package.source, evidence.source);
        assert_eq!(
            request.package.revision.as_deref(),
            Some(evidence.revision.as_str())
        );
        assert_eq!(request.package.path, evidence.path);
        assert_eq!(request.package.files, evidence.inline_files);
        assert_eq!(request.inputs, inputs);
        assert_eq!(request.diagnostics, evidence.diagnostics);
        assert_eq!(request.envelope_digest, Some(current_digest));
        Ok(())
    }

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
    fn versioned_workflow_accepts_a_typed_envelope_digest_selector() {
        let request = serde_json::from_value::<TaskSubmissionRequest>(serde_json::json!({
            "workflow": "repository-review@1",
            "envelopeDigest": format!("steward:sha256:{}", "a".repeat(64))
        }));
        assert!(
            request.is_ok(),
            "versioned Workflow submissions must accept the public Envelope digest selector"
        );
    }

    #[test]
    fn direct_package_accepts_a_typed_envelope_digest_selector() {
        let request = serde_json::from_value::<TaskCreateRequest>(serde_json::json!({
            "contractVersion": "steward.task/v2",
            "invocationPath": ".steward/invocations/release-summary.json",
            "envelopeDigest": format!("steward:sha256:{}", "b".repeat(64))
        }));
        assert!(
            matches!(request, Ok(TaskCreateRequest::Direct(_))),
            "direct-package submissions must accept the public Envelope digest selector"
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
        let mut catalog =
            serde_json::from_str::<serde_json::Value>(&execution_catalog(&workflow.agent, 'a')?)
                .map_err(|error| error.to_string())?;
        catalog["bindings"][0]["adapter"] = serde_json::json!("future-v1");
        let result = TaskApiConfig::new(None)?
            .with_execution_bindings_json(Some(&catalog.to_string()))?
            .with_execution_bindings_active(true);
        assert!(
            result.is_err_and(|reason| reason.contains("uses unavailable adapter future-v1")),
            "an active catalog must fail startup when no implementation owns its adapter contract"
        );
        Ok(())
    }

    #[test]
    fn malformed_versioned_workflow_is_rejected() {
        for workflow in [
            "repository-review@latest",
            "repository-review@0",
            "repository-review@",
            "repository-review@1@2",
        ] {
            assert!(
                versioned_workflow_reference(workflow, None).is_err(),
                "malformed versioned reference {workflow:?} must be rejected"
            );
        }
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
                runtime_minutes_limit: None,
                ttl: Duration("15m".to_owned()),
                runner: RunnerRequirements::default(),
            },
        };
        Ok(EnvelopeRequestRecord {
            id: Uuid::new_v4(),
            owner_user_id: CanonicalUserId::parse(owner_user_id)?,
            template_id: Some("developer".to_owned()),
            template_revision: Some(2),
            requested_envelope: envelope.clone(),
            approved_envelope: Some(envelope),
            status: EnvelopeRequestStatus::Provisioned,
            approval_id: None,
            envelope_instance_id: Some("env_instance_01".to_owned()),
            envelope_digest: Some("envelope-digest".to_owned()),
            reason: None,
            rationale: None,
            evidence_url: None,
            decision_key: None,
            expires_at: None,
            status_actor: owner_user_id.to_owned(),
            status_template_revision: Some(2),
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
        assert_eq!(plan.envelope.template_revision, Some(2));
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
        BoxFuture, IDENTITY_CLOCK_SKEW_SECONDS, IdentityTaskClaims, IdentityTaskIdentityResolver,
        IdentityTaskStore, MAX_IDENTITY_TASK_TOKEN_AGE_SECONDS, TaskAuthenticationError,
        TaskIdentityResolver, compatibility_task_identity_from_claims,
        map_canonical_identity_error, task_identity_from_identity_claims, valid_identity_issuer,
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
    use std::sync::Arc;
    use steward_store::{
        FederatedSubjectAssociationMethod, FederatedSubjectObservation, FederatedSubjectRecord,
        FederatedSubjectState, StoreError,
    };
    use steward_types::{CanonicalPrincipal, CanonicalUserId, Email, OrganizationId};
    use uuid::Uuid;

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

    #[derive(Serialize)]
    struct FederatedClaims<'a> {
        iss: &'a str,
        sub: &'a str,
        aud: Vec<&'a str>,
        exp: u64,
        iat: u64,
        nbf: u64,
        jti: &'a str,
        identity_contract: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        email: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        email_verified: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        groups: Option<Vec<&'a str>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        canonical_user_id: Option<&'a str>,
    }

    #[derive(Clone, Copy)]
    enum SeedFailure {
        Disabled,
        Conflict,
        Unavailable,
    }

    struct V2SeedFailureStore {
        principal: CanonicalPrincipal,
        seed_failure: SeedFailure,
    }

    struct PreassociatedFederatedStore {
        principal: CanonicalPrincipal,
    }

    struct UnknownCanonicalUserStore;

    impl PreassociatedFederatedStore {
        fn record(&self) -> FederatedSubjectRecord {
            FederatedSubjectRecord {
                subject_id: Uuid::nil(),
                issuer: ISSUER.to_owned(),
                subject: "github-actions:actor:16106037".to_owned(),
                state: FederatedSubjectState::Associated,
                canonical_user_id: Some(self.principal.user_id.clone()),
                actor_login: Some("alice".to_owned()),
                display_name: Some("Alice".to_owned()),
                association_method: Some(FederatedSubjectAssociationMethod::ConnectionVerification),
                revision: 1,
                first_seen_at: "2026-01-01T00:00:00Z".to_owned(),
                last_seen_at: "2026-01-01T00:00:00Z".to_owned(),
                updated_at: "2026-01-01T00:00:00Z".to_owned(),
            }
        }
    }

    impl IdentityTaskStore for PreassociatedFederatedStore {
        fn resolve_canonical_principal<'a>(
            &'a self,
            _user_id: &'a CanonicalUserId,
            _current_verified_email: &'a Email,
        ) -> BoxFuture<'a, Result<CanonicalPrincipal, StoreError>> {
            Box::pin(async move { Ok(self.principal.clone()) })
        }

        fn seed_federated_subject_association<'a>(
            &'a self,
            _observation: FederatedSubjectObservation<'a>,
            _canonical_user_id: &'a CanonicalUserId,
            _actor: &'a str,
        ) -> BoxFuture<'a, Result<FederatedSubjectRecord, StoreError>> {
            Box::pin(async move { Ok(self.record()) })
        }

        fn observe_federated_subject<'a>(
            &'a self,
            _observation: FederatedSubjectObservation<'a>,
        ) -> BoxFuture<'a, Result<FederatedSubjectRecord, StoreError>> {
            Box::pin(async move { Ok(self.record()) })
        }

        fn resolve_federated_subject<'a>(
            &'a self,
            issuer: &'a str,
            subject: &'a str,
        ) -> BoxFuture<'a, Result<CanonicalPrincipal, StoreError>> {
            Box::pin(async move {
                if issuer == ISSUER && subject == "github-actions:actor:16106037" {
                    Ok(self.principal.clone())
                } else {
                    Err(StoreError::FederatedSubjectNotFound)
                }
            })
        }
    }

    impl IdentityTaskStore for V2SeedFailureStore {
        fn resolve_canonical_principal<'a>(
            &'a self,
            _user_id: &'a CanonicalUserId,
            _current_verified_email: &'a Email,
        ) -> BoxFuture<'a, Result<CanonicalPrincipal, StoreError>> {
            Box::pin(async move { Ok(self.principal.clone()) })
        }

        fn seed_federated_subject_association<'a>(
            &'a self,
            _observation: FederatedSubjectObservation<'a>,
            _canonical_user_id: &'a CanonicalUserId,
            _actor: &'a str,
        ) -> BoxFuture<'a, Result<FederatedSubjectRecord, StoreError>> {
            Box::pin(async move {
                Err(match self.seed_failure {
                    SeedFailure::Disabled => StoreError::FederatedSubjectDisabled,
                    SeedFailure::Conflict => StoreError::FederatedSubjectConflict,
                    SeedFailure::Unavailable => {
                        StoreError::Database("synthetic availability failure".to_owned())
                    }
                })
            })
        }

        fn observe_federated_subject<'a>(
            &'a self,
            _observation: FederatedSubjectObservation<'a>,
        ) -> BoxFuture<'a, Result<FederatedSubjectRecord, StoreError>> {
            Box::pin(async { Err(StoreError::FederatedSubjectNotFound) })
        }

        fn resolve_federated_subject<'a>(
            &'a self,
            _issuer: &'a str,
            _subject: &'a str,
        ) -> BoxFuture<'a, Result<CanonicalPrincipal, StoreError>> {
            Box::pin(async { Err(StoreError::FederatedSubjectNotFound) })
        }
    }

    impl IdentityTaskStore for UnknownCanonicalUserStore {
        fn resolve_canonical_principal<'a>(
            &'a self,
            _user_id: &'a CanonicalUserId,
            _current_verified_email: &'a Email,
        ) -> BoxFuture<'a, Result<CanonicalPrincipal, StoreError>> {
            Box::pin(async { Err(StoreError::CanonicalIdentityNotFound) })
        }

        fn seed_federated_subject_association<'a>(
            &'a self,
            _observation: FederatedSubjectObservation<'a>,
            _canonical_user_id: &'a CanonicalUserId,
            _actor: &'a str,
        ) -> BoxFuture<'a, Result<FederatedSubjectRecord, StoreError>> {
            Box::pin(async { Err(StoreError::CanonicalIdentityNotFound) })
        }

        fn observe_federated_subject<'a>(
            &'a self,
            _observation: FederatedSubjectObservation<'a>,
        ) -> BoxFuture<'a, Result<FederatedSubjectRecord, StoreError>> {
            Box::pin(async { Err(StoreError::FederatedSubjectNotFound) })
        }

        fn resolve_federated_subject<'a>(
            &'a self,
            _issuer: &'a str,
            _subject: &'a str,
        ) -> BoxFuture<'a, Result<CanonicalPrincipal, StoreError>> {
            Box::pin(async { Err(StoreError::FederatedSubjectNotFound) })
        }
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
                email: "alice@example.com",
                email_verified: true,
                groups: vec![
                    "agents.apelogic.ai/acting-user:alice@example.com",
                    "agents.apelogic.ai/canonical-user:usr_528fc0fed6cf400abb93a3f327d9a809",
                    "agents.apelogic.ai/service-principal:steward-run",
                ],
                identity_contract: contract,
            },
            key,
        )
        .map_err(|error| format!("sign Identity task token: {error}"))
    }

    fn federated_token_with<'a>(
        key: &EncodingKey,
        kid: &str,
        claims: FederatedClaims<'a>,
    ) -> Result<String, String> {
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some(kid.to_owned());
        encode(&header, &claims, key).map_err(|error| format!("sign federated task token: {error}"))
    }

    fn federated_claims(now: u64) -> FederatedClaims<'static> {
        FederatedClaims {
            iss: ISSUER,
            sub: "github-actions:actor:16106037",
            aud: vec![AUDIENCE],
            exp: now + 60,
            iat: now,
            nbf: now.saturating_sub(1),
            jti: "federated-task-test-jti",
            identity_contract: "steward-task-v3",
            email: None,
            email_verified: None,
            groups: None,
            canonical_user_id: None,
        }
    }

    fn federated_token(key: &EncodingKey) -> Result<String, String> {
        let now = jsonwebtoken::get_current_timestamp();
        federated_token_with(key, KID, federated_claims(now))
    }

    #[test]
    fn authenticated_v2_unknown_user_is_not_an_invalid_credential() {
        assert_eq!(
            map_canonical_identity_error(StoreError::CanonicalIdentityNotFound),
            TaskAuthenticationError::UnknownUser,
            "a verified v2 credential naming an unknown canonical user must be distinguishable from an invalid credential"
        );
    }

    #[tokio::test]
    async fn verified_v2_token_reports_unknown_canonical_user_after_verification()
    -> Result<(), String> {
        let (key, jwks) = key_material()?;
        let assertion = token(&key, ISSUER, AUDIENCE, "steward-task-v2")?;
        let resolver = IdentityTaskIdentityResolver {
            jwks,
            issuer: ISSUER.to_owned(),
            audience: AUDIENCE.to_owned(),
            canonical_identities: Arc::new(UnknownCanonicalUserStore),
            federated_subjects_enabled: false,
        };

        assert_eq!(
            resolver.resolve(&assertion).await,
            Err(TaskAuthenticationError::UnknownUser)
        );
        Ok(())
    }

    #[tokio::test]
    async fn v2_resolver_keeps_authentication_authoritative_when_transition_seeding_fails()
    -> Result<(), String> {
        let (key, jwks) = key_material()?;
        let assertion = token(&key, ISSUER, AUDIENCE, "steward-task-v2")?;
        let principal = CanonicalPrincipal::new(
            CanonicalUserId::parse("usr_528fc0fed6cf400abb93a3f327d9a809")?,
            OrganizationId::parse("org_example")?,
            Email::parse("alice@example.com")?,
        )?;

        for seed_failure in [
            SeedFailure::Disabled,
            SeedFailure::Conflict,
            SeedFailure::Unavailable,
        ] {
            let resolver = IdentityTaskIdentityResolver {
                jwks: jwks.clone(),
                issuer: ISSUER.to_owned(),
                audience: AUDIENCE.to_owned(),
                canonical_identities: Arc::new(V2SeedFailureStore {
                    principal: principal.clone(),
                    seed_failure,
                }),
                federated_subjects_enabled: true,
            };

            let identity = resolver.resolve(&assertion).await.map_err(|error| {
                format!("v2 authentication was rejected by best-effort seeding: {error:?}")
            })?;
            assert_eq!(identity.owner.as_str(), "alice@example.com");
            assert_eq!(
                identity.canonical_user_id.as_str(),
                "usr_528fc0fed6cf400abb93a3f327d9a809"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn v3_resolver_accepts_subject_preassociated_by_verified_connection() -> Result<(), String>
    {
        let (key, jwks) = key_material()?;
        let assertion = federated_token(&key)?;
        let principal = CanonicalPrincipal::new(
            CanonicalUserId::parse("usr_528fc0fed6cf400abb93a3f327d9a809")?,
            OrganizationId::parse("org_example")?,
            Email::parse("alice@example.com")?,
        )?;
        let resolver = IdentityTaskIdentityResolver {
            jwks,
            issuer: ISSUER.to_owned(),
            audience: AUDIENCE.to_owned(),
            canonical_identities: Arc::new(PreassociatedFederatedStore { principal }),
            federated_subjects_enabled: true,
        };

        let identity = resolver
            .resolve(&assertion)
            .await
            .map_err(|error| format!("preassociated v3 subject was rejected: {error:?}"))?;
        assert_eq!(identity.owner.as_str(), "alice@example.com");
        assert_eq!(
            identity.canonical_user_id.as_str(),
            "usr_528fc0fed6cf400abb93a3f327d9a809"
        );
        Ok(())
    }

    #[test]
    fn federated_task_contract_rejects_caller_supplied_canonical_identity() -> Result<(), String> {
        let (key, jwks) = key_material()?;
        let now = jsonwebtoken::get_current_timestamp();
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some(KID.to_owned());
        let assertion = encode(
            &header,
            &FederatedClaims {
                iss: ISSUER,
                sub: "github-actions:actor:16106037",
                aud: vec![AUDIENCE],
                exp: now + 60,
                iat: now,
                nbf: now.saturating_sub(1),
                jti: "caller-identity-injection",
                identity_contract: "steward-task-v3",
                email: None,
                email_verified: None,
                groups: None,
                canonical_user_id: Some("usr_0123456789abcdef0123456789abcdef"),
            },
            &key,
        )
        .map_err(|error| format!("sign injected identity token: {error}"))?;
        assert!(matches!(
            verify_identity_task_token(&assertion, &jwks, ISSUER, AUDIENCE),
            Err(TaskAuthenticationError::InvalidCredentials)
        ));
        Ok(())
    }

    #[test]
    fn federated_task_contract_fails_closed_on_subject_key_signature_and_time() -> Result<(), String>
    {
        let (key, jwks) = key_material()?;
        let (other_key, _) = key_material()?;
        let now = jsonwebtoken::get_current_timestamp();

        let invalid_subjects = [
            "",
            "github-actions:actor:0",
            "github-actions:actor:016106037",
            "github-actions:actor:alice",
            "github-actions:login:16106037",
        ];
        for subject in invalid_subjects {
            let mut claims = federated_claims(now);
            claims.sub = subject;
            let assertion = federated_token_with(&key, KID, claims)?;
            assert!(matches!(
                verify_identity_task_token(&assertion, &jwks, ISSUER, AUDIENCE),
                Err(TaskAuthenticationError::InvalidCredentials)
            ));
        }

        let wrong_kid = federated_token_with(&key, "unknown-key", federated_claims(now))?;
        let wrong_signature = federated_token_with(&other_key, KID, federated_claims(now))?;
        let mut expired_claims = federated_claims(now);
        expired_claims.exp = now.saturating_sub(61);
        let expired = federated_token_with(&key, KID, expired_claims)?;
        let mut future_claims = federated_claims(now);
        future_claims.nbf = now + IDENTITY_CLOCK_SKEW_SECONDS + 60;
        let future = federated_token_with(&key, KID, future_claims)?;
        let mut over_age_claims = federated_claims(now);
        over_age_claims.iat = now.saturating_sub(MAX_IDENTITY_TASK_TOKEN_AGE_SECONDS + 1);
        let over_age = federated_token_with(&key, KID, over_age_claims)?;
        for (case, assertion) in [
            ("unknown key ID", wrong_kid),
            ("wrong signature", wrong_signature),
            ("expired", expired),
            ("not yet valid", future),
            ("over maximum age", over_age),
        ] {
            assert!(
                matches!(
                    verify_identity_task_token(&assertion, &jwks, ISSUER, AUDIENCE),
                    Err(TaskAuthenticationError::InvalidCredentials)
                ),
                "accepted {case} federated task credential"
            );
        }

        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some(KID.to_owned());
        let wrong_algorithm = encode(
            &header,
            &federated_claims(now),
            &EncodingKey::from_secret(b"obviously-fake-test-key"),
        )
        .map_err(|error| format!("sign wrong-algorithm token: {error}"))?;
        assert!(matches!(
            verify_identity_task_token(&wrong_algorithm, &jwks, ISSUER, AUDIENCE),
            Err(TaskAuthenticationError::InvalidCredentials)
        ));
        Ok(())
    }

    #[test]
    fn federated_task_contract_accepts_stable_subject_without_v2_identity_claims()
    -> Result<(), String> {
        let (key, jwks) = key_material()?;
        let assertion = federated_token(&key)?;
        let claims = verify_identity_task_token(&assertion, &jwks, ISSUER, AUDIENCE)
            .map_err(|error| format!("valid federated task credential rejected: {error:?}"))?;
        assert_eq!(claims.sub, "github-actions:actor:16106037");
        assert_eq!(claims.identity_contract, "steward-task-v3");
        Ok(())
    }

    #[test]
    fn federated_task_contract_rejects_partial_compatibility_identity_claims() -> Result<(), String>
    {
        let (key, jwks) = key_material()?;
        let now = jsonwebtoken::get_current_timestamp();

        let mut email_only = federated_claims(now);
        email_only.email = Some("alice@example.com");
        email_only.email_verified = Some(true);
        let mut groups_only = federated_claims(now);
        groups_only.groups = Some(vec![
            "agents.apelogic.ai/acting-user:alice@example.com",
            "agents.apelogic.ai/canonical-user:usr_528fc0fed6cf400abb93a3f327d9a809",
            "agents.apelogic.ai/service-principal:steward-run",
        ]);

        for claims in [email_only, groups_only] {
            let assertion = federated_token_with(&key, KID, claims)?;
            assert!(matches!(
                verify_identity_task_token(&assertion, &jwks, ISSUER, AUDIENCE),
                Err(TaskAuthenticationError::InvalidCredentials)
            ));
        }
        Ok(())
    }

    #[test]
    fn federated_task_contract_accepts_only_complete_v2_compatibility_identity()
    -> Result<(), String> {
        let (key, jwks) = key_material()?;
        let now = jsonwebtoken::get_current_timestamp();
        let mut compatibility = federated_claims(now);
        compatibility.email = Some("alice@example.com");
        compatibility.email_verified = Some(true);
        compatibility.groups = Some(vec![
            "agents.apelogic.ai/acting-user:alice@example.com",
            "agents.apelogic.ai/canonical-user:usr_528fc0fed6cf400abb93a3f327d9a809",
            "agents.apelogic.ai/service-principal:steward-run",
        ]);
        let assertion = federated_token_with(&key, KID, compatibility)?;
        let claims = verify_identity_task_token(&assertion, &jwks, ISSUER, AUDIENCE)
            .map_err(|error| format!("complete compatibility identity rejected: {error:?}"))?;
        let identity = compatibility_task_identity_from_claims(&claims)
            .map_err(|error| format!("complete compatibility identity did not map: {error:?}"))?;
        assert_eq!(identity.owner.as_str(), "alice@example.com");
        assert_eq!(
            identity.canonical_user_id.as_str(),
            "usr_528fc0fed6cf400abb93a3f327d9a809"
        );
        Ok(())
    }

    #[test]
    fn legacy_task_identity_issuer_retains_the_v2_configuration_contract() {
        assert!(valid_identity_issuer("https://identity.example.test"));
        assert!(valid_identity_issuer("https://identity.example.test/"));
        assert!(valid_identity_issuer(
            "https://identity.example.test/tenant/"
        ));
        for invalid in [
            "http://identity.example.test",
            "https://identity.example.test/a b",
        ] {
            assert!(
                !valid_identity_issuer(invalid),
                "accepted invalid issuer {invalid}"
            );
        }
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
        assert_eq!(identity.owner.0, "alice@example.com");
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
