//! REST admission path and authenticated administrator surface.

#[cfg(feature = "admin-demo")]
pub mod admin_demo;
mod admin_ui;
pub mod agent_runs_ui;
pub mod browser_auth;
pub mod connections;
#[cfg(feature = "admin-demo")]
pub mod connections_demo;
mod connections_ui;
pub mod google_oidc;
mod tasks;
pub mod user_envelopes;
#[cfg(feature = "admin-demo")]
pub mod user_envelopes_demo;
mod user_ui;

pub use tasks::{
    KubernetesTaskIdentityResolver, StaticTaskWorkflowCatalog, TaskAdmissionDelta, TaskArchive,
    TaskAuthenticationError, TaskErrorResponse, TaskIdentity, TaskIdentityResolver,
    TaskStatusResponse, TaskSubmissionLedger, TaskSubmissionRequest, TaskWorkflow,
    TaskWorkflowCatalog, task_router,
};

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;

use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Extension, Json, Router};
use k8s_openapi::api::authentication::v1::{
    TokenReview, TokenReviewSpec, TokenReviewStatus, UserInfo,
};
use kube::api::{Api, ListParams, PostParams};
use kube::core::Request as KubeRequest;
use kube::{Client, ResourceExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use steward_admission::{
    AdmissionDecision, AdmissionDelta, Envelope, EnvelopeScopeKind, add_budget_amount,
    evaluate_with_grants, validate_envelope,
};
pub use steward_ports::{
    DecisionChannel, DecisionReference, DecisionRequest, DecisionResolution, PortError,
};
use steward_store::{
    AgentRunPage, AgentRunQuery, AgentRunRecord, AgentRunTimelineEvent, AgentRunTimelineKind,
    AgentRunTimelineProvenance, ApprovalCandidate, ApproveAdmission, ApprovedAdmission,
    DecisionFiling, DecisionFilingClaim, GrantApplication, GrantReversion, ParkRejection,
    ParkedAdmission, PendingApproval, PgStore, StoreError,
};
use steward_types::{
    AgentRuntime, AgentRuntimeSpec, Budget, ModelRef, PENDING_APPROVAL_ANNOTATION, Principal,
    RuntimeOwnership, TaskPhase, ToolGrant,
};
use uuid::Uuid;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdmissionContext {
    pub actor: String,
    pub member_role: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdminContext {
    pub actor: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthenticatedCaller {
    pub actor: String,
    pub member_roles: Vec<String>,
    pub is_admin: bool,
    pub can_bootstrap_steward_run_service_envelope: bool,
}

pub const STEWARD_RUN_SERVICE_ENVELOPE_BOOTSTRAP_GROUP: &str =
    "agents.apelogic.ai/service-envelope-bootstrap:steward-run";
const STEWARD_RUN_SERVICE_ENVELOPE_PATH: &str = "/admin/service-envelopes/steward-run";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthenticationError {
    InvalidCredentials,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KubernetesTokenReviewAudience(String);

impl KubernetesTokenReviewAudience {
    pub fn new(value: String) -> Result<Self, &'static str> {
        if value.trim().is_empty() {
            return Err("Kubernetes TokenReview audience must be non-empty");
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

pub trait RequestAuthenticator: Clone + Send + Sync + 'static {
    fn authenticate<'a>(
        &'a self,
        bearer_token: &'a str,
    ) -> BoxFuture<'a, Result<AuthenticatedCaller, AuthenticationError>>;
}

#[derive(Clone)]
pub struct KubernetesTokenAuthenticator {
    client: Client,
    admin_group: String,
    audience: KubernetesTokenReviewAudience,
}

impl KubernetesTokenAuthenticator {
    pub fn new(
        client: Client,
        admin_group: String,
        audience: KubernetesTokenReviewAudience,
    ) -> Self {
        Self {
            client,
            admin_group,
            audience,
        }
    }
}

impl RequestAuthenticator for KubernetesTokenAuthenticator {
    fn authenticate<'a>(
        &'a self,
        bearer_token: &'a str,
    ) -> BoxFuture<'a, Result<AuthenticatedCaller, AuthenticationError>> {
        Box::pin(async move {
            let review = token_review_request(bearer_token, &self.audience);
            let reviewed = Api::<TokenReview>::all(self.client.clone())
                .create(&PostParams::default(), &review)
                .await
                .map_err(|_| AuthenticationError::Unavailable)?;
            let user = authenticated_token_review_user(reviewed.status, self.audience.as_str())
                .ok_or(AuthenticationError::InvalidCredentials)?;
            caller_from_kubernetes_user(&user, &self.admin_group)
        })
    }
}

pub(crate) fn token_review_request(
    bearer_token: &str,
    audience: &KubernetesTokenReviewAudience,
) -> TokenReview {
    TokenReview {
        spec: TokenReviewSpec {
            audiences: Some(vec![audience.as_str().to_owned()]),
            token: Some(bearer_token.to_owned()),
        },
        ..TokenReview::default()
    }
}

pub(crate) fn authenticated_token_review_user(
    status: Option<TokenReviewStatus>,
    requested_audience: &str,
) -> Option<UserInfo> {
    let status = status.filter(|status| status.authenticated == Some(true))?;
    status
        .audiences
        .as_deref()?
        .iter()
        .any(|audience| audience == requested_audience)
        .then_some(())?;
    status.user
}

#[cfg(test)]
fn caller_from_token_review(
    status: Option<TokenReviewStatus>,
    admin_group: &str,
    requested_audience: &str,
) -> Result<AuthenticatedCaller, AuthenticationError> {
    let user = authenticated_token_review_user(status, requested_audience)
        .ok_or(AuthenticationError::InvalidCredentials)?;
    let caller = caller_from_kubernetes_user(&user, admin_group)?;
    Ok(caller)
}

fn caller_from_kubernetes_user(
    user: &UserInfo,
    admin_group: &str,
) -> Result<AuthenticatedCaller, AuthenticationError> {
    let actor = user
        .username
        .clone()
        .filter(|username| !username.is_empty())
        .ok_or(AuthenticationError::InvalidCredentials)?;
    let groups = user.groups.as_deref().unwrap_or_default();
    let bootstrap_group_count = groups
        .iter()
        .filter(|group| group.as_str() == STEWARD_RUN_SERVICE_ENVELOPE_BOOTSTRAP_GROUP)
        .count();
    if bootstrap_group_count > 1 {
        return Err(AuthenticationError::InvalidCredentials);
    }
    let member_roles: Vec<String> = groups
        .iter()
        .filter_map(|group| group.strip_prefix("agents.apelogic.ai/member-role:"))
        .filter(|role| !role.is_empty())
        .map(str::to_owned)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let is_admin = groups.iter().any(|group| group == admin_group);
    if bootstrap_group_count == 1 && (is_admin || !member_roles.is_empty()) {
        return Err(AuthenticationError::InvalidCredentials);
    }
    Ok(AuthenticatedCaller {
        actor,
        member_roles,
        is_admin,
        can_bootstrap_steward_run_service_envelope: bootstrap_group_count == 1,
    })
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BudgetIncrease {
    pub amount: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateRuntimeRequest {
    pub name: String,
    pub spec: AgentRuntimeSpec,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalRequest {
    pub rationale: String,
    pub evidence_url: String,
    pub expires_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GrantRevocationRequest {
    pub reason: String,
}

#[derive(utoipa::OpenApi)]
#[openapi(
    paths(
        create_runtime_contract,
        budget_increase_contract,
        task_submission_contract,
        task_inputs_contract,
        task_execute_contract,
        task_status_contract,
        task_outputs_contract,
        task_delete_contract,
        admin_ui::bootstrap,
        agent_runs_contract,
        agent_run_contract,
        agent_run_timeline_contract
    ),
    components(schemas(
        CreateRuntimeRequest,
        BudgetIncrease,
        TaskSubmissionRequest,
        TaskStatusResponse,
        TaskAdmissionDelta,
        TaskArchive,
        TaskErrorResponse,
        admin_ui::AdminBootstrapResponse,
        admin_ui::AdminSurface,
        AgentRunAvailability,
        AgentRunDataStatus,
        AgentRunSpendView,
        AgentRunView,
        AgentRunsResponse,
        AgentRunResponse,
        AgentRunTimelineEventView,
        AgentRunTimelineProvenanceView,
        AgentRunTimelineResponse
    )),
    modifiers(&TaskSecurity)
)]
pub struct ApiDoc;

struct TaskSecurity;

impl utoipa::Modify for TaskSecurity {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};

        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                "taskBearer",
                SecurityScheme::Http(
                    HttpBuilder::new()
                        .scheme(HttpAuthScheme::Bearer)
                        .bearer_format("OIDC assertion")
                        .build(),
                ),
            );
            components.add_security_scheme(
                "adminBearer",
                SecurityScheme::Http(
                    HttpBuilder::new()
                        .scheme(HttpAuthScheme::Bearer)
                        .bearer_format("short-lived OIDC assertion")
                        .build(),
                ),
            );
        }
    }
}

#[utoipa::path(
    post,
    path = "/v1/namespaces/{namespace}/runtimes",
    params(
        ("namespace" = String, Path, description = "AgentRuntime namespace")
    ),
    request_body = CreateRuntimeRequest,
    responses(
        (status = 201, description = "In-envelope runtime manifest admitted and created"),
        (status = 202, description = "Over-envelope runtime manifest parked for approval"),
        (status = 403, description = "Authenticated principal does not own the runtime"),
        (status = 404, description = "Kubernetes namespace does not exist"),
        (status = 409, description = "AgentRuntime name already exists"),
        (status = 422, description = "Manifest is invalid or cannot be bound to authority"),
        (status = 503, description = "Kubernetes, authority store, or decision channel unavailable")
    )
)]
#[doc(hidden)]
pub async fn create_runtime_contract() {}

#[utoipa::path(
    patch,
    path = "/v1/namespaces/{namespace}/runtimes/{name}/budget",
    params(
        ("namespace" = String, Path, description = "AgentRuntime namespace"),
        ("name" = String, Path, description = "AgentRuntime name")
    ),
    request_body = BudgetIncrease,
    responses(
        (status = 200, description = "Composed absolute manifest admitted and applied"),
        (status = 202, description = "Composed absolute manifest rejected and parked"),
        (status = 403, description = "Authenticated principal does not own the runtime"),
        (status = 422, description = "Edit or composed manifest is invalid")
    )
)]
#[doc(hidden)]
pub async fn budget_increase_contract() {}

#[utoipa::path(
    post,
    path = "/v1/tasks",
    request_body(content = TaskSubmissionRequest, content_type = "application/json"),
    params(
        ("Idempotency-Key" = String, Header, description = "Submitter-scoped upstream job identity")
    ),
    security(("taskBearer" = [])),
    responses(
        (status = 201, description = "Task admitted without an approval hold and bound to its runtime", body = TaskStatusResponse, content_type = "application/json"),
        (status = 202, description = "Task parked on a governed approval hold; execution resumes after approval", body = TaskStatusResponse, content_type = "application/json"),
        (status = 400, description = "Submission JSON is malformed", body = String, content_type = "text/plain"),
        (status = 401, description = "Identity assertion is invalid", body = TaskErrorResponse, content_type = "application/json"),
        (status = 404, description = "Selected workflow does not exist", body = TaskErrorResponse, content_type = "application/json"),
        (status = 409, description = "Idempotency key or adopted runtime conflicts", body = TaskErrorResponse, content_type = "application/json"),
        (status = 415, description = "Content-Type is not application/json", body = String, content_type = "text/plain"),
        (status = 422, description = "Workflow, runtime version, service envelope, or authority is invalid", body = TaskErrorResponse, content_type = "application/json"),
        (status = 503, description = "Identity, Kubernetes, persistence, or decision dependency unavailable", body = TaskErrorResponse, content_type = "application/json")
    )
)]
#[doc(hidden)]
pub async fn task_submission_contract() {}

#[utoipa::path(
    put,
    path = "/v1/tasks/{taskUid}/inputs",
    params(("taskUid" = String, Path, format = "uuid", description = "Task lifecycle identity")),
    request_body(content = TaskArchive, content_type = "application/x-tar", description = "Opaque workspace-relative tar archive; maximum 64 MiB"),
    security(("taskBearer" = [])),
    responses(
        (status = 204, description = "Input archive staged durably; an identical retry is idempotent"),
        (status = 400, description = "taskUid is not a UUID", body = String, content_type = "text/plain"),
        (status = 401, description = "Identity assertion is invalid", body = TaskErrorResponse, content_type = "application/json"),
        (status = 404, description = "Task is not owned by the resolved submitter", body = TaskErrorResponse, content_type = "application/json"),
        (status = 409, description = "Task no longer accepts inputs or the retry body differs", body = TaskErrorResponse, content_type = "application/json"),
        (status = 413, description = "Input archive exceeds 64 MiB", body = TaskErrorResponse, content_type = "application/json"),
        (status = 415, description = "Content-Type is not application/x-tar", body = TaskErrorResponse, content_type = "application/json"),
        (status = 503, description = "Identity or persistence dependency unavailable", body = TaskErrorResponse, content_type = "application/json")
    )
)]
#[doc(hidden)]
pub async fn task_inputs_contract() {}

#[utoipa::path(
    post,
    path = "/v1/tasks/{taskUid}/execute",
    params(("taskUid" = String, Path, format = "uuid", description = "Task lifecycle identity")),
    security(("taskBearer" = [])),
    responses(
        (status = 202, description = "Execution requested durably; repeated requests are idempotent", body = TaskStatusResponse, content_type = "application/json"),
        (status = 400, description = "taskUid is not a UUID", body = String, content_type = "text/plain"),
        (status = 401, description = "Identity assertion is invalid", body = TaskErrorResponse, content_type = "application/json"),
        (status = 404, description = "Task is not owned by the resolved submitter", body = TaskErrorResponse, content_type = "application/json"),
        (status = 409, description = "Inputs are absent, cleanup was requested, or the phase cannot execute", body = TaskErrorResponse, content_type = "application/json"),
        (status = 503, description = "Identity or persistence dependency unavailable", body = TaskErrorResponse, content_type = "application/json")
    )
)]
#[doc(hidden)]
pub async fn task_execute_contract() {}

#[utoipa::path(
    get,
    path = "/v1/tasks/{taskUid}",
    params(("taskUid" = String, Path, format = "uuid", description = "Task lifecycle identity")),
    security(("taskBearer" = [])),
    responses(
        (status = 200, description = "Current durable Task phase and cleanup state", body = TaskStatusResponse, content_type = "application/json"),
        (status = 400, description = "taskUid is not a UUID", body = String, content_type = "text/plain"),
        (status = 401, description = "Identity assertion is invalid", body = TaskErrorResponse, content_type = "application/json"),
        (status = 404, description = "Task is not owned by the resolved submitter", body = TaskErrorResponse, content_type = "application/json"),
        (status = 503, description = "Identity or persistence dependency unavailable", body = TaskErrorResponse, content_type = "application/json")
    )
)]
#[doc(hidden)]
pub async fn task_status_contract() {}

#[utoipa::path(
    get,
    path = "/v1/tasks/{taskUid}/outputs",
    params(("taskUid" = String, Path, format = "uuid", description = "Task lifecycle identity")),
    security(("taskBearer" = [])),
    responses(
        (status = 200, description = "Opaque workspace-relative output tar archive; maximum 64 MiB", body = TaskArchive, content_type = "application/x-tar"),
        (status = 400, description = "taskUid is not a UUID", body = String, content_type = "text/plain"),
        (status = 401, description = "Identity assertion is invalid", body = TaskErrorResponse, content_type = "application/json"),
        (status = 404, description = "Task is not owned by the resolved submitter", body = TaskErrorResponse, content_type = "application/json"),
        (status = 409, description = "Task has not succeeded or output is not available", body = TaskErrorResponse, content_type = "application/json"),
        (status = 503, description = "Identity or persistence dependency unavailable", body = TaskErrorResponse, content_type = "application/json")
    )
)]
#[doc(hidden)]
pub async fn task_outputs_contract() {}

#[utoipa::path(
    delete,
    path = "/v1/tasks/{taskUid}",
    params(("taskUid" = String, Path, format = "uuid", description = "Task lifecycle identity")),
    security(("taskBearer" = [])),
    responses(
        (status = 202, description = "Cleanup requested durably; repeated requests are idempotent", body = TaskStatusResponse, content_type = "application/json"),
        (status = 400, description = "taskUid is not a UUID", body = String, content_type = "text/plain"),
        (status = 401, description = "Identity assertion is invalid", body = TaskErrorResponse, content_type = "application/json"),
        (status = 404, description = "Task is not owned by the resolved submitter", body = TaskErrorResponse, content_type = "application/json"),
        (status = 503, description = "Identity or persistence dependency unavailable", body = TaskErrorResponse, content_type = "application/json")
    )
)]
#[doc(hidden)]
pub async fn task_delete_contract() {}

#[utoipa::path(
    get,
    path = "/admin/api/v1/runs",
    params(
        ("limit" = Option<u16>, Query, description = "Page size from 1 through 100; defaults to 50"),
        ("cursor" = Option<String>, Query, format = "uuid", description = "Task UID at the immutable page boundary"),
        ("phase" = Option<String>, Query, description = "Exact Task lifecycle phase"),
        ("workflow" = Option<String>, Query, description = "Exact workflow name")
    ),
    security(("adminBearer" = [])),
    responses(
        (status = 200, description = "Privacy-bounded authoritative Agent Runs page", body = AgentRunsResponse),
        (status = 400, description = "Query or cursor is invalid"),
        (status = 401, description = "Identity assertion is missing or invalid"),
        (status = 403, description = "Exact Steward administrator authority is absent"),
        (status = 503, description = "Agent-run persistence is unavailable")
    )
)]
#[doc(hidden)]
pub async fn agent_runs_contract() {}

#[utoipa::path(
    get,
    path = "/admin/api/v1/runs/{taskUid}",
    params(("taskUid" = String, Path, format = "uuid", description = "Canonical Steward Task UID")),
    security(("adminBearer" = [])),
    responses(
        (status = 200, description = "Privacy-bounded authoritative Agent Run", body = AgentRunResponse),
        (status = 401, description = "Identity assertion is missing or invalid"),
        (status = 403, description = "Exact Steward administrator authority is absent"),
        (status = 404, description = "Agent Run does not exist"),
        (status = 503, description = "Agent-run persistence is unavailable")
    )
)]
#[doc(hidden)]
pub async fn agent_run_contract() {}

#[utoipa::path(
    get,
    path = "/admin/api/v1/runs/{taskUid}/timeline",
    params(("taskUid" = String, Path, format = "uuid", description = "Canonical Steward Task UID")),
    security(("adminBearer" = [])),
    responses(
        (status = 200, description = "Append-only Task lifecycle with recorded/backfilled provenance", body = AgentRunTimelineResponse),
        (status = 401, description = "Identity assertion is missing or invalid"),
        (status = 403, description = "Exact Steward administrator authority is absent"),
        (status = 404, description = "Agent Run does not exist"),
        (status = 503, description = "Agent-run persistence is unavailable")
    )
)]
#[doc(hidden)]
pub async fn agent_run_timeline_contract() {}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "status"
)]
pub enum SubmissionOutcome {
    Applied {
        proposed_spec: AgentRuntimeSpec,
    },
    Parked {
        approval_id: Uuid,
        decision_id: Uuid,
        decision_key: String,
        evidence_url: String,
        proposed_spec: AgentRuntimeSpec,
        deltas: Vec<AdmissionDelta>,
        counterexample: String,
    },
}

const AGENT_RUNS_API_VERSION: &str = "steward.admin/runs/v1";

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AgentRunsQuery {
    #[serde(default = "default_agent_runs_limit")]
    limit: u16,
    cursor: Option<Uuid>,
    phase: Option<TaskPhase>,
    workflow: Option<String>,
}

const fn default_agent_runs_limit() -> u16 {
    50
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum AgentRunAvailability {
    Available,
    Partial,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentRunDataStatus {
    pub availability: AgentRunAvailability,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentRunSpendView {
    pub observed_amount: String,
    pub currency: String,
    pub exhausted: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentRunView {
    #[schema(value_type = String, format = "uuid")]
    pub task_uid: Uuid,
    pub submitter_service: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acting_user: Option<String>,
    pub owner: String,
    pub workflow: String,
    pub coding_agent_runtime: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_uid: Option<String>,
    pub runtime_ownership: RuntimeOwnership,
    pub phase: TaskPhase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub envelope_revision: Option<i64>,
    pub finalization_requested: bool,
    pub finalized: bool,
    pub created_at: String,
    pub updated_at: String,
    pub configured_models: Vec<ModelRef>,
    pub granted_tools: Vec<ToolGrant>,
    pub allocated_budget: Budget,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_spend: Option<AgentRunSpendView>,
    pub authority: AgentRunDataStatus,
    pub spend: AgentRunDataStatus,
    pub lifecycle: AgentRunDataStatus,
    pub tool_activity: AgentRunDataStatus,
    pub inference_activity: AgentRunDataStatus,
    pub resources: AgentRunDataStatus,
    pub github_run: AgentRunDataStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_category: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentRunsResponse {
    pub api_version: String,
    pub runs: Vec<AgentRunView>,
    #[schema(value_type = Option<String>, format = "uuid")]
    pub next_cursor: Option<Uuid>,
}

#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentRunResponse {
    pub api_version: String,
    pub run: AgentRunView,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum AgentRunTimelineEventView {
    Phase {
        phase: TaskPhase,
        provenance: AgentRunTimelineProvenanceView,
        at: String,
    },
    FinalizationRequested {
        provenance: AgentRunTimelineProvenanceView,
        at: String,
    },
    Finalized {
        provenance: AgentRunTimelineProvenanceView,
        at: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum AgentRunTimelineProvenanceView {
    Recorded,
    Backfilled,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentRunTimelineResponse {
    pub api_version: String,
    #[schema(value_type = String, format = "uuid")]
    pub task_uid: Uuid,
    pub history: AgentRunDataStatus,
    pub events: Vec<AgentRunTimelineEventView>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApiError {
    RuntimeCreate(RuntimeCreateError),
    Runtime(String),
    Store(StoreError),
    MissingRuntimeUid,
    PrincipalMismatch,
    MissingEnvelope,
    InvalidBudgetIncrease { value: String },
    Admission(String),
    DecisionChannel(String),
    Conflict(String),
    NoActiveGrants,
    TaskAuthentication,
    TaskAuthenticationUnavailable,
    TaskWorkflowNotFound,
    TaskNotReady,
    TaskOutputNotReady,
}

impl fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl Error for ApiError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeCreateError {
    Kubernetes { status: u16, message: String },
    Unavailable(String),
}

pub trait RuntimeRepository: Clone + Send + Sync + 'static {
    fn create<'a>(
        &'a self,
        namespace: &'a str,
        runtime: &'a AgentRuntime,
        context: &'a AdmissionContext,
    ) -> BoxFuture<'a, Result<AgentRuntime, RuntimeCreateError>>;

    fn create_as_authority<'a>(
        &'a self,
        namespace: &'a str,
        runtime: &'a AgentRuntime,
    ) -> BoxFuture<'a, Result<AgentRuntime, RuntimeCreateError>>;

    fn get<'a>(
        &'a self,
        namespace: &'a str,
        name: &'a str,
    ) -> BoxFuture<'a, Result<AgentRuntime, String>>;

    fn get_bound<'a>(
        &'a self,
        namespace: &'a str,
        name: &'a str,
        runtime_uid: &'a str,
    ) -> BoxFuture<'a, Result<AgentRuntime, String>>;

    fn get_by_uid<'a>(
        &'a self,
        runtime_uid: &'a str,
    ) -> BoxFuture<'a, Result<AgentRuntime, String>>;

    fn replace<'a>(
        &'a self,
        runtime: &'a AgentRuntime,
        context: &'a AdmissionContext,
    ) -> BoxFuture<'a, Result<(), String>>;

    fn replace_as_authority<'a>(
        &'a self,
        runtime: &'a AgentRuntime,
    ) -> BoxFuture<'a, Result<(), String>>;
}

#[derive(Clone)]
pub struct KubeRuntimeRepository {
    client: Client,
}

impl KubeRuntimeRepository {
    pub fn new(client: Client) -> Self {
        Self { client }
    }
}

impl RuntimeRepository for KubeRuntimeRepository {
    fn create<'a>(
        &'a self,
        namespace: &'a str,
        runtime: &'a AgentRuntime,
        context: &'a AdmissionContext,
    ) -> BoxFuture<'a, Result<AgentRuntime, RuntimeCreateError>> {
        Box::pin(async move {
            let unavailable = |error: String| RuntimeCreateError::Unavailable(error);
            let body =
                serde_json::to_vec(runtime).map_err(|error| unavailable(error.to_string()))?;
            let mut request = KubeRequest::new(format!(
                "/apis/agents.apelogic.ai/v1alpha1/namespaces/{namespace}/agentruntimes"
            ))
            .create(&PostParams::default(), body)
            .map_err(|error| unavailable(error.to_string()))?;
            request.headers_mut().insert(
                HeaderName::from_static("impersonate-user"),
                HeaderValue::from_str(&context.actor)
                    .map_err(|error| unavailable(error.to_string()))?,
            );
            request.headers_mut().insert(
                HeaderName::from_static("impersonate-group"),
                HeaderValue::from_str(&format!(
                    "agents.apelogic.ai/member-role:{}",
                    context.member_role
                ))
                .map_err(|error| unavailable(error.to_string()))?,
            );
            self.client
                .request::<AgentRuntime>(request)
                .await
                .map_err(|error| match error {
                    kube::Error::Api(response) => RuntimeCreateError::Kubernetes {
                        status: response.code,
                        message: response.message,
                    },
                    other => RuntimeCreateError::Unavailable(other.to_string()),
                })
        })
    }

    fn create_as_authority<'a>(
        &'a self,
        namespace: &'a str,
        runtime: &'a AgentRuntime,
    ) -> BoxFuture<'a, Result<AgentRuntime, RuntimeCreateError>> {
        Box::pin(async move {
            Api::<AgentRuntime>::namespaced(self.client.clone(), namespace)
                .create(&PostParams::default(), runtime)
                .await
                .map_err(|error| match error {
                    kube::Error::Api(response) => RuntimeCreateError::Kubernetes {
                        status: response.code,
                        message: response.message,
                    },
                    other => RuntimeCreateError::Unavailable(other.to_string()),
                })
        })
    }

    fn get<'a>(
        &'a self,
        namespace: &'a str,
        name: &'a str,
    ) -> BoxFuture<'a, Result<AgentRuntime, String>> {
        Box::pin(async move {
            Api::<AgentRuntime>::namespaced(self.client.clone(), namespace)
                .get(name)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn get_bound<'a>(
        &'a self,
        namespace: &'a str,
        name: &'a str,
        runtime_uid: &'a str,
    ) -> BoxFuture<'a, Result<AgentRuntime, String>> {
        Box::pin(async move {
            let runtime = Api::<AgentRuntime>::namespaced(self.client.clone(), namespace)
                .get(name)
                .await
                .map_err(|error| error.to_string())?;
            if runtime.metadata.uid.as_deref() == Some(runtime_uid) {
                Ok(runtime)
            } else {
                Err(format!(
                    "AgentRuntime {namespace}/{name} no longer has UID {runtime_uid}"
                ))
            }
        })
    }

    fn get_by_uid<'a>(
        &'a self,
        runtime_uid: &'a str,
    ) -> BoxFuture<'a, Result<AgentRuntime, String>> {
        Box::pin(async move {
            let mut matches = Api::<AgentRuntime>::all(self.client.clone())
                .list(&ListParams::default())
                .await
                .map_err(|error| error.to_string())?
                .items
                .into_iter()
                .filter(|runtime| runtime.metadata.uid.as_deref() == Some(runtime_uid));
            let runtime = matches
                .next()
                .ok_or_else(|| format!("AgentRuntime UID {runtime_uid} does not exist"))?;
            if matches.next().is_some() {
                return Err(format!(
                    "AgentRuntime UID {runtime_uid} resolved to more than one object"
                ));
            }
            Ok(runtime)
        })
    }

    fn replace<'a>(
        &'a self,
        runtime: &'a AgentRuntime,
        context: &'a AdmissionContext,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let namespace = runtime
                .namespace()
                .ok_or_else(|| "AgentRuntime namespace is required".to_owned())?;
            let name = runtime.name_any();
            let body = serde_json::to_vec(runtime).map_err(|error| error.to_string())?;
            let mut request = KubeRequest::new(format!(
                "/apis/agents.apelogic.ai/v1alpha1/namespaces/{namespace}/agentruntimes"
            ))
            .replace(&name, &PostParams::default(), body)
            .map_err(|error| error.to_string())?;
            request.headers_mut().insert(
                HeaderName::from_static("impersonate-user"),
                HeaderValue::from_str(&context.actor).map_err(|error| error.to_string())?,
            );
            request.headers_mut().insert(
                HeaderName::from_static("impersonate-group"),
                HeaderValue::from_str(&format!(
                    "agents.apelogic.ai/member-role:{}",
                    context.member_role
                ))
                .map_err(|error| error.to_string())?,
            );
            self.client
                .request::<AgentRuntime>(request)
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        })
    }

    fn replace_as_authority<'a>(
        &'a self,
        runtime: &'a AgentRuntime,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let namespace = runtime
                .namespace()
                .ok_or_else(|| "AgentRuntime namespace is required".to_owned())?;
            Api::<AgentRuntime>::namespaced(self.client.clone(), &namespace)
                .replace(&runtime.name_any(), &PostParams::default(), runtime)
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        })
    }
}

pub trait AdmissionLedger: Clone + Send + Sync + 'static {
    fn insert_envelope<'a>(
        &'a self,
        member_role: &'a str,
        envelope: &'a Envelope,
        authored_by: &'a str,
    ) -> BoxFuture<'a, Result<(), StoreError>>;

    fn insert_service_envelope<'a>(
        &'a self,
        service: &'a str,
        envelope: &'a Envelope,
        authored_by: &'a str,
    ) -> BoxFuture<'a, Result<(), StoreError>>;

    fn latest_envelope<'a>(
        &'a self,
        member_role: &'a str,
    ) -> BoxFuture<'a, Result<Option<Envelope>, StoreError>>;

    fn latest_service_envelope<'a>(
        &'a self,
        service: &'a str,
    ) -> BoxFuture<'a, Result<Option<Envelope>, StoreError>>;

    fn park_rejection<'a>(
        &'a self,
        request: ParkRejection<'a>,
    ) -> BoxFuture<'a, Result<ParkedAdmission, StoreError>>;

    fn pending_approvals(&self) -> BoxFuture<'_, Result<Vec<PendingApproval>, StoreError>>;

    fn retire_pending_approval_if_superseded<'a>(
        &'a self,
        approval_id: Uuid,
        winning_approval_id: Uuid,
        runtime_uid: &'a str,
        decided_by: &'a str,
        rationale: &'a str,
    ) -> BoxFuture<'a, Result<Option<GrantReversion>, StoreError>>;

    fn link_decision_reference<'a>(
        &'a self,
        approval_id: Uuid,
        decision_key: &'a str,
        evidence_url: &'a str,
    ) -> BoxFuture<'a, Result<(), StoreError>>;

    fn approve_admission<'a>(
        &'a self,
        request: ApproveAdmission<'a>,
    ) -> BoxFuture<'a, Result<ApprovedAdmission, StoreError>>;

    fn grants_for_runtime<'a>(
        &'a self,
        runtime_uid: &'a str,
        member_role: &'a str,
        envelope_revision: i64,
    ) -> BoxFuture<'a, Result<Vec<AdmissionDelta>, StoreError>>;

    fn grants_for_runtime_scoped<'a>(
        &'a self,
        runtime_uid: &'a str,
        scope_kind: EnvelopeScopeKind,
        scope_ref: &'a str,
        envelope_revision: i64,
    ) -> BoxFuture<'a, Result<Vec<AdmissionDelta>, StoreError>>;

    fn approval_candidate<'a>(
        &'a self,
        approval_id: Uuid,
        evidence_url: &'a str,
    ) -> BoxFuture<'a, Result<ApprovalCandidate, StoreError>>;

    fn revoke_runtime_grants<'a>(
        &'a self,
        runtime_uid: &'a str,
        revoked_by: &'a str,
        reason: &'a str,
    ) -> BoxFuture<'a, Result<u64, StoreError>>;

    fn approval_for_filing(
        &self,
        approval_id: Uuid,
    ) -> BoxFuture<'_, Result<DecisionFiling, StoreError>>;

    fn claim_decision_filing(
        &self,
        approval_id: Uuid,
    ) -> BoxFuture<'_, Result<DecisionFilingClaim, StoreError>>;

    fn complete_decision_filing<'a>(
        &'a self,
        approval_id: Uuid,
        token: Uuid,
        decision_key: &'a str,
        evidence_url: &'a str,
    ) -> BoxFuture<'a, Result<(), StoreError>>;

    fn release_decision_filing(
        &self,
        approval_id: Uuid,
        token: Uuid,
    ) -> BoxFuture<'_, Result<(), StoreError>>;

    fn grant_reversion<'a>(
        &'a self,
        runtime_uid: &'a str,
    ) -> BoxFuture<'a, Result<Option<GrantReversion>, StoreError>>;

    fn grant_application<'a>(
        &'a self,
        runtime_uid: &'a str,
    ) -> BoxFuture<'a, Result<Option<GrantApplication>, StoreError>>;
}

pub trait AgentRunLedger: Clone + Send + Sync + 'static {
    fn agent_runs<'a>(
        &'a self,
        query: &'a AgentRunQuery,
    ) -> BoxFuture<'a, Result<AgentRunPage, StoreError>>;

    fn agent_run(
        &self,
        task_uid: Uuid,
    ) -> BoxFuture<'_, Result<Option<AgentRunRecord>, StoreError>>;

    fn agent_run_timeline(
        &self,
        task_uid: Uuid,
    ) -> BoxFuture<'_, Result<Option<Vec<AgentRunTimelineEvent>>, StoreError>>;
}

impl AgentRunLedger for PgStore {
    fn agent_runs<'a>(
        &'a self,
        query: &'a AgentRunQuery,
    ) -> BoxFuture<'a, Result<AgentRunPage, StoreError>> {
        Box::pin(async move { PgStore::agent_runs(self, query).await })
    }

    fn agent_run(
        &self,
        task_uid: Uuid,
    ) -> BoxFuture<'_, Result<Option<AgentRunRecord>, StoreError>> {
        Box::pin(async move { PgStore::agent_run(self, task_uid).await })
    }

    fn agent_run_timeline(
        &self,
        task_uid: Uuid,
    ) -> BoxFuture<'_, Result<Option<Vec<AgentRunTimelineEvent>>, StoreError>> {
        Box::pin(async move { PgStore::agent_run_timeline(self, task_uid).await })
    }
}

impl AdmissionLedger for PgStore {
    fn insert_envelope<'a>(
        &'a self,
        member_role: &'a str,
        envelope: &'a Envelope,
        authored_by: &'a str,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(
            async move { PgStore::insert_envelope(self, member_role, envelope, authored_by).await },
        )
    }

    fn insert_service_envelope<'a>(
        &'a self,
        service: &'a str,
        envelope: &'a Envelope,
        authored_by: &'a str,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            PgStore::insert_service_envelope(self, service, envelope, authored_by).await
        })
    }

    fn latest_envelope<'a>(
        &'a self,
        member_role: &'a str,
    ) -> BoxFuture<'a, Result<Option<Envelope>, StoreError>> {
        Box::pin(async move { PgStore::latest_envelope(self, member_role).await })
    }

    fn latest_service_envelope<'a>(
        &'a self,
        service: &'a str,
    ) -> BoxFuture<'a, Result<Option<Envelope>, StoreError>> {
        Box::pin(async move { PgStore::latest_service_envelope(self, service).await })
    }

    fn park_rejection<'a>(
        &'a self,
        request: ParkRejection<'a>,
    ) -> BoxFuture<'a, Result<ParkedAdmission, StoreError>> {
        Box::pin(async move { PgStore::park_rejection(self, request).await })
    }

    fn pending_approvals(&self) -> BoxFuture<'_, Result<Vec<PendingApproval>, StoreError>> {
        Box::pin(async move { PgStore::pending_approvals(self).await })
    }

    fn retire_pending_approval_if_superseded<'a>(
        &'a self,
        approval_id: Uuid,
        winning_approval_id: Uuid,
        runtime_uid: &'a str,
        decided_by: &'a str,
        rationale: &'a str,
    ) -> BoxFuture<'a, Result<Option<GrantReversion>, StoreError>> {
        Box::pin(async move {
            PgStore::retire_pending_approval_if_superseded(
                self,
                approval_id,
                winning_approval_id,
                runtime_uid,
                decided_by,
                rationale,
            )
            .await
        })
    }

    fn link_decision_reference<'a>(
        &'a self,
        approval_id: Uuid,
        decision_key: &'a str,
        evidence_url: &'a str,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            PgStore::link_decision_reference(self, approval_id, decision_key, evidence_url).await
        })
    }

    fn approve_admission<'a>(
        &'a self,
        request: ApproveAdmission<'a>,
    ) -> BoxFuture<'a, Result<ApprovedAdmission, StoreError>> {
        Box::pin(async move { PgStore::approve_admission(self, request).await })
    }

    fn grants_for_runtime<'a>(
        &'a self,
        runtime_uid: &'a str,
        member_role: &'a str,
        envelope_revision: i64,
    ) -> BoxFuture<'a, Result<Vec<AdmissionDelta>, StoreError>> {
        Box::pin(async move {
            PgStore::grants_for_runtime(self, runtime_uid, member_role, envelope_revision).await
        })
    }

    fn grants_for_runtime_scoped<'a>(
        &'a self,
        runtime_uid: &'a str,
        scope_kind: EnvelopeScopeKind,
        scope_ref: &'a str,
        envelope_revision: i64,
    ) -> BoxFuture<'a, Result<Vec<AdmissionDelta>, StoreError>> {
        Box::pin(async move {
            PgStore::grants_for_runtime_scoped(
                self,
                runtime_uid,
                scope_kind,
                scope_ref,
                envelope_revision,
            )
            .await
        })
    }

    fn approval_candidate<'a>(
        &'a self,
        approval_id: Uuid,
        evidence_url: &'a str,
    ) -> BoxFuture<'a, Result<ApprovalCandidate, StoreError>> {
        Box::pin(async move { PgStore::approval_candidate(self, approval_id, evidence_url).await })
    }

    fn revoke_runtime_grants<'a>(
        &'a self,
        runtime_uid: &'a str,
        revoked_by: &'a str,
        reason: &'a str,
    ) -> BoxFuture<'a, Result<u64, StoreError>> {
        Box::pin(async move {
            PgStore::revoke_runtime_grants(self, runtime_uid, revoked_by, reason).await
        })
    }

    fn approval_for_filing(
        &self,
        approval_id: Uuid,
    ) -> BoxFuture<'_, Result<DecisionFiling, StoreError>> {
        Box::pin(async move { PgStore::approval_for_filing(self, approval_id).await })
    }

    fn claim_decision_filing(
        &self,
        approval_id: Uuid,
    ) -> BoxFuture<'_, Result<DecisionFilingClaim, StoreError>> {
        Box::pin(async move { PgStore::claim_decision_filing(self, approval_id).await })
    }

    fn complete_decision_filing<'a>(
        &'a self,
        approval_id: Uuid,
        token: Uuid,
        decision_key: &'a str,
        evidence_url: &'a str,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            PgStore::complete_decision_filing(self, approval_id, token, decision_key, evidence_url)
                .await
        })
    }

    fn release_decision_filing(
        &self,
        approval_id: Uuid,
        token: Uuid,
    ) -> BoxFuture<'_, Result<(), StoreError>> {
        Box::pin(async move { PgStore::release_decision_filing(self, approval_id, token).await })
    }

    fn grant_reversion<'a>(
        &'a self,
        runtime_uid: &'a str,
    ) -> BoxFuture<'a, Result<Option<GrantReversion>, StoreError>> {
        Box::pin(async move { PgStore::grant_reversion(self, runtime_uid).await })
    }

    fn grant_application<'a>(
        &'a self,
        runtime_uid: &'a str,
    ) -> BoxFuture<'a, Result<Option<GrantApplication>, StoreError>> {
        Box::pin(async move { PgStore::grant_application(self, runtime_uid).await })
    }
}

#[derive(Clone)]
struct AppState<R, L, D> {
    runtimes: R,
    ledger: L,
    decisions: D,
}

pub fn router<R, L, A, D>(runtimes: R, ledger: L, authenticator: A, decisions: D) -> Router
where
    R: RuntimeRepository,
    L: AdmissionLedger + AgentRunLedger,
    A: RequestAuthenticator,
    D: DecisionChannel + Clone,
{
    let admission = Router::new()
        .route(
            "/v1/namespaces/{namespace}/runtimes",
            post(create_runtime_handler::<R, L, D>),
        )
        .route(
            "/v1/namespaces/{namespace}/runtimes/{name}/budget",
            patch(budget_increase_handler::<R, L, D>),
        )
        .route_layer(middleware::from_fn_with_state(
            authenticator.clone(),
            authenticate_admission::<A>,
        ));
    let admin = protect_admin_routes(
        Router::new()
            .merge(admin_ui::router::<AppState<R, L, D>>())
            .route("/admin/api/v1/runs", get(agent_runs_handler::<R, L, D>))
            .route(
                "/admin/api/v1/runs/{taskUid}",
                get(agent_run_handler::<R, L, D>),
            )
            .route(
                "/admin/api/v1/runs/{taskUid}/timeline",
                get(agent_run_timeline_handler::<R, L, D>),
            )
            .route("/admin/approvals", get(approval_queue_handler::<R, L, D>))
            .route(
                "/admin/envelopes/{member_role}",
                post(author_envelope_handler::<R, L, D>),
            )
            .route(
                "/admin/service-envelopes/{service}",
                post(author_service_envelope_handler::<R, L, D>),
            )
            .route(
                "/admin/approvals/{approval_id}/approve",
                post(approve_handler::<R, L, D>),
            )
            .route(
                "/admin/approvals/{approval_id}/file",
                post(file_decision_handler::<R, L, D>),
            )
            .route(
                "/admin/runtimes/{runtime_uid}/grants/revoke",
                post(revoke_grants_handler::<R, L, D>),
            ),
        authenticator,
    );
    admission.merge(admin).with_state(AppState {
        runtimes,
        ledger,
        decisions,
    })
}

pub(crate) fn protect_admin_routes<S, A>(routes: Router<S>, authenticator: A) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    A: RequestAuthenticator,
{
    routes
        .route_layer(middleware::from_fn_with_state(
            authenticator,
            authenticate_admin::<A>,
        ))
        .route_layer(middleware::from_fn(admin_ui::add_browser_security_headers))
}

async fn agent_runs_handler<R, L, D>(
    State(state): State<AppState<R, L, D>>,
    Query(query): Query<AgentRunsQuery>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger + AgentRunLedger,
    D: DecisionChannel + Clone,
{
    let query = AgentRunQuery {
        limit: query.limit,
        cursor: query.cursor,
        phase: query.phase,
        workflow: query.workflow,
        owner_user_id: None,
        runtime_uid: None,
        task_uid: None,
    };
    match state.ledger.agent_runs(&query).await {
        Ok(page) => Json(AgentRunsResponse {
            api_version: AGENT_RUNS_API_VERSION.to_owned(),
            runs: page.records.into_iter().map(agent_run_view).collect(),
            next_cursor: page.next_cursor,
        })
        .into_response(),
        Err(StoreError::InvalidRunQuery | StoreError::InvalidRunCursor) => agent_run_error(
            StatusCode::BAD_REQUEST,
            "agent-run query or cursor is invalid",
        ),
        Err(_) => agent_run_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent-run persistence is unavailable",
        ),
    }
}

async fn agent_run_handler<R, L, D>(
    State(state): State<AppState<R, L, D>>,
    Path(task_uid): Path<Uuid>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger + AgentRunLedger,
    D: DecisionChannel + Clone,
{
    match state.ledger.agent_run(task_uid).await {
        Ok(Some(record)) => Json(AgentRunResponse {
            api_version: AGENT_RUNS_API_VERSION.to_owned(),
            run: agent_run_view(record),
        })
        .into_response(),
        Ok(None) => agent_run_error(StatusCode::NOT_FOUND, "agent run does not exist"),
        Err(_) => agent_run_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent-run persistence is unavailable",
        ),
    }
}

async fn agent_run_timeline_handler<R, L, D>(
    State(state): State<AppState<R, L, D>>,
    Path(task_uid): Path<Uuid>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger + AgentRunLedger,
    D: DecisionChannel + Clone,
{
    match state.ledger.agent_run_timeline(task_uid).await {
        Ok(Some(events)) => {
            let partial = events
                .iter()
                .any(|event| event.provenance == AgentRunTimelineProvenance::Backfilled);
            let observed_at = events.last().map(|event| event.at.clone());
            Json(AgentRunTimelineResponse {
                api_version: AGENT_RUNS_API_VERSION.to_owned(),
                task_uid,
                history: AgentRunDataStatus {
                    availability: if partial {
                        AgentRunAvailability::Partial
                    } else {
                        AgentRunAvailability::Available
                    },
                    source: "taskLifecycleEvents".to_owned(),
                    observed_at,
                    reason: partial.then(|| "backfilledHistory".to_owned()),
                },
                events: events.into_iter().map(agent_run_timeline_view).collect(),
            })
            .into_response()
        }
        Ok(None) => agent_run_error(StatusCode::NOT_FOUND, "agent run does not exist"),
        Err(_) => agent_run_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent-run persistence is unavailable",
        ),
    }
}

fn agent_run_view(record: AgentRunRecord) -> AgentRunView {
    let authority_observed_at = record.created_at.clone();
    let spend_observed_at = record.spend.as_ref().map(|spend| spend.observed_at.clone());
    let observed_spend = record.spend.as_ref().map(|spend| AgentRunSpendView {
        observed_amount: spend.observed_amount.clone(),
        currency: spend.currency.clone(),
        exhausted: spend.exhausted,
    });
    let unavailable = |reason: &str| AgentRunDataStatus {
        availability: AgentRunAvailability::Unavailable,
        source: "none".to_owned(),
        observed_at: None,
        reason: Some(reason.to_owned()),
    };
    AgentRunView {
        task_uid: record.task_uid,
        submitter_service: record.submitter_service,
        acting_user: record.acting_user,
        owner: record.owner,
        workflow: record.workflow,
        coding_agent_runtime: record.coding_agent_runtime,
        runtime_uid: record.runtime_uid,
        runtime_ownership: record.runtime_ownership,
        phase: record.phase,
        envelope_revision: record.envelope_revision,
        finalization_requested: record.finalize_requested,
        finalized: record.finalized,
        created_at: record.created_at.clone(),
        updated_at: record.updated_at.clone(),
        configured_models: record.runtime_spec.llms,
        granted_tools: record.runtime_spec.tools,
        allocated_budget: record.runtime_spec.budget,
        observed_spend,
        authority: AgentRunDataStatus {
            availability: if record.envelope_revision.is_some() {
                AgentRunAvailability::Available
            } else {
                AgentRunAvailability::Partial
            },
            source: "taskSubmissions.runtimeSpec".to_owned(),
            observed_at: Some(authority_observed_at),
            reason: record
                .envelope_revision
                .is_none()
                .then(|| "taskPredatesEnvelopeRevision".to_owned()),
        },
        spend: AgentRunDataStatus {
            availability: if spend_observed_at.is_some() {
                AgentRunAvailability::Available
            } else {
                AgentRunAvailability::Unavailable
            },
            source: "spendObservations".to_owned(),
            observed_at: spend_observed_at,
            reason: record
                .spend
                .is_none()
                .then(|| "noSpendObservation".to_owned()),
        },
        lifecycle: AgentRunDataStatus {
            availability: if record.history_partial {
                AgentRunAvailability::Partial
            } else {
                AgentRunAvailability::Available
            },
            source: "taskLifecycleEvents".to_owned(),
            observed_at: Some(record.updated_at),
            reason: record
                .history_partial
                .then(|| "backfilledHistory".to_owned()),
        },
        tool_activity: unavailable("notPersisted"),
        inference_activity: unavailable("notPersisted"),
        resources: unavailable("notPersisted"),
        github_run: unavailable("notRecorded"),
        error_category: bounded_task_error_category(record.failure_reason.as_deref())
            .map(str::to_owned),
    }
}

fn bounded_task_error_category(reason: Option<&str>) -> Option<&'static str> {
    reason.map(|reason| {
        if reason.starts_with("task output archive exceeds") {
            "output-limit"
        } else if reason.starts_with("sandbox does not support task operation") {
            "unsupported-operation"
        } else if reason == "sandbox task execution failed" {
            "sandbox-execution"
        } else {
            "execution-failed"
        }
    })
}

fn agent_run_timeline_view(event: AgentRunTimelineEvent) -> AgentRunTimelineEventView {
    let provenance = match event.provenance {
        AgentRunTimelineProvenance::Recorded => AgentRunTimelineProvenanceView::Recorded,
        AgentRunTimelineProvenance::Backfilled => AgentRunTimelineProvenanceView::Backfilled,
    };
    match event.kind {
        AgentRunTimelineKind::Phase(phase) => AgentRunTimelineEventView::Phase {
            phase,
            provenance,
            at: event.at,
        },
        AgentRunTimelineKind::FinalizationRequested => {
            AgentRunTimelineEventView::FinalizationRequested {
                provenance,
                at: event.at,
            }
        }
        AgentRunTimelineKind::Finalized => AgentRunTimelineEventView::Finalized {
            provenance,
            at: event.at,
        },
    }
}

fn agent_run_error(status: StatusCode, message: &'static str) -> Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

async fn authenticate_admission<A: RequestAuthenticator>(
    State(authenticator): State<A>,
    mut request: Request,
    next: Next,
) -> Response {
    let caller = match authenticate_request(&authenticator, request.headers()).await {
        Ok(caller) => caller,
        Err(response) => return response,
    };
    let roles = caller
        .member_roles
        .iter()
        .filter(|role| !role.is_empty())
        .cloned()
        .collect::<BTreeSet<_>>();
    let Some(member_role) = roles.first().filter(|_| roles.len() == 1) else {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "exactly one authenticated member role is required",
            })),
        )
            .into_response();
    };
    request.extensions_mut().insert(AdmissionContext {
        actor: caller.actor,
        member_role: member_role.clone(),
    });
    next.run(request).await
}

async fn authenticate_admin<A: RequestAuthenticator>(
    State(authenticator): State<A>,
    mut request: Request,
    next: Next,
) -> Response {
    let caller = match authenticate_request(&authenticator, request.headers()).await {
        Ok(caller) => caller,
        Err(response) => return response,
    };
    if !(caller.is_admin
        || caller.can_bootstrap_steward_run_service_envelope
            && request.method() == Method::POST
            && request.uri().path() == STEWARD_RUN_SERVICE_ENVELOPE_PATH)
    {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "authority for this administrator route is required",
            })),
        )
            .into_response();
    }
    request.extensions_mut().insert(AdminContext {
        actor: caller.actor,
    });
    next.run(request).await
}

async fn authenticate_request<A: RequestAuthenticator>(
    authenticator: &A,
    headers: &HeaderMap,
) -> Result<AuthenticatedCaller, Response> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| !token.is_empty())
        .ok_or_else(unauthorized)?;
    authenticator.authenticate(token).await.map_err(|error| {
        let status = match error {
            AuthenticationError::InvalidCredentials => StatusCode::UNAUTHORIZED,
            AuthenticationError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        };
        (
            status,
            Json(serde_json::json!({
                "error": "caller authentication failed",
            })),
        )
            .into_response()
    })
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(
            axum::http::header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Bearer"),
        )],
        Json(serde_json::json!({
            "error": "bearer authentication is required",
        })),
    )
        .into_response()
}

async fn budget_increase_handler<R, L, D>(
    State(state): State<AppState<R, L, D>>,
    Extension(context): Extension<AdmissionContext>,
    Path((namespace, name)): Path<(String, String)>,
    Json(edit): Json<BudgetIncrease>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel + Clone,
{
    match submit_budget_increase(
        &state.runtimes,
        &state.ledger,
        &state.decisions,
        &context,
        &namespace,
        &name,
        &edit,
    )
    .await
    {
        Ok(outcome @ SubmissionOutcome::Applied { .. }) => {
            (StatusCode::OK, Json(outcome)).into_response()
        }
        Ok(outcome @ SubmissionOutcome::Parked { .. }) => {
            (StatusCode::ACCEPTED, Json(outcome)).into_response()
        }
        Err(error) => error.into_response(),
    }
}

async fn create_runtime_handler<R, L, D>(
    State(state): State<AppState<R, L, D>>,
    Extension(context): Extension<AdmissionContext>,
    Path(namespace): Path<String>,
    Json(request): Json<CreateRuntimeRequest>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel + Clone,
{
    match submit_runtime_request(
        &state.runtimes,
        &state.ledger,
        &state.decisions,
        &context,
        &namespace,
        &request,
    )
    .await
    {
        Ok(outcome @ SubmissionOutcome::Applied { .. }) => {
            (StatusCode::CREATED, Json(outcome)).into_response()
        }
        Ok(outcome @ SubmissionOutcome::Parked { .. }) => {
            (StatusCode::ACCEPTED, Json(outcome)).into_response()
        }
        Err(error) => error.into_response(),
    }
}

async fn approval_queue_handler<R, L, D>(
    State(state): State<AppState<R, L, D>>,
    Extension(_admin): Extension<AdminContext>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel + Clone,
{
    match state.ledger.pending_approvals().await {
        Ok(approvals) => Html(render_approval_queue(&approvals)).into_response(),
        Err(error) => ApiError::Store(error).into_response(),
    }
}

async fn author_envelope_handler<R, L, D>(
    State(state): State<AppState<R, L, D>>,
    Extension(admin): Extension<AdminContext>,
    Path(member_role): Path<String>,
    Json(envelope): Json<Envelope>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel + Clone,
{
    if member_role.is_empty() || envelope.revision <= 0 {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "member role and positive envelope revision are required",
            })),
        )
            .into_response();
    }
    if let Err(error) = validate_envelope(&envelope) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": format!("invalid envelope: {error:?}"),
            })),
        )
            .into_response();
    }
    match state
        .ledger
        .insert_envelope(&member_role, &envelope, &admin.actor)
        .await
    {
        Ok(()) => StatusCode::CREATED.into_response(),
        Err(error) => ApiError::Store(error).into_response(),
    }
}

async fn author_service_envelope_handler<R, L, D>(
    State(state): State<AppState<R, L, D>>,
    Extension(admin): Extension<AdminContext>,
    Path(service): Path<String>,
    Json(envelope): Json<Envelope>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel + Clone,
{
    if service.is_empty() || envelope.revision <= 0 {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "service name and positive envelope revision are required",
            })),
        )
            .into_response();
    }
    if let Err(error) = validate_envelope(&envelope) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": format!("invalid envelope: {error:?}"),
            })),
        )
            .into_response();
    }
    match state.ledger.latest_service_envelope(&service).await {
        Ok(Some(current)) if current == envelope => return StatusCode::OK.into_response(),
        Ok(_) => {}
        Err(error) => return ApiError::Store(error).into_response(),
    }
    match state
        .ledger
        .insert_service_envelope(&service, &envelope, &admin.actor)
        .await
    {
        Ok(()) => StatusCode::CREATED.into_response(),
        Err(error) => ApiError::Store(error).into_response(),
    }
}

async fn approve_handler<R, L, D>(
    State(state): State<AppState<R, L, D>>,
    Extension(admin): Extension<AdminContext>,
    Path(approval_id): Path<Uuid>,
    Json(request): Json<ApprovalRequest>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel + Clone,
{
    match approve_parked_request(
        &state.runtimes,
        &state.ledger,
        &state.decisions,
        &admin,
        approval_id,
        &request,
    )
    .await
    {
        Ok(outcome) => (StatusCode::OK, Json(outcome)).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn revoke_grants_handler<R, L, D>(
    State(state): State<AppState<R, L, D>>,
    Extension(admin): Extension<AdminContext>,
    Path(runtime_uid): Path<String>,
    Json(request): Json<GrantRevocationRequest>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel + Clone,
{
    let revoked = match state
        .ledger
        .revoke_runtime_grants(&runtime_uid, &admin.actor, &request.reason)
        .await
    {
        Ok(revoked) => revoked,
        Err(error) => return ApiError::Store(error).into_response(),
    };
    if revoked == 0 {
        return ApiError::NoActiveGrants.into_response();
    }
    match reconcile_grant_reversion(&state.runtimes, &state.ledger, &runtime_uid).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => error.into_response(),
    }
}

async fn file_decision_handler<R, L, D>(
    State(state): State<AppState<R, L, D>>,
    Extension(_admin): Extension<AdminContext>,
    Path(approval_id): Path<Uuid>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel + Clone,
{
    match file_decision_reference(&state.ledger, &state.decisions, approval_id).await {
        Ok(reference) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "decisionKey": reference.key,
                "evidenceUrl": reference.evidence_url,
            })),
        )
            .into_response(),
        Err(error) => error.into_response(),
    }
}

fn render_approval_queue(approvals: &[PendingApproval]) -> String {
    let rows = approvals
        .iter()
        .map(|approval| {
            let counterexample = steward_admission::AdmissionDecision::Reject {
                deltas: approval.deltas.clone(),
            }
            .counterexample()
            .unwrap_or_else(|| "envelope exceeded".to_owned());
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                escape_html(&approval.runtime_uid),
                escape_html(&approval.member_role),
                escape_html(&approval.actor),
                escape_html(&counterexample),
                escape_html(approval.decision_key.as_deref().unwrap_or("unfiled")),
                escape_html(approval.evidence_url.as_deref().unwrap_or("unfiled")),
            )
        })
        .collect::<Vec<_>>()
        .join("");
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>Pending approvals</title></head>\
         <body><main><h1>Pending approvals</h1><table><thead><tr><th>Runtime UID</th><th>Member role</th>\
         <th>Actor</th><th>Counterexample</th><th>Decision key</th><th>Evidence</th></tr></thead>\
         <tbody>{rows}</tbody></table></main></body></html>"
    )
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::RuntimeCreate(RuntimeCreateError::Kubernetes { status, .. }) => {
                StatusCode::from_u16(*status).unwrap_or(StatusCode::SERVICE_UNAVAILABLE)
            }
            Self::PrincipalMismatch => StatusCode::FORBIDDEN,
            Self::TaskAuthentication => StatusCode::UNAUTHORIZED,
            Self::TaskAuthenticationUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::TaskWorkflowNotFound
            | Self::Store(
                StoreError::TaskNotFound
                | StoreError::CanonicalIdentityNotFound
                | StoreError::EnvelopeRequestNotFound,
            ) => StatusCode::NOT_FOUND,
            Self::Store(StoreError::CanonicalIdentityInactive) => StatusCode::FORBIDDEN,
            Self::TaskNotReady => StatusCode::SERVICE_UNAVAILABLE,
            Self::TaskOutputNotReady => StatusCode::CONFLICT,
            Self::MissingEnvelope | Self::MissingRuntimeUid => StatusCode::UNPROCESSABLE_ENTITY,
            Self::InvalidBudgetIncrease { .. } | Self::Admission(_) => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::NoActiveGrants => StatusCode::NOT_FOUND,
            Self::Store(StoreError::ApprovalNotFound) => StatusCode::NOT_FOUND,
            Self::Store(
                StoreError::ApprovalNotPending
                | StoreError::MissingDecisionReference
                | StoreError::DecisionReferenceMismatch
                | StoreError::DecisionFilingInProgress
                | StoreError::EvidenceMismatch
                | StoreError::StaleEnvelope
                | StoreError::EnvelopeRevisionNotIncreasing
                | StoreError::TaskIdempotencyConflict
                | StoreError::EnvelopeRequestIdempotencyConflict
                | StoreError::InvalidTaskTransition
                | StoreError::InvalidEnvelopeRequestTransition
                | StoreError::CanonicalIdentityStale
                | StoreError::CanonicalIdentityAmbiguousEmail
                | StoreError::CanonicalIdentityConflict,
            ) => StatusCode::CONFLICT,
            Self::Store(
                StoreError::InvalidGrantExpiry
                | StoreError::MissingRevocationReason
                | StoreError::CanonicalIdentityInvalidActor
                | StoreError::CanonicalIdentityInvalidRecord
                | StoreError::InvalidTaskIdentityBinding
                | StoreError::InvalidEnvelopeRequest,
            ) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Store(StoreError::InvalidRunQuery | StoreError::InvalidRunCursor) => {
                StatusCode::BAD_REQUEST
            }
            Self::RuntimeCreate(RuntimeCreateError::Unavailable(_))
            | Self::Runtime(_)
            | Self::Store(StoreError::Database(_) | StoreError::DecisionFilingClaimLost)
            | Self::DecisionChannel(_) => StatusCode::SERVICE_UNAVAILABLE,
        };
        (
            status,
            Json(serde_json::json!({
                "error": self.to_string(),
            })),
        )
            .into_response()
    }
}

pub async fn submit_budget_increase<R, L, D>(
    runtimes: &R,
    ledger: &L,
    decisions: &D,
    context: &AdmissionContext,
    namespace: &str,
    name: &str,
    edit: &BudgetIncrease,
) -> Result<SubmissionOutcome, ApiError>
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel,
{
    let runtime = runtimes
        .get(namespace, name)
        .await
        .map_err(ApiError::Runtime)?;
    match &runtime.spec.principal {
        Principal::User { acting_user } if acting_user.0 == context.actor => {}
        _ => return Err(ApiError::PrincipalMismatch),
    }
    let runtime_uid = runtime
        .metadata
        .uid
        .as_deref()
        .ok_or(ApiError::MissingRuntimeUid)?;
    let envelope = ledger
        .latest_envelope(&context.member_role)
        .await
        .map_err(ApiError::Store)?
        .ok_or(ApiError::MissingEnvelope)?;
    let mut proposed = runtime.clone();
    proposed.spec.budget.monthly_limit =
        add_budget_amount(&runtime.spec.budget.monthly_limit, &edit.amount).map_err(|_| {
            ApiError::InvalidBudgetIncrease {
                value: edit.amount.clone(),
            }
        })?;
    let grants = ledger
        .grants_for_runtime(runtime_uid, &context.member_role, envelope.revision)
        .await
        .map_err(ApiError::Store)?;
    let decision = evaluate_with_grants(&proposed.spec, &envelope, &grants)
        .map_err(|error| ApiError::Admission(format!("{error:?}")))?;
    match decision {
        AdmissionDecision::Admit => {
            runtimes
                .replace(&proposed, context)
                .await
                .map_err(ApiError::Runtime)?;
            Ok(SubmissionOutcome::Applied {
                proposed_spec: proposed.spec,
            })
        }
        AdmissionDecision::Reject { deltas } => {
            let counterexample = AdmissionDecision::Reject {
                deltas: deltas.clone(),
            }
            .counterexample()
            .ok_or_else(|| {
                ApiError::Admission("rejected decision did not carry a counterexample".to_owned())
            })?;
            let base_spec_digest = spec_digest(&runtime.spec)?;
            let spec_digest = spec_digest(&proposed.spec)?;
            let parked = ledger
                .park_rejection(ParkRejection {
                    runtime_uid,
                    runtime_namespace: namespace,
                    runtime_name: name,
                    spec_digest: &spec_digest,
                    base_spec_digest: &base_spec_digest,
                    base_pending_approval_digest: None,
                    base_spec: &runtime.spec,
                    envelope_revision: envelope.revision,
                    deltas: &deltas,
                    proposed_spec: &proposed.spec,
                    actor: &context.actor,
                    member_role: &context.member_role,
                })
                .await
                .map_err(ApiError::Store)?;
            let reference = match (parked.decision_key, parked.evidence_url) {
                (Some(key), Some(evidence_url)) => DecisionReference { key, evidence_url },
                (None, None) => {
                    file_decision_reference(ledger, decisions, parked.approval_id).await?
                }
                _ => {
                    return Err(ApiError::Conflict(
                        "parked approval has an incomplete decision reference".to_owned(),
                    ));
                }
            };
            Ok(SubmissionOutcome::Parked {
                approval_id: parked.approval_id,
                decision_id: parked.decision_id,
                decision_key: reference.key,
                evidence_url: reference.evidence_url,
                proposed_spec: proposed.spec,
                deltas,
                counterexample,
            })
        }
    }
}

pub async fn submit_runtime_request<R, L, D>(
    runtimes: &R,
    ledger: &L,
    decisions: &D,
    context: &AdmissionContext,
    namespace: &str,
    request: &CreateRuntimeRequest,
) -> Result<SubmissionOutcome, ApiError>
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel,
{
    if request.spec.canonical_authority.is_some() {
        return Err(ApiError::PrincipalMismatch);
    }
    match &request.spec.principal {
        Principal::User { acting_user } if acting_user.0 == context.actor => {}
        _ => return Err(ApiError::PrincipalMismatch),
    }
    let envelope = ledger
        .latest_envelope(&context.member_role)
        .await
        .map_err(ApiError::Store)?
        .ok_or(ApiError::MissingEnvelope)?;
    let decision = evaluate_with_grants(&request.spec, &envelope, &[])
        .map_err(|error| ApiError::Admission(format!("{error:?}")))?;
    let mut runtime = AgentRuntime::new(&request.name, request.spec.clone());
    runtime.metadata.namespace = Some(namespace.to_owned());
    runtime.metadata.annotations = Some(std::collections::BTreeMap::from([(
        "agents.apelogic.ai/member-role".to_owned(),
        context.member_role.clone(),
    )]));
    match decision {
        AdmissionDecision::Admit => {
            match runtimes.create(namespace, &runtime, context).await {
                Ok(_) => {}
                Err(RuntimeCreateError::Kubernetes { status: 409, .. }) => {
                    let existing = runtimes
                        .get(namespace, &request.name)
                        .await
                        .map_err(ApiError::Runtime)?;
                    let request_digest = spec_digest(&request.spec)?;
                    let annotations = existing.annotations();
                    let matching_actor = matches!(
                        &existing.spec.principal,
                        Principal::User { acting_user } if acting_user.0 == context.actor
                    );
                    if annotations
                        .get(PENDING_APPROVAL_ANNOTATION)
                        .map(String::as_str)
                        != Some(request_digest.as_str())
                        || annotations
                            .get("agents.apelogic.ai/member-role")
                            .map(String::as_str)
                            != Some(context.member_role.as_str())
                        || !matching_actor
                    {
                        return Err(ApiError::Conflict(
                            "an unrelated AgentRuntime already uses this name".to_owned(),
                        ));
                    }
                    let mut released = existing;
                    released.spec = request.spec.clone();
                    released
                        .metadata
                        .annotations
                        .get_or_insert_default()
                        .remove(PENDING_APPROVAL_ANNOTATION);
                    runtimes
                        .replace_as_authority(&released)
                        .await
                        .map_err(ApiError::Runtime)?;
                }
                Err(error) => return Err(ApiError::RuntimeCreate(error)),
            }
            Ok(SubmissionOutcome::Applied {
                proposed_spec: request.spec.clone(),
            })
        }
        AdmissionDecision::Reject { deltas } => {
            let counterexample = AdmissionDecision::Reject {
                deltas: deltas.clone(),
            }
            .counterexample()
            .ok_or_else(|| {
                ApiError::Admission("rejected decision did not carry a counterexample".to_owned())
            })?;
            let request_digest = spec_digest(&request.spec)?;
            runtime.spec.llms.clear();
            runtime.spec.tools.clear();
            runtime.spec.budget.monthly_limit = "0".to_owned();
            runtime.spec.budget.currency = envelope.spec.budget.currency.clone();
            runtime.spec.ttl = envelope.spec.ttl.clone();
            runtime.spec.bindings = None;
            runtime.metadata.annotations.get_or_insert_default().insert(
                PENDING_APPROVAL_ANNOTATION.to_owned(),
                request_digest.clone(),
            );
            let created = match runtimes.create_as_authority(namespace, &runtime).await {
                Ok(created) => created,
                Err(RuntimeCreateError::Kubernetes { status: 409, .. }) => {
                    let existing = runtimes
                        .get(namespace, &request.name)
                        .await
                        .map_err(ApiError::Runtime)?;
                    let annotations = existing.annotations();
                    if annotations
                        .get(PENDING_APPROVAL_ANNOTATION)
                        .map(String::as_str)
                        != Some(request_digest.as_str())
                        || annotations
                            .get("agents.apelogic.ai/member-role")
                            .map(String::as_str)
                            != Some(context.member_role.as_str())
                        || existing.spec != runtime.spec
                    {
                        return Err(ApiError::Conflict(
                            "an unrelated AgentRuntime already uses this name".to_owned(),
                        ));
                    }
                    existing
                }
                Err(error) => return Err(ApiError::RuntimeCreate(error)),
            };
            let runtime_uid = created
                .metadata
                .uid
                .as_deref()
                .ok_or(ApiError::MissingRuntimeUid)?;
            let base_spec_digest = spec_digest(&created.spec)?;
            let parked = ledger
                .park_rejection(ParkRejection {
                    runtime_uid,
                    runtime_namespace: namespace,
                    runtime_name: &request.name,
                    spec_digest: &request_digest,
                    base_spec_digest: &base_spec_digest,
                    base_pending_approval_digest: Some(&request_digest),
                    base_spec: &created.spec,
                    envelope_revision: envelope.revision,
                    deltas: &deltas,
                    proposed_spec: &request.spec,
                    actor: &context.actor,
                    member_role: &context.member_role,
                })
                .await
                .map_err(ApiError::Store)?;
            if let Some(outcome) = converge_superseded_create(
                runtimes,
                ledger,
                context,
                namespace,
                request,
                &created,
                parked.approval_id,
            )
            .await?
            {
                return Ok(outcome);
            }
            let reference = match (parked.decision_key, parked.evidence_url) {
                (Some(key), Some(evidence_url)) => DecisionReference { key, evidence_url },
                (None, None) => {
                    file_decision_reference(ledger, decisions, parked.approval_id).await?
                }
                _ => {
                    return Err(ApiError::Conflict(
                        "parked approval has an incomplete decision reference".to_owned(),
                    ));
                }
            };
            if let Some(outcome) = converge_superseded_create(
                runtimes,
                ledger,
                context,
                namespace,
                request,
                &created,
                parked.approval_id,
            )
            .await?
            {
                return Ok(outcome);
            }
            Ok(SubmissionOutcome::Parked {
                approval_id: parked.approval_id,
                decision_id: parked.decision_id,
                decision_key: reference.key,
                evidence_url: reference.evidence_url,
                proposed_spec: request.spec.clone(),
                deltas,
                counterexample,
            })
        }
    }
}

async fn converge_superseded_create<R, L>(
    runtimes: &R,
    ledger: &L,
    context: &AdmissionContext,
    namespace: &str,
    request: &CreateRuntimeRequest,
    placeholder: &AgentRuntime,
    parked_approval_id: Uuid,
) -> Result<Option<SubmissionOutcome>, ApiError>
where
    R: RuntimeRepository,
    L: AdmissionLedger,
{
    let runtime_uid = placeholder
        .metadata
        .uid
        .as_deref()
        .ok_or(ApiError::MissingRuntimeUid)?;
    let Some(application) = ledger
        .grant_application(runtime_uid)
        .await
        .map_err(ApiError::Store)?
    else {
        return Ok(None);
    };
    validate_approved_create(
        context,
        namespace,
        request,
        placeholder,
        &application.application,
    )?;
    let Some(active_application) = ledger
        .retire_pending_approval_if_superseded(
            parked_approval_id,
            application.approval_id,
            runtime_uid,
            "steward-apiserver",
            "superseded by an active approval during create convergence",
        )
        .await
        .map_err(ApiError::Store)?
    else {
        return Ok(None);
    };
    converge_approved_create(
        runtimes,
        context,
        namespace,
        request,
        placeholder,
        active_application,
    )
    .await
    .map(Some)
}

async fn converge_approved_create<R: RuntimeRepository>(
    runtimes: &R,
    context: &AdmissionContext,
    namespace: &str,
    request: &CreateRuntimeRequest,
    placeholder: &AgentRuntime,
    application: GrantReversion,
) -> Result<SubmissionOutcome, ApiError> {
    validate_approved_create(context, namespace, request, placeholder, &application)?;
    let mut restored = placeholder.clone();
    restored.spec = application.proposed_spec;
    restored
        .metadata
        .annotations
        .get_or_insert_default()
        .remove(PENDING_APPROVAL_ANNOTATION);
    runtimes
        .replace_as_authority(&restored)
        .await
        .map_err(ApiError::Runtime)?;
    Ok(SubmissionOutcome::Applied {
        proposed_spec: request.spec.clone(),
    })
}

fn validate_approved_create(
    context: &AdmissionContext,
    namespace: &str,
    request: &CreateRuntimeRequest,
    placeholder: &AgentRuntime,
    application: &GrantReversion,
) -> Result<(), ApiError> {
    let runtime_uid = placeholder
        .metadata
        .uid
        .as_deref()
        .ok_or(ApiError::MissingRuntimeUid)?;
    let pending_digest = placeholder
        .annotations()
        .get(PENDING_APPROVAL_ANNOTATION)
        .map(String::as_str);
    let proposed_digest = spec_digest(&application.proposed_spec)?;
    let bound_role = placeholder
        .annotations()
        .get("agents.apelogic.ai/member-role")
        .map(String::as_str);
    let proposed_actor = match &application.proposed_spec.principal {
        Principal::User { acting_user } => acting_user.0.as_str(),
        _ => "",
    };
    if application.runtime_uid != runtime_uid
        || application.runtime_namespace != namespace
        || application.runtime_name != request.name
        || application.actor != context.actor
        || application.member_role != context.member_role
        || application.base_spec != placeholder.spec
        || application.proposed_spec != request.spec
        || application.base_pending_approval_digest.as_deref() != pending_digest
        || pending_digest != Some(proposed_digest.as_str())
        || bound_role != Some(application.member_role.as_str())
        || proposed_actor != application.actor
    {
        return Err(ApiError::Conflict(
            "active approved application does not match this create request".to_owned(),
        ));
    }
    Ok(())
}

pub async fn approve_parked_request<R, L, D>(
    runtimes: &R,
    ledger: &L,
    decisions: &D,
    admin: &AdminContext,
    approval_id: Uuid,
    request: &ApprovalRequest,
) -> Result<SubmissionOutcome, ApiError>
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel,
{
    if request.rationale.is_empty()
        || request.evidence_url.is_empty()
        || request.expires_at.is_empty()
    {
        return Err(ApiError::Admission(
            "approval rationale, evidence URL, and grant expiry are required".to_owned(),
        ));
    }
    let candidate = ledger
        .approval_candidate(approval_id, &request.evidence_url)
        .await
        .map_err(ApiError::Store)?;
    let scope_kind = approval_scope_kind(&candidate.proposed_spec, &candidate.member_role)?;
    let latest_envelope = match scope_kind {
        EnvelopeScopeKind::MemberRole => ledger.latest_envelope(&candidate.member_role).await,
        EnvelopeScopeKind::Service => ledger.latest_service_envelope(&candidate.member_role).await,
    }
    .map_err(ApiError::Store)?
    .ok_or(ApiError::MissingEnvelope)?;
    if latest_envelope.revision != candidate.envelope_revision {
        return Err(ApiError::Conflict(
            "approval envelope is no longer current".to_owned(),
        ));
    }
    let mut runtime = runtimes
        .get_bound(
            &candidate.runtime_namespace,
            &candidate.runtime_name,
            &candidate.runtime_uid,
        )
        .await
        .map_err(ApiError::Runtime)?;
    if runtime.metadata.uid.as_deref() != Some(candidate.runtime_uid.as_str()) {
        return Err(ApiError::Runtime(
            "runtime repository returned a different runtime UID".to_owned(),
        ));
    }
    let pending_digest = runtime
        .annotations()
        .get(PENDING_APPROVAL_ANNOTATION)
        .map(String::as_str);
    let proposed_digest = spec_digest(&candidate.proposed_spec)?;
    let proposed_actor = principal_actor(&candidate.proposed_spec);
    let bound_scope = match scope_kind {
        EnvelopeScopeKind::MemberRole => runtime
            .annotations()
            .get("agents.apelogic.ai/member-role")
            .map(String::as_str),
        EnvelopeScopeKind::Service => runtime
            .annotations()
            .get("agents.apelogic.ai/service-principal")
            .map(String::as_str),
    };
    if pending_digest != candidate.base_pending_approval_digest.as_deref()
        || candidate
            .base_pending_approval_digest
            .as_deref()
            .is_some_and(|digest| digest != proposed_digest)
        || bound_scope != Some(candidate.member_role.as_str())
        || proposed_actor != candidate.actor
    {
        return Err(ApiError::Conflict(
            "parked approval provenance does not match the bound runtime".to_owned(),
        ));
    }
    let current_digest = spec_digest(&runtime.spec)?;
    let already_applied = runtime.spec == candidate.proposed_spec;
    if !already_applied && current_digest != candidate.base_spec_digest {
        return Err(ApiError::Conflict(
            "runtime changed after the approval request was parked".to_owned(),
        ));
    }
    let approved = ledger
        .approve_admission(ApproveAdmission {
            approval_id,
            decided_by: &admin.actor,
            rationale: &request.rationale,
            evidence_url: &request.evidence_url,
            expires_at: &request.expires_at,
        })
        .await
        .map_err(ApiError::Store)?;
    let pending_annotation_removed = runtime
        .metadata
        .annotations
        .get_or_insert_default()
        .remove(PENDING_APPROVAL_ANNOTATION)
        .is_some();
    if !already_applied || pending_annotation_removed {
        runtime.spec = approved.proposed_spec.clone();
        if pending_annotation_removed
            || matches!(&approved.proposed_spec.principal, Principal::Service { .. })
        {
            runtimes
                .replace_as_authority(&runtime)
                .await
                .map_err(ApiError::Runtime)?;
        } else {
            runtimes
                .replace(
                    &runtime,
                    &AdmissionContext {
                        actor: approved.actor.clone(),
                        member_role: approved.member_role.clone(),
                    },
                )
                .await
                .map_err(ApiError::Runtime)?;
        }
    }
    decisions
        .record_resolution(&DecisionResolution {
            request_id: approval_id.to_string(),
            key: approved.decision_key,
            decided_by: approved.decided_by,
            rationale: approved.rationale,
            evidence_url: approved.evidence_url,
        })
        .await
        .map_err(|error| ApiError::DecisionChannel(format!("{error:?}")))?;
    Ok(SubmissionOutcome::Applied {
        proposed_spec: approved.proposed_spec,
    })
}

fn principal_actor(spec: &AgentRuntimeSpec) -> &str {
    match &spec.principal {
        Principal::User { acting_user } => &acting_user.0,
        Principal::Service { name, .. } => name,
    }
}

fn approval_scope_kind(
    spec: &AgentRuntimeSpec,
    scope_ref: &str,
) -> Result<EnvelopeScopeKind, ApiError> {
    match &spec.principal {
        Principal::User { .. } => Ok(EnvelopeScopeKind::MemberRole),
        Principal::Service { name, .. } if name == scope_ref => Ok(EnvelopeScopeKind::Service),
        Principal::Service { .. } => Err(ApiError::Conflict(
            "service approval scope does not match its principal name".to_owned(),
        )),
    }
}

async fn file_decision_reference<L, D>(
    ledger: &L,
    decisions: &D,
    approval_id: Uuid,
) -> Result<DecisionReference, ApiError>
where
    L: AdmissionLedger,
    D: DecisionChannel,
{
    let claim = ledger
        .claim_decision_filing(approval_id)
        .await
        .map_err(ApiError::Store)?;
    let filing = claim.filing;
    if let (Some(key), Some(evidence_url)) = (filing.decision_key, filing.evidence_url) {
        return Ok(DecisionReference { key, evidence_url });
    }
    let token = claim
        .token
        .ok_or(ApiError::Store(StoreError::DecisionFilingClaimLost))?;
    let counterexample = AdmissionDecision::Reject {
        deltas: filing.deltas,
    }
    .counterexample()
    .ok_or_else(|| {
        ApiError::Admission("parked decision did not carry a counterexample".to_owned())
    })?;
    let reference = match decisions
        .request(&DecisionRequest {
            request_id: filing.approval_id.to_string(),
            runtime_uid: filing.runtime_uid,
            actor: filing.actor,
            member_role: filing.member_role,
            counterexample,
        })
        .await
    {
        Ok(reference) => reference,
        Err(error) => {
            ledger
                .release_decision_filing(approval_id, token)
                .await
                .map_err(ApiError::Store)?;
            return Err(ApiError::DecisionChannel(format!("{error:?}")));
        }
    };
    ledger
        .complete_decision_filing(approval_id, token, &reference.key, &reference.evidence_url)
        .await
        .map_err(ApiError::Store)?;
    Ok(reference)
}

async fn reconcile_grant_reversion<R, L>(
    runtimes: &R,
    ledger: &L,
    runtime_uid: &str,
) -> Result<(), ApiError>
where
    R: RuntimeRepository,
    L: AdmissionLedger,
{
    let Some(reversion) = ledger
        .grant_reversion(runtime_uid)
        .await
        .map_err(ApiError::Store)?
    else {
        return Ok(());
    };
    let mut runtime = runtimes
        .get_bound(
            &reversion.runtime_namespace,
            &reversion.runtime_name,
            &reversion.runtime_uid,
        )
        .await
        .map_err(ApiError::Runtime)?;
    if runtime.spec == reversion.proposed_spec {
        runtime.spec = reversion.base_spec;
        if let Some(pending_digest) = reversion.base_pending_approval_digest {
            runtime
                .metadata
                .annotations
                .get_or_insert_default()
                .insert(PENDING_APPROVAL_ANNOTATION.to_owned(), pending_digest);
            runtimes
                .replace_as_authority(&runtime)
                .await
                .map_err(ApiError::Runtime)?;
        } else if matches!(&runtime.spec.principal, Principal::Service { .. }) {
            runtimes
                .replace_as_authority(&runtime)
                .await
                .map_err(ApiError::Runtime)?;
        } else {
            runtimes
                .replace(
                    &runtime,
                    &AdmissionContext {
                        actor: reversion.actor,
                        member_role: reversion.member_role,
                    },
                )
                .await
                .map_err(ApiError::Runtime)?;
        }
    }
    Ok(())
}

fn spec_digest(spec: &AgentRuntimeSpec) -> Result<String, ApiError> {
    let serialized =
        serde_json::to_vec(spec).map_err(|error| ApiError::Admission(error.to_string()))?;
    Ok(Sha256::digest(serialized)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use k8s_openapi::api::authentication::v1::{TokenReviewStatus, UserInfo};
    use kube::ResourceExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration as StdDuration;

    use steward_admission::{
        AdmissionDecision, AdmissionDelta, Envelope, EnvelopeScopeKind, EnvelopeSpec,
    };
    use steward_ports::{
        DecisionChannel, DecisionReference, DecisionRequest, DecisionResolution, PortError,
    };
    use steward_store::{
        AgentRunPage, AgentRunQuery, AgentRunRecord, AgentRunSpend, AgentRunTimelineEvent,
        AgentRunTimelineKind, AgentRunTimelineProvenance, ApprovalCandidate, ApproveAdmission,
        ApprovedAdmission, DecisionFiling, DecisionFilingClaim, GrantApplication, GrantReversion,
        ParkRejection, ParkedAdmission, PendingApproval, StoreError, TaskRecord, TaskReservation,
        TaskReservationRequest,
    };
    use steward_types::{
        AgentRuntime, AgentRuntimeSpec, AgentType, Budget, CanonicalUserId, Duration, Email,
        ModelRef, PENDING_APPROVAL_ANNOTATION, Principal, TaskPhase,
    };
    use tower::ServiceExt;
    use utoipa::OpenApi;
    use uuid::Uuid;

    use super::tasks::task_identity_from_token_review;
    use super::{
        AGENT_RUNS_API_VERSION, AdmissionContext, AdmissionLedger, AgentRunLedger, ApiDoc,
        ApiError, AuthenticatedCaller, AuthenticationError, BoxFuture, BudgetIncrease,
        CreateRuntimeRequest, KubernetesTokenReviewAudience, RequestAuthenticator,
        RuntimeCreateError, RuntimeRepository, STEWARD_RUN_SERVICE_ENVELOPE_BOOTSTRAP_GROUP,
        StaticTaskWorkflowCatalog, SubmissionOutcome, TaskAuthenticationError, TaskIdentity,
        TaskIdentityResolver, TaskSubmissionLedger, TaskSubmissionRequest, TaskWorkflow,
        caller_from_kubernetes_user, caller_from_token_review, router, spec_digest,
        submit_budget_increase, task_router, token_review_request,
    };

    const KUBERNETES_TOKEN_REVIEW_AUDIENCE: &str = "https://kubernetes.default.svc";

    #[derive(Clone)]
    struct FakeAuthenticator;

    #[derive(Clone)]
    struct BootstrapAuthenticator;

    #[derive(Clone)]
    struct FakeTaskIdentityResolver;

    impl TaskIdentityResolver for FakeTaskIdentityResolver {
        fn resolve<'a>(
            &'a self,
            assertion: &'a str,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<TaskIdentity, TaskAuthenticationError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                match assertion {
                    "github-assertion" => Ok(TaskIdentity {
                        service: "steward-run".to_owned(),
                        acting_user: Some(Email("alice@example.com".to_owned())),
                        owner: Email("alice@example.com".to_owned()),
                        canonical_user_id: CanonicalUserId::parse(
                            "usr_0123456789abcdef0123456789abcdef",
                        )
                        .map_err(|_| TaskAuthenticationError::InvalidCredentials)?,
                    }),
                    "github-bob-assertion" => Ok(TaskIdentity {
                        service: "steward-run".to_owned(),
                        acting_user: Some(Email("bob@example.org".to_owned())),
                        owner: Email("bob@example.org".to_owned()),
                        canonical_user_id: CanonicalUserId::parse(
                            "usr_abcdef0123456789abcdef0123456789",
                        )
                        .map_err(|_| TaskAuthenticationError::InvalidCredentials)?,
                    }),
                    "github-renamed-assertion" => Ok(TaskIdentity {
                        service: "steward-run".to_owned(),
                        acting_user: Some(Email("alice-renamed@example.com".to_owned())),
                        owner: Email("alice-renamed@example.com".to_owned()),
                        canonical_user_id: CanonicalUserId::parse(
                            "usr_0123456789abcdef0123456789abcdef",
                        )
                        .map_err(|_| TaskAuthenticationError::InvalidCredentials)?,
                    }),
                    "scheduled-assertion" => Ok(TaskIdentity {
                        service: "scheduled-scanner".to_owned(),
                        acting_user: None,
                        owner: Email("owner@example.org".to_owned()),
                        canonical_user_id: CanonicalUserId::parse(
                            "usr_456789abcdef0123456789abcdef0123",
                        )
                        .map_err(|_| TaskAuthenticationError::InvalidCredentials)?,
                    }),
                    _ => Err(TaskAuthenticationError::InvalidCredentials),
                }
            })
        }
    }

    #[test]
    fn kubernetes_identity_is_the_only_source_of_roles_and_admin_authority() -> Result<(), String> {
        let caller = caller_from_kubernetes_user(
            &UserInfo {
                username: Some("alice@example.com".to_owned()),
                groups: Some(vec![
                    "agents.apelogic.ai/member-role:engineer".to_owned(),
                    "agents.apelogic.ai/member-role:engineer".to_owned(),
                    "agents.apelogic.ai/admin".to_owned(),
                ]),
                ..UserInfo::default()
            },
            "agents.apelogic.ai/admin",
        )
        .map_err(|error| format!("authenticated Kubernetes user was rejected: {error:?}"))?;
        assert_eq!(caller.actor, "alice@example.com");
        assert_eq!(caller.member_roles, vec!["engineer"]);
        assert!(caller.is_admin);
        assert_eq!(
            caller_from_kubernetes_user(
                &UserInfo {
                    groups: Some(vec!["agents.apelogic.ai/admin".to_owned()]),
                    ..UserInfo::default()
                },
                "agents.apelogic.ai/admin",
            ),
            Err(AuthenticationError::InvalidCredentials),
            "a group without an authenticated Kubernetes username is not a caller"
        );
        Ok(())
    }

    #[test]
    fn kubernetes_token_audience_must_match_the_configured_api_audience() {
        let status = TokenReviewStatus {
            authenticated: Some(true),
            audiences: Some(vec![KUBERNETES_TOKEN_REVIEW_AUDIENCE.to_owned()]),
            user: Some(UserInfo {
                username: Some("alice@example.com".to_owned()),
                groups: Some(vec!["agents.apelogic.ai/member-role:engineer".to_owned()]),
                ..UserInfo::default()
            }),
            ..TokenReviewStatus::default()
        };
        assert!(
            caller_from_token_review(
                Some(status.clone()),
                "agents.apelogic.ai/admin",
                KUBERNETES_TOKEN_REVIEW_AUDIENCE,
            )
            .is_ok()
        );
        assert_eq!(
            caller_from_token_review(Some(status), "agents.apelogic.ai/admin", "other-api"),
            Err(AuthenticationError::InvalidCredentials),
            "a valid Kubernetes token for another audience must fail closed"
        );

        for (case, status) in [
            (
                "unauthenticated result",
                Some(TokenReviewStatus {
                    authenticated: Some(false),
                    audiences: Some(vec![KUBERNETES_TOKEN_REVIEW_AUDIENCE.to_owned()]),
                    user: Some(UserInfo {
                        username: Some("alice@example.com".to_owned()),
                        ..UserInfo::default()
                    }),
                    ..TokenReviewStatus::default()
                }),
            ),
            (
                "missing audience result",
                Some(TokenReviewStatus {
                    authenticated: Some(true),
                    user: Some(UserInfo {
                        username: Some("alice@example.com".to_owned()),
                        ..UserInfo::default()
                    }),
                    ..TokenReviewStatus::default()
                }),
            ),
            (
                "empty audience result",
                Some(TokenReviewStatus {
                    authenticated: Some(true),
                    audiences: Some(Vec::new()),
                    user: Some(UserInfo {
                        username: Some("alice@example.com".to_owned()),
                        ..UserInfo::default()
                    }),
                    ..TokenReviewStatus::default()
                }),
            ),
            (
                "missing user result",
                Some(TokenReviewStatus {
                    authenticated: Some(true),
                    audiences: Some(vec![KUBERNETES_TOKEN_REVIEW_AUDIENCE.to_owned()]),
                    ..TokenReviewStatus::default()
                }),
            ),
        ] {
            assert_eq!(
                caller_from_token_review(
                    status,
                    "agents.apelogic.ai/admin",
                    KUBERNETES_TOKEN_REVIEW_AUDIENCE,
                ),
                Err(AuthenticationError::InvalidCredentials),
                "{case} must fail closed"
            );
        }
    }

    #[test]
    fn delegated_token_review_request_uses_the_exact_kubernetes_audience() -> Result<(), String> {
        for invalid in [String::new(), "   ".to_owned()] {
            assert!(
                KubernetesTokenReviewAudience::new(invalid).is_err(),
                "missing delegated audience configuration must fail closed"
            );
        }
        let audience =
            KubernetesTokenReviewAudience::new(KUBERNETES_TOKEN_REVIEW_AUDIENCE.to_owned())
                .map_err(str::to_owned)?;
        let review = token_review_request("opaque-exchanged-token", &audience);
        assert_eq!(
            review.spec.audiences,
            Some(vec![KUBERNETES_TOKEN_REVIEW_AUDIENCE.to_owned()])
        );
        assert_eq!(review.spec.token.as_deref(), Some("opaque-exchanged-token"));
        Ok(())
    }

    #[test]
    fn service_envelope_bootstrap_identity_is_exact_and_audience_bound() -> Result<(), String> {
        let status = TokenReviewStatus {
            authenticated: Some(true),
            audiences: Some(vec![KUBERNETES_TOKEN_REVIEW_AUDIENCE.to_owned()]),
            user: Some(UserInfo {
                username: Some("bootstrap@example.com".to_owned()),
                groups: Some(vec![
                    STEWARD_RUN_SERVICE_ENVELOPE_BOOTSTRAP_GROUP.to_owned(),
                ]),
                ..UserInfo::default()
            }),
            ..TokenReviewStatus::default()
        };
        let caller = caller_from_token_review(
            Some(status.clone()),
            "agents.apelogic.ai/admin",
            KUBERNETES_TOKEN_REVIEW_AUDIENCE,
        )
        .map_err(|error| format!("route-scoped bootstrap identity was rejected: {error:?}"))?;
        assert_eq!(caller.actor, "bootstrap@example.com");
        assert!(!caller.is_admin);
        assert!(caller.can_bootstrap_steward_run_service_envelope);

        assert_eq!(
            caller_from_token_review(
                Some(status.clone()),
                "agents.apelogic.ai/admin",
                "other-api",
            ),
            Err(AuthenticationError::InvalidCredentials),
            "bootstrap authority issued for another audience must fail closed"
        );

        let mut duplicate = status.clone();
        duplicate
            .user
            .as_mut()
            .and_then(|user| user.groups.as_mut())
            .ok_or_else(|| "bootstrap test identity groups are missing".to_owned())?
            .push(STEWARD_RUN_SERVICE_ENVELOPE_BOOTSTRAP_GROUP.to_owned());
        assert_eq!(
            caller_from_token_review(
                Some(duplicate),
                "agents.apelogic.ai/admin",
                KUBERNETES_TOKEN_REVIEW_AUDIENCE,
            ),
            Err(AuthenticationError::InvalidCredentials),
            "duplicate bootstrap authority groups must fail closed"
        );

        for contradictory_group in [
            "agents.apelogic.ai/admin",
            "agents.apelogic.ai/member-role:engineer",
        ] {
            let mut contradictory = status.clone();
            contradictory
                .user
                .as_mut()
                .and_then(|user| user.groups.as_mut())
                .ok_or_else(|| "bootstrap test identity groups are missing".to_owned())?
                .push(contradictory_group.to_owned());
            assert_eq!(
                caller_from_token_review(
                    Some(contradictory),
                    "agents.apelogic.ai/admin",
                    KUBERNETES_TOKEN_REVIEW_AUDIENCE,
                ),
                Err(AuthenticationError::InvalidCredentials),
                "bootstrap authority combined with {contradictory_group} must fail closed"
            );
        }

        for groups in [
            Vec::new(),
            vec!["agents.apelogic.ai/service-envelope-bootstrap:other-service".to_owned()],
        ] {
            let mut unauthorized = status.clone();
            unauthorized
                .user
                .as_mut()
                .ok_or_else(|| "bootstrap test identity is missing".to_owned())?
                .groups = Some(groups);
            let caller = caller_from_token_review(
                Some(unauthorized),
                "agents.apelogic.ai/admin",
                KUBERNETES_TOKEN_REVIEW_AUDIENCE,
            )
            .map_err(|error| {
                format!("authenticated unprivileged identity was rejected: {error:?}")
            })?;
            assert!(
                !caller.can_bootstrap_steward_run_service_envelope,
                "missing or wrong-service bootstrap group must confer no authority"
            );
        }
        Ok(())
    }

    #[test]
    fn task_identity_is_resolved_only_from_authenticated_token_review_attributes()
    -> Result<(), String> {
        let delegated = task_identity_from_token_review(
            Some(TokenReviewStatus {
                authenticated: Some(true),
                audiences: Some(vec![KUBERNETES_TOKEN_REVIEW_AUDIENCE.to_owned()]),
                user: Some(UserInfo {
                    username: Some("alice@example.com".to_owned()),
                    groups: Some(vec![
                        "agents.apelogic.ai/service-principal:steward-run".to_owned(),
                        "agents.apelogic.ai/acting-user:alice@example.com".to_owned(),
                        "agents.apelogic.ai/canonical-user:usr_0123456789abcdef0123456789abcdef"
                            .to_owned(),
                    ]),
                    ..UserInfo::default()
                }),
                ..TokenReviewStatus::default()
            }),
            KUBERNETES_TOKEN_REVIEW_AUDIENCE,
        )
        .map_err(|error| format!("server-validated delegated assertion was rejected: {error:?}"))?;
        assert_eq!(delegated.service, "steward-run");
        assert_eq!(
            delegated.acting_user,
            Some(Email("alice@example.com".to_owned()))
        );
        assert_eq!(delegated.owner, Email("alice@example.com".to_owned()));
        assert_eq!(
            delegated.canonical_user_id.as_str(),
            "usr_0123456789abcdef0123456789abcdef"
        );

        let scheduled = task_identity_from_token_review(
            Some(TokenReviewStatus {
                authenticated: Some(true),
                audiences: Some(vec![KUBERNETES_TOKEN_REVIEW_AUDIENCE.to_owned()]),
                user: Some(UserInfo {
                    username: Some("scheduler:trigger-123".to_owned()),
                    groups: Some(vec![
                        "agents.apelogic.ai/service-principal:scheduled-scanner".to_owned(),
                        "agents.apelogic.ai/task-owner:owner@example.org".to_owned(),
                        "agents.apelogic.ai/canonical-user:usr_abcdef0123456789abcdef0123456789"
                            .to_owned(),
                    ]),
                    ..UserInfo::default()
                }),
                ..TokenReviewStatus::default()
            }),
            KUBERNETES_TOKEN_REVIEW_AUDIENCE,
        )
        .map_err(|error| format!("server-validated scheduled assertion was rejected: {error:?}"))?;
        assert_eq!(scheduled.service, "scheduled-scanner");
        assert_eq!(scheduled.acting_user, None);
        assert_eq!(scheduled.owner, Email("owner@example.org".to_owned()));
        assert_eq!(
            scheduled.canonical_user_id.as_str(),
            "usr_abcdef0123456789abcdef0123456789"
        );

        let self_asserted_only = task_identity_from_token_review(
            Some(TokenReviewStatus {
                authenticated: Some(true),
                audiences: Some(vec![KUBERNETES_TOKEN_REVIEW_AUDIENCE.to_owned()]),
                user: Some(UserInfo {
                    username: Some("alice@example.com".to_owned()),
                    groups: Some(Vec::new()),
                    ..UserInfo::default()
                }),
                ..TokenReviewStatus::default()
            }),
            KUBERNETES_TOKEN_REVIEW_AUDIENCE,
        );
        assert_eq!(
            self_asserted_only,
            Err(TaskAuthenticationError::InvalidCredentials),
            "an authenticated username without a server-resolved service binding is not a task principal"
        );
        Ok(())
    }

    #[test]
    fn task_identity_rejects_duplicate_token_review_groups() {
        for duplicate_group in [
            "agents.apelogic.ai/service-principal:steward-run",
            "agents.apelogic.ai/acting-user:alice@example.com",
            "agents.apelogic.ai/canonical-user:usr_0123456789abcdef0123456789abcdef",
        ] {
            let identity = task_identity_from_token_review(
                Some(TokenReviewStatus {
                    authenticated: Some(true),
                    audiences: Some(vec![KUBERNETES_TOKEN_REVIEW_AUDIENCE.to_owned()]),
                    user: Some(UserInfo {
                        username: Some("alice@example.com".to_owned()),
                        groups: Some(vec![
                            "agents.apelogic.ai/service-principal:steward-run".to_owned(),
                            "agents.apelogic.ai/acting-user:alice@example.com".to_owned(),
                            "agents.apelogic.ai/canonical-user:usr_0123456789abcdef0123456789abcdef"
                                .to_owned(),
                            duplicate_group.to_owned(),
                        ]),
                        ..UserInfo::default()
                    }),
                    ..TokenReviewStatus::default()
                }),
                KUBERNETES_TOKEN_REVIEW_AUDIENCE,
            );
            assert_eq!(
                identity,
                Err(TaskAuthenticationError::InvalidCredentials),
                "duplicate identity group {duplicate_group} must fail closed"
            );
        }
    }

    #[test]
    fn task_identity_binds_acting_user_to_the_verified_username() {
        for (username, acting_user) in [
            ("github-assertion:job-123", "alice@example.com"),
            ("bob@example.org", "alice@example.com"),
        ] {
            let identity = task_identity_from_token_review(
                Some(TokenReviewStatus {
                    authenticated: Some(true),
                    audiences: Some(vec![KUBERNETES_TOKEN_REVIEW_AUDIENCE.to_owned()]),
                    user: Some(UserInfo {
                        username: Some(username.to_owned()),
                        groups: Some(vec![
                            "agents.apelogic.ai/service-principal:steward-run".to_owned(),
                            format!("agents.apelogic.ai/acting-user:{acting_user}"),
                            "agents.apelogic.ai/canonical-user:usr_0123456789abcdef0123456789abcdef"
                                .to_owned(),
                        ]),
                        ..UserInfo::default()
                    }),
                    ..TokenReviewStatus::default()
                }),
                KUBERNETES_TOKEN_REVIEW_AUDIENCE,
            );
            assert_eq!(
                identity,
                Err(TaskAuthenticationError::InvalidCredentials),
                "acting-user {acting_user} must equal the verified corporate username {username}"
            );
        }
    }

    #[test]
    fn task_identity_fails_closed_on_incomplete_or_misdirected_exchange_results() {
        let cases = [
            (
                "wrong audience",
                vec![
                    "agents.apelogic.ai/service-principal:steward-run".to_owned(),
                    "agents.apelogic.ai/acting-user:alice@example.com".to_owned(),
                    "agents.apelogic.ai/canonical-user:usr_0123456789abcdef0123456789abcdef"
                        .to_owned(),
                ],
                Some(vec!["other-api".to_owned()]),
            ),
            (
                "missing audience",
                vec![
                    "agents.apelogic.ai/service-principal:steward-run".to_owned(),
                    "agents.apelogic.ai/acting-user:alice@example.com".to_owned(),
                    "agents.apelogic.ai/canonical-user:usr_0123456789abcdef0123456789abcdef"
                        .to_owned(),
                ],
                None,
            ),
            (
                "empty audience",
                vec![
                    "agents.apelogic.ai/service-principal:steward-run".to_owned(),
                    "agents.apelogic.ai/acting-user:alice@example.com".to_owned(),
                    "agents.apelogic.ai/canonical-user:usr_0123456789abcdef0123456789abcdef"
                        .to_owned(),
                ],
                Some(Vec::new()),
            ),
            (
                "missing service group",
                vec![
                    "agents.apelogic.ai/acting-user:alice@example.com".to_owned(),
                    "agents.apelogic.ai/canonical-user:usr_0123456789abcdef0123456789abcdef"
                        .to_owned(),
                ],
                Some(vec![KUBERNETES_TOKEN_REVIEW_AUDIENCE.to_owned()]),
            ),
            (
                "missing acting-user group",
                vec![
                    "agents.apelogic.ai/service-principal:steward-run".to_owned(),
                    "agents.apelogic.ai/canonical-user:usr_0123456789abcdef0123456789abcdef"
                        .to_owned(),
                ],
                Some(vec![KUBERNETES_TOKEN_REVIEW_AUDIENCE.to_owned()]),
            ),
            (
                "unrecognized identity groups",
                vec![
                    "example.com/service-principal:steward-run".to_owned(),
                    "example.com/acting-user:alice@example.com".to_owned(),
                    "agents.apelogic.ai/canonical-user:usr_0123456789abcdef0123456789abcdef"
                        .to_owned(),
                ],
                Some(vec![KUBERNETES_TOKEN_REVIEW_AUDIENCE.to_owned()]),
            ),
            (
                "empty service group",
                vec![
                    "agents.apelogic.ai/service-principal:".to_owned(),
                    "agents.apelogic.ai/acting-user:alice@example.com".to_owned(),
                    "agents.apelogic.ai/canonical-user:usr_0123456789abcdef0123456789abcdef"
                        .to_owned(),
                ],
                Some(vec![KUBERNETES_TOKEN_REVIEW_AUDIENCE.to_owned()]),
            ),
        ];

        for (case, groups, audiences) in cases {
            let identity = task_identity_from_token_review(
                Some(TokenReviewStatus {
                    authenticated: Some(true),
                    audiences,
                    user: Some(UserInfo {
                        username: Some("alice@example.com".to_owned()),
                        groups: Some(groups),
                        ..UserInfo::default()
                    }),
                    ..TokenReviewStatus::default()
                }),
                KUBERNETES_TOKEN_REVIEW_AUDIENCE,
            );
            assert_eq!(
                identity,
                Err(TaskAuthenticationError::InvalidCredentials),
                "{case} must fail closed"
            );
        }
    }

    #[test]
    fn task_identity_requires_exactly_one_opaque_canonical_user_group() {
        for canonical_groups in [
            Vec::new(),
            vec!["agents.apelogic.ai/canonical-user:alice@example.com".to_owned()],
            vec![
                "agents.apelogic.ai/canonical-user:usr_0123456789abcdef0123456789abcdef".to_owned(),
                "agents.apelogic.ai/canonical-user:usr_abcdef0123456789abcdef0123456789".to_owned(),
            ],
        ] {
            let mut groups = vec![
                "agents.apelogic.ai/service-principal:steward-run".to_owned(),
                "agents.apelogic.ai/acting-user:alice@example.com".to_owned(),
            ];
            groups.extend(canonical_groups);
            let identity = task_identity_from_token_review(
                Some(TokenReviewStatus {
                    authenticated: Some(true),
                    audiences: Some(vec![KUBERNETES_TOKEN_REVIEW_AUDIENCE.to_owned()]),
                    user: Some(UserInfo {
                        username: Some("alice@example.com".to_owned()),
                        groups: Some(groups),
                        ..UserInfo::default()
                    }),
                    ..TokenReviewStatus::default()
                }),
                KUBERNETES_TOKEN_REVIEW_AUDIENCE,
            );
            assert_eq!(identity, Err(TaskAuthenticationError::InvalidCredentials));
        }
    }

    #[test]
    fn task_submission_body_cannot_select_a_canonical_or_acting_user() {
        for body in [
            r#"{"workflow":"code-review","codingAgentRuntime":"base","canonicalUserId":"usr_abcdef0123456789abcdef0123456789"}"#,
            r#"{"workflow":"code-review","codingAgentRuntime":"base","actingUser":"bob@example.org"}"#,
        ] {
            assert!(
                serde_json::from_str::<TaskSubmissionRequest>(body).is_err(),
                "caller-selected identity field was accepted: {body}"
            );
        }
    }

    impl RequestAuthenticator for FakeAuthenticator {
        fn authenticate<'a>(
            &'a self,
            bearer_token: &'a str,
        ) -> BoxFuture<'a, Result<AuthenticatedCaller, AuthenticationError>> {
            Box::pin(async move {
                match bearer_token {
                    "user-session" => Ok(AuthenticatedCaller {
                        actor: "alice@example.com".to_owned(),
                        member_roles: vec!["engineer".to_owned()],
                        is_admin: false,
                        can_bootstrap_steward_run_service_envelope: false,
                    }),
                    "user-duplicate-role-session" => Ok(AuthenticatedCaller {
                        actor: "alice@example.com".to_owned(),
                        member_roles: vec!["engineer".to_owned(), "engineer".to_owned()],
                        is_admin: false,
                        can_bootstrap_steward_run_service_envelope: false,
                    }),
                    "admin-session" => Ok(AuthenticatedCaller {
                        actor: "admin@example.com".to_owned(),
                        member_roles: Vec::new(),
                        is_admin: true,
                        can_bootstrap_steward_run_service_envelope: false,
                    }),
                    _ => Err(AuthenticationError::InvalidCredentials),
                }
            })
        }
    }

    impl RequestAuthenticator for BootstrapAuthenticator {
        fn authenticate<'a>(
            &'a self,
            bearer_token: &'a str,
        ) -> BoxFuture<'a, Result<AuthenticatedCaller, AuthenticationError>> {
            Box::pin(async move {
                if bearer_token != "bootstrap-session" {
                    return Err(AuthenticationError::InvalidCredentials);
                }
                Ok(AuthenticatedCaller {
                    actor: "bootstrap@example.com".to_owned(),
                    member_roles: Vec::new(),
                    is_admin: false,
                    can_bootstrap_steward_run_service_envelope: true,
                })
            })
        }
    }

    #[test]
    fn runtime_create_is_published_in_the_openapi_contract() -> Result<(), String> {
        let document = serde_json::to_value(ApiDoc::openapi())
            .map_err(|error| format!("failed to serialize OpenAPI document: {error}"))?;
        let operation = document
            .pointer("/paths/~1v1~1namespaces~1{namespace}~1runtimes/post")
            .ok_or_else(|| "runtime create operation is absent from OpenAPI".to_owned())?;
        assert!(
            operation
                .pointer("/requestBody/content/application~1json/schema")
                .is_some(),
            "runtime create must advertise its request schema"
        );
        for status in ["201", "202", "403", "404", "409", "422", "503"] {
            assert!(
                operation.pointer(&format!("/responses/{status}")).is_some(),
                "runtime create OpenAPI is missing its {status} response"
            );
        }
        for schema in [
            "CreateRuntimeRequest",
            "AgentRuntimeSpec",
            "Principal",
            "AgentType",
            "ModelRef",
            "ToolGrant",
            "Budget",
            "Duration",
            "BindingRef",
            "Email",
        ] {
            assert!(
                document
                    .pointer(&format!("/components/schemas/{schema}"))
                    .is_some(),
                "runtime create OpenAPI is missing its {schema} component"
            );
        }
        Ok(())
    }

    #[test]
    fn task_lifecycle_is_fully_published_in_the_openapi_contract() -> Result<(), String> {
        let document = serde_json::to_value(ApiDoc::openapi())
            .map_err(|error| format!("failed to serialize OpenAPI document: {error}"))?;
        let operations = [
            (
                "/paths/~1v1~1tasks/post",
                ["201", "202"].as_slice(),
                [
                    ("400", "text/plain"),
                    ("401", "application/json"),
                    ("404", "application/json"),
                    ("409", "application/json"),
                    ("415", "text/plain"),
                    ("422", "application/json"),
                    ("503", "application/json"),
                ]
                .as_slice(),
            ),
            (
                "/paths/~1v1~1tasks~1{taskUid}~1inputs/put",
                ["204"].as_slice(),
                [
                    ("400", "text/plain"),
                    ("401", "application/json"),
                    ("404", "application/json"),
                    ("409", "application/json"),
                    ("413", "application/json"),
                    ("415", "application/json"),
                    ("503", "application/json"),
                ]
                .as_slice(),
            ),
            (
                "/paths/~1v1~1tasks~1{taskUid}~1execute/post",
                ["202"].as_slice(),
                [
                    ("400", "text/plain"),
                    ("401", "application/json"),
                    ("404", "application/json"),
                    ("409", "application/json"),
                    ("503", "application/json"),
                ]
                .as_slice(),
            ),
            (
                "/paths/~1v1~1tasks~1{taskUid}/get",
                ["200"].as_slice(),
                [
                    ("400", "text/plain"),
                    ("401", "application/json"),
                    ("404", "application/json"),
                    ("503", "application/json"),
                ]
                .as_slice(),
            ),
            (
                "/paths/~1v1~1tasks~1{taskUid}~1outputs/get",
                ["200"].as_slice(),
                [
                    ("400", "text/plain"),
                    ("401", "application/json"),
                    ("404", "application/json"),
                    ("409", "application/json"),
                    ("503", "application/json"),
                ]
                .as_slice(),
            ),
            (
                "/paths/~1v1~1tasks~1{taskUid}/delete",
                ["202"].as_slice(),
                [
                    ("400", "text/plain"),
                    ("401", "application/json"),
                    ("404", "application/json"),
                    ("503", "application/json"),
                ]
                .as_slice(),
            ),
        ];
        for (pointer, success_statuses, error_responses) in operations {
            let operation = document
                .pointer(pointer)
                .ok_or_else(|| format!("Task operation {pointer} is absent from OpenAPI"))?;
            assert_eq!(
                operation.pointer("/security/0/taskBearer/type"),
                None,
                "operation security must reference, not inline, the taskBearer scheme"
            );
            assert!(
                operation.pointer("/security/0/taskBearer").is_some(),
                "Task operation {pointer} must require taskBearer"
            );
            for status in success_statuses {
                assert!(
                    operation.pointer(&format!("/responses/{status}")).is_some(),
                    "Task operation {pointer} is missing success status {status}"
                );
            }
            for (status, content_type) in error_responses {
                let content_type = content_type.replace('/', "~1");
                assert!(
                    operation
                        .pointer(&format!(
                            "/responses/{status}/content/{content_type}/schema"
                        ))
                        .is_some(),
                    "Task {pointer} error {status} must publish its {content_type} schema"
                );
            }
        }

        assert_eq!(
            document.pointer("/components/securitySchemes/taskBearer/type"),
            Some(&serde_json::json!("http"))
        );
        assert_eq!(
            document.pointer("/components/securitySchemes/taskBearer/scheme"),
            Some(&serde_json::json!("bearer"))
        );
        let status = document
            .pointer("/components/schemas/TaskStatusResponse")
            .ok_or_else(|| "TaskStatusResponse schema is absent".to_owned())?;
        for property in [
            "taskUid",
            "runtimeUid",
            "phase",
            "runtimeOwnership",
            "finalized",
            "failureReason",
            "deltas",
        ] {
            assert!(
                status.pointer(&format!("/properties/{property}")).is_some(),
                "TaskStatusResponse is missing {property}"
            );
        }
        assert_eq!(
            document.pointer("/components/schemas/TaskPhase/enum"),
            Some(&serde_json::json!([
                "submitted",
                "parked",
                "queued",
                "running",
                "succeeded",
                "failed",
                "cancelled"
            ]))
        );
        assert!(
            document
                .pointer("/components/schemas/TaskAdmissionDelta")
                .is_some(),
            "Task admission deltas must be machine-readable"
        );
        let submission = document
            .pointer("/paths/~1v1~1tasks/post/responses")
            .ok_or_else(|| "Task submission responses are absent".to_owned())?;
        assert!(
            submission
                .pointer("/201/description")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|value| value.contains("without an approval hold")),
            "201 must explicitly mean admitted without a hold"
        );
        assert!(
            submission
                .pointer("/202/description")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|value| value.contains("parked")),
            "202 must explicitly mean parked for approval"
        );
        assert!(
            document
                .pointer(
                    "/paths/~1v1~1tasks~1{taskUid}~1inputs/put/requestBody/content/application~1x-tar/schema",
                )
                .is_some(),
            "Task inputs must publish application/x-tar"
        );
        assert!(
            document
                .pointer(
                    "/paths/~1v1~1tasks~1{taskUid}~1outputs/get/responses/200/content/application~1x-tar/schema",
                )
                .is_some(),
            "Task outputs must publish application/x-tar"
        );
        Ok(())
    }

    #[test]
    fn agent_runs_contract_is_versioned_admin_only_and_explicit_about_gaps() -> Result<(), String> {
        let document = serde_json::to_value(ApiDoc::openapi())
            .map_err(|error| format!("failed to serialize OpenAPI document: {error}"))?;
        for pointer in [
            "/paths/~1admin~1api~1v1~1runs/get",
            "/paths/~1admin~1api~1v1~1runs~1{taskUid}/get",
            "/paths/~1admin~1api~1v1~1runs~1{taskUid}~1timeline/get",
        ] {
            let operation = document
                .pointer(pointer)
                .ok_or_else(|| format!("Agent Runs operation {pointer} is absent"))?;
            assert!(
                operation.pointer("/security/0/adminBearer").is_some(),
                "Agent Runs operation {pointer} must require adminBearer"
            );
            for status in ["200", "401", "403", "503"] {
                assert!(
                    operation.pointer(&format!("/responses/{status}")).is_some(),
                    "Agent Runs operation {pointer} is missing {status}"
                );
            }
        }
        assert_eq!(
            document.pointer("/components/securitySchemes/adminBearer/type"),
            Some(&serde_json::json!("http"))
        );
        let run = document
            .pointer("/components/schemas/AgentRunView/properties")
            .ok_or_else(|| "AgentRunView schema is absent".to_owned())?;
        for property in [
            "taskUid",
            "configuredModels",
            "grantedTools",
            "allocatedBudget",
            "observedSpend",
            "toolActivity",
            "inferenceActivity",
            "resources",
            "githubRun",
        ] {
            assert!(
                run.get(property).is_some(),
                "AgentRunView is missing explicit field {property}"
            );
        }
        Ok(())
    }

    #[derive(Clone, Default)]
    struct FakeDecisionChannel {
        requests: Arc<Mutex<Vec<DecisionRequest>>>,
        resolutions: Arc<Mutex<Vec<DecisionResolution>>>,
    }

    #[derive(Clone, Default)]
    struct SlowDecisionChannel {
        requests: Arc<AtomicUsize>,
    }

    #[derive(Clone)]
    struct ApprovalDuringDecisionFiling {
        application_slot: Arc<Mutex<Option<GrantReversion>>>,
        application: GrantReversion,
    }

    impl DecisionChannel for SlowDecisionChannel {
        async fn request(
            &self,
            _request: &DecisionRequest,
        ) -> Result<DecisionReference, PortError> {
            self.requests.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(StdDuration::from_millis(25)).await;
            Ok(DecisionReference {
                key: "PROJ-123".to_owned(),
                evidence_url: "https://jira.example.com/browse/PROJ-123".to_owned(),
            })
        }

        async fn record_resolution(
            &self,
            _resolution: &DecisionResolution,
        ) -> Result<(), PortError> {
            Ok(())
        }
    }

    impl DecisionChannel for ApprovalDuringDecisionFiling {
        async fn request(
            &self,
            _request: &DecisionRequest,
        ) -> Result<DecisionReference, PortError> {
            *self
                .application_slot
                .lock()
                .map_err(|_| PortError::Failed {
                    reason: "fake approved-application lock was poisoned".to_owned(),
                })? = Some(self.application.clone());
            Ok(DecisionReference {
                key: "PROJ-123".to_owned(),
                evidence_url: "https://jira.example.com/browse/PROJ-123".to_owned(),
            })
        }

        async fn record_resolution(
            &self,
            _resolution: &DecisionResolution,
        ) -> Result<(), PortError> {
            Ok(())
        }
    }

    impl DecisionChannel for FakeDecisionChannel {
        async fn request(&self, request: &DecisionRequest) -> Result<DecisionReference, PortError> {
            self.requests
                .lock()
                .map_err(|_| PortError::Failed {
                    reason: "fake decision-request lock was poisoned".to_owned(),
                })?
                .push(request.clone());
            Ok(DecisionReference {
                key: "PROJ-123".to_owned(),
                evidence_url: "https://jira.example.com/browse/PROJ-123".to_owned(),
            })
        }

        async fn record_resolution(
            &self,
            resolution: &DecisionResolution,
        ) -> Result<(), PortError> {
            self.resolutions
                .lock()
                .map_err(|_| PortError::Failed {
                    reason: "fake decision-resolution lock was poisoned".to_owned(),
                })?
                .push(resolution.clone());
            Ok(())
        }
    }

    #[derive(Clone)]
    struct FakeRuntimeRepository {
        runtime: Arc<Mutex<AgentRuntime>>,
    }

    #[derive(Clone, Default)]
    struct MultiRuntimeRepository {
        runtimes: Arc<Mutex<Vec<AgentRuntime>>>,
    }

    #[derive(Clone)]
    struct RejectedCreateRepository {
        runtime: AgentRuntime,
        status: u16,
    }

    impl RuntimeRepository for RejectedCreateRepository {
        fn create<'a>(
            &'a self,
            _namespace: &'a str,
            _runtime: &'a AgentRuntime,
            _context: &'a AdmissionContext,
        ) -> BoxFuture<'a, Result<AgentRuntime, RuntimeCreateError>> {
            Box::pin(async {
                Err(RuntimeCreateError::Kubernetes {
                    status: self.status,
                    message: "Kubernetes rejected the AgentRuntime".to_owned(),
                })
            })
        }

        fn create_as_authority<'a>(
            &'a self,
            _namespace: &'a str,
            _runtime: &'a AgentRuntime,
        ) -> BoxFuture<'a, Result<AgentRuntime, RuntimeCreateError>> {
            Box::pin(async {
                Err(RuntimeCreateError::Kubernetes {
                    status: self.status,
                    message: "Kubernetes rejected the AgentRuntime".to_owned(),
                })
            })
        }

        fn get<'a>(
            &'a self,
            _namespace: &'a str,
            _name: &'a str,
        ) -> BoxFuture<'a, Result<AgentRuntime, String>> {
            Box::pin(async move { Ok(self.runtime.clone()) })
        }

        fn get_bound<'a>(
            &'a self,
            _namespace: &'a str,
            _name: &'a str,
            _runtime_uid: &'a str,
        ) -> BoxFuture<'a, Result<AgentRuntime, String>> {
            Box::pin(async move { Ok(self.runtime.clone()) })
        }

        fn get_by_uid<'a>(
            &'a self,
            runtime_uid: &'a str,
        ) -> BoxFuture<'a, Result<AgentRuntime, String>> {
            Box::pin(async move {
                if self.runtime.metadata.uid.as_deref() == Some(runtime_uid) {
                    Ok(self.runtime.clone())
                } else {
                    Err(format!("AgentRuntime UID {runtime_uid} does not exist"))
                }
            })
        }

        fn replace<'a>(
            &'a self,
            _runtime: &'a AgentRuntime,
            _context: &'a AdmissionContext,
        ) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }

        fn replace_as_authority<'a>(
            &'a self,
            _runtime: &'a AgentRuntime,
        ) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }
    }

    impl RuntimeRepository for FakeRuntimeRepository {
        fn create<'a>(
            &'a self,
            _namespace: &'a str,
            runtime: &'a AgentRuntime,
            _context: &'a AdmissionContext,
        ) -> BoxFuture<'a, Result<AgentRuntime, RuntimeCreateError>> {
            Box::pin(async move {
                let mut stored = self.runtime.lock().map_err(|_| {
                    RuntimeCreateError::Unavailable("fake runtime lock was poisoned".to_owned())
                })?;
                if stored.name_any() == runtime.name_any() {
                    return Err(RuntimeCreateError::Kubernetes {
                        status: 409,
                        message: "AgentRuntime already exists".to_owned(),
                    });
                }
                let mut created = runtime.clone();
                created.metadata.uid = Some(format!("{}-uid", created.name_any()));
                created.metadata.resource_version = Some("1".to_owned());
                *stored = created.clone();
                Ok(created)
            })
        }

        fn create_as_authority<'a>(
            &'a self,
            _namespace: &'a str,
            runtime: &'a AgentRuntime,
        ) -> BoxFuture<'a, Result<AgentRuntime, RuntimeCreateError>> {
            Box::pin(async move {
                let mut stored = self.runtime.lock().map_err(|_| {
                    RuntimeCreateError::Unavailable("fake runtime lock was poisoned".to_owned())
                })?;
                if stored.name_any() == runtime.name_any() {
                    return Err(RuntimeCreateError::Kubernetes {
                        status: 409,
                        message: "AgentRuntime already exists".to_owned(),
                    });
                }
                let mut created = runtime.clone();
                created.metadata.uid = Some(format!("{}-uid", created.name_any()));
                created.metadata.resource_version = Some("1".to_owned());
                *stored = created.clone();
                Ok(created)
            })
        }

        fn get<'a>(
            &'a self,
            _namespace: &'a str,
            _name: &'a str,
        ) -> BoxFuture<'a, Result<AgentRuntime, String>> {
            Box::pin(async move {
                self.runtime
                    .lock()
                    .map(|runtime| runtime.clone())
                    .map_err(|_| "fake runtime lock was poisoned".to_owned())
            })
        }

        fn get_bound<'a>(
            &'a self,
            namespace: &'a str,
            name: &'a str,
            runtime_uid: &'a str,
        ) -> BoxFuture<'a, Result<AgentRuntime, String>> {
            Box::pin(async move {
                let runtime = self
                    .runtime
                    .lock()
                    .map_err(|_| "fake runtime lock was poisoned".to_owned())?
                    .clone();
                if runtime.metadata.namespace.as_deref() == Some(namespace)
                    && runtime.metadata.name.as_deref() == Some(name)
                    && runtime.metadata.uid.as_deref() == Some(runtime_uid)
                {
                    Ok(runtime)
                } else {
                    Err(format!(
                        "AgentRuntime {namespace}/{name} is not bound to UID {runtime_uid}"
                    ))
                }
            })
        }

        fn get_by_uid<'a>(
            &'a self,
            runtime_uid: &'a str,
        ) -> BoxFuture<'a, Result<AgentRuntime, String>> {
            Box::pin(async move {
                let runtime = self
                    .runtime
                    .lock()
                    .map_err(|_| "fake runtime lock was poisoned".to_owned())?
                    .clone();
                if runtime.metadata.uid.as_deref() == Some(runtime_uid) {
                    Ok(runtime)
                } else {
                    Err(format!("AgentRuntime UID {runtime_uid} does not exist"))
                }
            })
        }

        fn replace<'a>(
            &'a self,
            runtime: &'a AgentRuntime,
            _context: &'a AdmissionContext,
        ) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(async move {
                let mut stored = self
                    .runtime
                    .lock()
                    .map_err(|_| "fake runtime lock was poisoned".to_owned())?;
                *stored = runtime.clone();
                Ok(())
            })
        }

        fn replace_as_authority<'a>(
            &'a self,
            runtime: &'a AgentRuntime,
        ) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(async move {
                let mut stored = self
                    .runtime
                    .lock()
                    .map_err(|_| "fake runtime lock was poisoned".to_owned())?;
                *stored = runtime.clone();
                Ok(())
            })
        }
    }

    impl RuntimeRepository for MultiRuntimeRepository {
        fn create<'a>(
            &'a self,
            _namespace: &'a str,
            runtime: &'a AgentRuntime,
            _context: &'a AdmissionContext,
        ) -> BoxFuture<'a, Result<AgentRuntime, RuntimeCreateError>> {
            self.create_as_authority(_namespace, runtime)
        }

        fn create_as_authority<'a>(
            &'a self,
            _namespace: &'a str,
            runtime: &'a AgentRuntime,
        ) -> BoxFuture<'a, Result<AgentRuntime, RuntimeCreateError>> {
            Box::pin(async move {
                let mut runtimes = self.runtimes.lock().map_err(|_| {
                    RuntimeCreateError::Unavailable(
                        "multi-runtime repository lock was poisoned".to_owned(),
                    )
                })?;
                if runtimes
                    .iter()
                    .any(|stored| stored.name_any() == runtime.name_any())
                {
                    return Err(RuntimeCreateError::Kubernetes {
                        status: 409,
                        message: "AgentRuntime already exists".to_owned(),
                    });
                }
                let mut created = runtime.clone();
                created.metadata.uid = Some(format!("{}-uid", created.name_any()));
                created.metadata.resource_version = Some("1".to_owned());
                runtimes.push(created.clone());
                Ok(created)
            })
        }

        fn get<'a>(
            &'a self,
            namespace: &'a str,
            name: &'a str,
        ) -> BoxFuture<'a, Result<AgentRuntime, String>> {
            Box::pin(async move {
                self.runtimes
                    .lock()
                    .map_err(|_| "multi-runtime repository lock was poisoned".to_owned())?
                    .iter()
                    .find(|runtime| {
                        runtime.metadata.namespace.as_deref() == Some(namespace)
                            && runtime.metadata.name.as_deref() == Some(name)
                    })
                    .cloned()
                    .ok_or_else(|| format!("AgentRuntime {namespace}/{name} does not exist"))
            })
        }

        fn get_bound<'a>(
            &'a self,
            namespace: &'a str,
            name: &'a str,
            runtime_uid: &'a str,
        ) -> BoxFuture<'a, Result<AgentRuntime, String>> {
            Box::pin(async move {
                let runtime = self.get(namespace, name).await?;
                if runtime.metadata.uid.as_deref() == Some(runtime_uid) {
                    Ok(runtime)
                } else {
                    Err(format!(
                        "AgentRuntime {namespace}/{name} is not bound to UID {runtime_uid}"
                    ))
                }
            })
        }

        fn get_by_uid<'a>(
            &'a self,
            runtime_uid: &'a str,
        ) -> BoxFuture<'a, Result<AgentRuntime, String>> {
            Box::pin(async move {
                self.runtimes
                    .lock()
                    .map_err(|_| "multi-runtime repository lock was poisoned".to_owned())?
                    .iter()
                    .find(|runtime| runtime.metadata.uid.as_deref() == Some(runtime_uid))
                    .cloned()
                    .ok_or_else(|| format!("AgentRuntime UID {runtime_uid} does not exist"))
            })
        }

        fn replace<'a>(
            &'a self,
            runtime: &'a AgentRuntime,
            _context: &'a AdmissionContext,
        ) -> BoxFuture<'a, Result<(), String>> {
            self.replace_as_authority(runtime)
        }

        fn replace_as_authority<'a>(
            &'a self,
            runtime: &'a AgentRuntime,
        ) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(async move {
                let mut runtimes = self
                    .runtimes
                    .lock()
                    .map_err(|_| "multi-runtime repository lock was poisoned".to_owned())?;
                let stored = runtimes
                    .iter_mut()
                    .find(|stored| stored.metadata.uid == runtime.metadata.uid)
                    .ok_or_else(|| "AgentRuntime replacement target does not exist".to_owned())?;
                *stored = runtime.clone();
                Ok(())
            })
        }
    }

    #[derive(Clone)]
    struct FakeLedger {
        envelope: Arc<Mutex<Envelope>>,
        grants: Vec<AdmissionDelta>,
        parked: ParkedRows,
        decision_references: DecisionReferences,
        decision_filing_claim: Arc<Mutex<Option<Uuid>>>,
        revoke_rows: u64,
        reversion: Option<GrantReversion>,
        application: Arc<Mutex<Option<GrantReversion>>>,
        application_committed_during_park: Arc<Mutex<Option<GrantReversion>>>,
        application_revoked_during_retirement: Arc<Mutex<bool>>,
        tasks: Arc<Mutex<Vec<TaskRecord>>>,
        agent_runs: Arc<Mutex<Vec<AgentRunRecord>>>,
        agent_run_events: AgentRunEvents,
        service_envelope_authors: Arc<Mutex<Vec<(String, String)>>>,
    }

    #[derive(Clone)]
    struct FakeParked {
        runtime_uid: String,
        runtime_namespace: String,
        runtime_name: String,
        deltas: Vec<AdmissionDelta>,
        proposed_spec: AgentRuntimeSpec,
        base_spec_digest: String,
        base_pending_approval_digest: Option<String>,
        actor: String,
        member_role: String,
        envelope_revision: i64,
    }

    type ParkedRows = Arc<Mutex<Vec<FakeParked>>>;
    type DecisionReferences = Arc<Mutex<Vec<(Uuid, String, String)>>>;
    type AgentRunEvents = Arc<Mutex<Vec<(Uuid, Vec<AgentRunTimelineEvent>)>>>;

    impl AdmissionLedger for FakeLedger {
        fn insert_envelope<'a>(
            &'a self,
            _member_role: &'a str,
            _envelope: &'a Envelope,
            _authored_by: &'a str,
        ) -> BoxFuture<'a, Result<(), StoreError>> {
            Box::pin(async { Ok(()) })
        }

        fn insert_service_envelope<'a>(
            &'a self,
            service: &'a str,
            _envelope: &'a Envelope,
            authored_by: &'a str,
        ) -> BoxFuture<'a, Result<(), StoreError>> {
            Box::pin(async move {
                self.service_envelope_authors
                    .lock()
                    .map_err(|_| {
                        StoreError::Database(
                            "fake service-envelope author lock was poisoned".to_owned(),
                        )
                    })?
                    .push((service.to_owned(), authored_by.to_owned()));
                Ok(())
            })
        }

        fn latest_envelope<'a>(
            &'a self,
            _member_role: &'a str,
        ) -> BoxFuture<'a, Result<Option<Envelope>, StoreError>> {
            Box::pin(async move {
                self.envelope
                    .lock()
                    .map(|envelope| Some(envelope.clone()))
                    .map_err(|_| StoreError::Database("fake ledger lock was poisoned".to_owned()))
            })
        }

        fn latest_service_envelope<'a>(
            &'a self,
            _service: &'a str,
        ) -> BoxFuture<'a, Result<Option<Envelope>, StoreError>> {
            Box::pin(async move {
                self.envelope
                    .lock()
                    .map(|envelope| Some(envelope.clone()))
                    .map_err(|_| StoreError::Database("fake ledger lock was poisoned".to_owned()))
            })
        }

        fn park_rejection<'a>(
            &'a self,
            request: ParkRejection<'a>,
        ) -> BoxFuture<'a, Result<ParkedAdmission, StoreError>> {
            Box::pin(async move {
                let application_committed = if let Some(application) = self
                    .application_committed_during_park
                    .lock()
                    .map_err(|_| {
                        StoreError::Database("fake approval-race lock was poisoned".to_owned())
                    })?
                    .take()
                {
                    *self.application.lock().map_err(|_| {
                        StoreError::Database(
                            "fake approved-application lock was poisoned".to_owned(),
                        )
                    })? = Some(application);
                    true
                } else {
                    false
                };
                let mut parked = self.parked.lock().map_err(|_| {
                    StoreError::Database("fake ledger lock was poisoned".to_owned())
                })?;
                if application_committed {
                    parked.clear();
                }
                if parked.is_empty() || application_committed {
                    parked.push(FakeParked {
                        runtime_uid: request.runtime_uid.to_owned(),
                        runtime_namespace: request.runtime_namespace.to_owned(),
                        runtime_name: request.runtime_name.to_owned(),
                        deltas: request.deltas.to_vec(),
                        proposed_spec: request.proposed_spec.clone(),
                        base_spec_digest: request.base_spec_digest.to_owned(),
                        base_pending_approval_digest: request
                            .base_pending_approval_digest
                            .map(str::to_owned),
                        actor: request.actor.to_owned(),
                        member_role: request.member_role.to_owned(),
                        envelope_revision: request.envelope_revision,
                    });
                }
                let reference = self
                    .decision_references
                    .lock()
                    .map_err(|_| StoreError::Database("fake ledger lock was poisoned".to_owned()))?
                    .iter()
                    .find(|(approval_id, _, _)| *approval_id == Uuid::nil())
                    .cloned();
                Ok(ParkedAdmission {
                    decision_id: Uuid::nil(),
                    approval_id: Uuid::nil(),
                    decision_key: reference.as_ref().map(|(_, key, _)| key.clone()),
                    evidence_url: reference.map(|(_, _, url)| url),
                })
            })
        }

        fn pending_approvals(&self) -> BoxFuture<'_, Result<Vec<PendingApproval>, StoreError>> {
            Box::pin(async move {
                let rows = self.parked.lock().map_err(|_| {
                    StoreError::Database("fake ledger lock was poisoned".to_owned())
                })?;
                let references = self.decision_references.lock().map_err(|_| {
                    StoreError::Database("fake ledger lock was poisoned".to_owned())
                })?;
                let reference = references
                    .iter()
                    .find(|(approval_id, _, _)| *approval_id == Uuid::nil());
                Ok(rows
                    .iter()
                    .map(|parked| PendingApproval {
                        approval_id: Uuid::nil(),
                        decision_id: Uuid::nil(),
                        runtime_uid: parked.runtime_uid.clone(),
                        decision_key: reference.map(|(_, key, _)| key.clone()),
                        evidence_url: reference.map(|(_, _, url)| url.clone()),
                        deltas: parked.deltas.clone(),
                        proposed_spec: parked.proposed_spec.clone(),
                        base_spec_digest: parked.base_spec_digest.clone(),
                        base_pending_approval_digest: parked.base_pending_approval_digest.clone(),
                        envelope_revision: parked.envelope_revision,
                        actor: parked.actor.clone(),
                        member_role: parked.member_role.clone(),
                    })
                    .collect())
            })
        }

        fn retire_pending_approval_if_superseded<'a>(
            &'a self,
            _approval_id: Uuid,
            winning_approval_id: Uuid,
            _runtime_uid: &'a str,
            _decided_by: &'a str,
            _rationale: &'a str,
        ) -> BoxFuture<'a, Result<Option<GrantReversion>, StoreError>> {
            Box::pin(async move {
                let mut revoke =
                    self.application_revoked_during_retirement
                        .lock()
                        .map_err(|_| {
                            StoreError::Database(
                                "fake retirement-race lock was poisoned".to_owned(),
                            )
                        })?;
                if *revoke {
                    *revoke = false;
                    *self.application.lock().map_err(|_| {
                        StoreError::Database(
                            "fake approved-application lock was poisoned".to_owned(),
                        )
                    })? = None;
                    return Ok(None);
                }
                let application = self
                    .application
                    .lock()
                    .map_err(|_| {
                        StoreError::Database(
                            "fake approved-application lock was poisoned".to_owned(),
                        )
                    })?
                    .clone();
                if winning_approval_id != Uuid::nil() {
                    return Ok(None);
                }
                if application.is_none() {
                    return Ok(None);
                }
                self.parked
                    .lock()
                    .map_err(|_| StoreError::Database("fake ledger lock was poisoned".to_owned()))?
                    .pop()
                    .ok_or(StoreError::ApprovalNotFound)?;
                Ok(application)
            })
        }

        fn link_decision_reference<'a>(
            &'a self,
            approval_id: Uuid,
            decision_key: &'a str,
            evidence_url: &'a str,
        ) -> BoxFuture<'a, Result<(), StoreError>> {
            Box::pin(async move {
                self.decision_references
                    .lock()
                    .map_err(|_| StoreError::Database("fake ledger lock was poisoned".to_owned()))?
                    .push((
                        approval_id,
                        decision_key.to_owned(),
                        evidence_url.to_owned(),
                    ));
                Ok(())
            })
        }

        fn approve_admission<'a>(
            &'a self,
            request: ApproveAdmission<'a>,
        ) -> BoxFuture<'a, Result<ApprovedAdmission, StoreError>> {
            Box::pin(async move {
                let rows = self.parked.lock().map_err(|_| {
                    StoreError::Database("fake ledger lock was poisoned".to_owned())
                })?;
                let parked = rows.first().ok_or(StoreError::ApprovalNotFound)?;
                let references = self.decision_references.lock().map_err(|_| {
                    StoreError::Database("fake ledger lock was poisoned".to_owned())
                })?;
                let (_, decision_key, evidence_url) = references
                    .iter()
                    .find(|(approval_id, _, _)| *approval_id == request.approval_id)
                    .ok_or(StoreError::MissingDecisionReference)?;
                if evidence_url != request.evidence_url {
                    return Err(StoreError::EvidenceMismatch);
                }
                Ok(ApprovedAdmission {
                    approval_id: request.approval_id,
                    decision_id: Uuid::nil(),
                    runtime_uid: parked.runtime_uid.clone(),
                    proposed_spec: parked.proposed_spec.clone(),
                    base_spec_digest: parked.base_spec_digest.clone(),
                    actor: parked.actor.clone(),
                    member_role: parked.member_role.clone(),
                    decision_key: decision_key.clone(),
                    evidence_url: evidence_url.clone(),
                    grants: parked.deltas.clone(),
                    decided_by: request.decided_by.to_owned(),
                    rationale: request.rationale.to_owned(),
                })
            })
        }

        fn grants_for_runtime<'a>(
            &'a self,
            _runtime_uid: &'a str,
            _member_role: &'a str,
            _envelope_revision: i64,
        ) -> BoxFuture<'a, Result<Vec<AdmissionDelta>, StoreError>> {
            Box::pin(async move { Ok(self.grants.clone()) })
        }

        fn grants_for_runtime_scoped<'a>(
            &'a self,
            _runtime_uid: &'a str,
            _scope_kind: EnvelopeScopeKind,
            _scope_ref: &'a str,
            _envelope_revision: i64,
        ) -> BoxFuture<'a, Result<Vec<AdmissionDelta>, StoreError>> {
            Box::pin(async move { Ok(self.grants.clone()) })
        }

        fn approval_candidate<'a>(
            &'a self,
            approval_id: Uuid,
            evidence_url: &'a str,
        ) -> BoxFuture<'a, Result<ApprovalCandidate, StoreError>> {
            Box::pin(async move {
                let rows = self.parked.lock().map_err(|_| {
                    StoreError::Database("fake ledger lock was poisoned".to_owned())
                })?;
                let parked = rows.first().ok_or(StoreError::ApprovalNotFound)?;
                let references = self.decision_references.lock().map_err(|_| {
                    StoreError::Database("fake ledger lock was poisoned".to_owned())
                })?;
                let (_, _, stored_evidence) = references
                    .iter()
                    .find(|(id, _, _)| *id == approval_id)
                    .ok_or(StoreError::MissingDecisionReference)?;
                if stored_evidence != evidence_url {
                    return Err(StoreError::EvidenceMismatch);
                }
                Ok(ApprovalCandidate {
                    approval_id,
                    runtime_uid: parked.runtime_uid.clone(),
                    proposed_spec: parked.proposed_spec.clone(),
                    base_spec_digest: parked.base_spec_digest.clone(),
                    base_pending_approval_digest: parked.base_pending_approval_digest.clone(),
                    actor: parked.actor.clone(),
                    member_role: parked.member_role.clone(),
                    envelope_revision: parked.envelope_revision,
                    runtime_namespace: parked.runtime_namespace.clone(),
                    runtime_name: parked.runtime_name.clone(),
                })
            })
        }

        fn approval_for_filing(
            &self,
            approval_id: Uuid,
        ) -> BoxFuture<'_, Result<DecisionFiling, StoreError>> {
            Box::pin(async move {
                let rows = self.parked.lock().map_err(|_| {
                    StoreError::Database("fake ledger lock was poisoned".to_owned())
                })?;
                let parked = rows.first().ok_or(StoreError::ApprovalNotFound)?;
                let reference = self
                    .decision_references
                    .lock()
                    .map_err(|_| StoreError::Database("fake ledger lock was poisoned".to_owned()))?
                    .iter()
                    .find(|(id, _, _)| *id == approval_id)
                    .cloned();
                Ok(DecisionFiling {
                    approval_id,
                    runtime_uid: parked.runtime_uid.clone(),
                    actor: parked.actor.clone(),
                    member_role: parked.member_role.clone(),
                    deltas: parked.deltas.clone(),
                    decision_key: reference.as_ref().map(|(_, key, _)| key.clone()),
                    evidence_url: reference.map(|(_, _, url)| url),
                })
            })
        }

        fn claim_decision_filing(
            &self,
            approval_id: Uuid,
        ) -> BoxFuture<'_, Result<DecisionFilingClaim, StoreError>> {
            Box::pin(async move {
                let filing = self.approval_for_filing(approval_id).await?;
                if filing.decision_key.is_some() && filing.evidence_url.is_some() {
                    return Ok(DecisionFilingClaim {
                        filing,
                        token: None,
                    });
                }
                let mut claim = self.decision_filing_claim.lock().map_err(|_| {
                    StoreError::Database("fake filing claim lock was poisoned".to_owned())
                })?;
                if claim.is_some() {
                    return Err(StoreError::DecisionFilingInProgress);
                }
                let token = Uuid::new_v4();
                *claim = Some(token);
                Ok(DecisionFilingClaim {
                    filing,
                    token: Some(token),
                })
            })
        }

        fn complete_decision_filing<'a>(
            &'a self,
            approval_id: Uuid,
            token: Uuid,
            decision_key: &'a str,
            evidence_url: &'a str,
        ) -> BoxFuture<'a, Result<(), StoreError>> {
            Box::pin(async move {
                let mut claim = self.decision_filing_claim.lock().map_err(|_| {
                    StoreError::Database("fake filing claim lock was poisoned".to_owned())
                })?;
                if *claim != Some(token) {
                    return Err(StoreError::DecisionFilingClaimLost);
                }
                self.decision_references
                    .lock()
                    .map_err(|_| StoreError::Database("fake ledger lock was poisoned".to_owned()))?
                    .push((
                        approval_id,
                        decision_key.to_owned(),
                        evidence_url.to_owned(),
                    ));
                *claim = None;
                Ok(())
            })
        }

        fn release_decision_filing(
            &self,
            _approval_id: Uuid,
            token: Uuid,
        ) -> BoxFuture<'_, Result<(), StoreError>> {
            Box::pin(async move {
                let mut claim = self.decision_filing_claim.lock().map_err(|_| {
                    StoreError::Database("fake filing claim lock was poisoned".to_owned())
                })?;
                if *claim == Some(token) {
                    *claim = None;
                }
                Ok(())
            })
        }

        fn grant_reversion<'a>(
            &'a self,
            _runtime_uid: &'a str,
        ) -> BoxFuture<'a, Result<Option<GrantReversion>, StoreError>> {
            Box::pin(async move { Ok(self.reversion.clone()) })
        }

        fn grant_application<'a>(
            &'a self,
            _runtime_uid: &'a str,
        ) -> BoxFuture<'a, Result<Option<GrantApplication>, StoreError>> {
            Box::pin(async move {
                self.application
                    .lock()
                    .map(|application| {
                        application.clone().map(|application| GrantApplication {
                            approval_id: Uuid::nil(),
                            application,
                        })
                    })
                    .map_err(|_| {
                        StoreError::Database(
                            "fake approved-application lock was poisoned".to_owned(),
                        )
                    })
            })
        }

        fn revoke_runtime_grants<'a>(
            &'a self,
            _runtime_uid: &'a str,
            _revoked_by: &'a str,
            _reason: &'a str,
        ) -> BoxFuture<'a, Result<u64, StoreError>> {
            Box::pin(async move { Ok(self.revoke_rows) })
        }
    }

    impl AgentRunLedger for FakeLedger {
        fn agent_runs<'a>(
            &'a self,
            query: &'a AgentRunQuery,
        ) -> BoxFuture<'a, Result<AgentRunPage, StoreError>> {
            Box::pin(async move {
                if query.limit == 0 || query.limit > 100 {
                    return Err(StoreError::InvalidRunQuery);
                }
                let mut records = self
                    .agent_runs
                    .lock()
                    .map_err(|_| {
                        StoreError::Database("fake agent-run lock was poisoned".to_owned())
                    })?
                    .clone();
                records.sort_by(|left, right| {
                    right
                        .created_at
                        .cmp(&left.created_at)
                        .then_with(|| right.task_uid.cmp(&left.task_uid))
                });
                if let Some(cursor) = query.cursor {
                    let Some(index) = records.iter().position(|record| record.task_uid == cursor)
                    else {
                        return Err(StoreError::InvalidRunCursor);
                    };
                    records.drain(..=index);
                }
                if let Some(phase) = query.phase {
                    records.retain(|record| record.phase == phase);
                }
                if let Some(workflow) = query.workflow.as_deref() {
                    if workflow.is_empty() {
                        return Err(StoreError::InvalidRunQuery);
                    }
                    records.retain(|record| record.workflow == workflow);
                }
                let next_cursor = if records.len() > query.limit as usize {
                    records.truncate(query.limit as usize);
                    records.last().map(|record| record.task_uid)
                } else {
                    None
                };
                Ok(AgentRunPage {
                    records,
                    next_cursor,
                })
            })
        }

        fn agent_run(
            &self,
            task_uid: Uuid,
        ) -> BoxFuture<'_, Result<Option<AgentRunRecord>, StoreError>> {
            Box::pin(async move {
                self.agent_runs
                    .lock()
                    .map_err(|_| {
                        StoreError::Database("fake agent-run lock was poisoned".to_owned())
                    })
                    .map(|records| {
                        records
                            .iter()
                            .find(|record| record.task_uid == task_uid)
                            .cloned()
                    })
            })
        }

        fn agent_run_timeline(
            &self,
            task_uid: Uuid,
        ) -> BoxFuture<'_, Result<Option<Vec<AgentRunTimelineEvent>>, StoreError>> {
            Box::pin(async move {
                self.agent_run_events
                    .lock()
                    .map_err(|_| {
                        StoreError::Database("fake agent-run timeline lock was poisoned".to_owned())
                    })
                    .map(|events| {
                        events
                            .iter()
                            .find(|(candidate, _)| *candidate == task_uid)
                            .map(|(_, events)| events.clone())
                    })
            })
        }
    }

    impl TaskSubmissionLedger for FakeLedger {
        fn reserve_task<'a>(
            &'a self,
            request: TaskReservationRequest<'a>,
        ) -> BoxFuture<'a, Result<TaskReservation, StoreError>> {
            Box::pin(async move {
                let mut tasks = self.tasks.lock().map_err(|_| {
                    StoreError::Database("fake task ledger lock was poisoned".to_owned())
                })?;
                if let Some(existing) = tasks.iter().find(|task| {
                    task.submitter_service == request.submitter_service
                        && task.owner_user_id.as_deref() == Some(request.owner_user_id)
                        && task.idempotency_key == request.idempotency_key
                }) {
                    return Ok(TaskReservation {
                        inserted: false,
                        record: existing.clone(),
                    });
                }
                let record = TaskRecord {
                    task_uid: Uuid::new_v4(),
                    idempotency_key: request.idempotency_key.to_owned(),
                    submitter_service: request.submitter_service.to_owned(),
                    acting_user: request.acting_user.map(str::to_owned),
                    acting_user_id: request.acting_user_id.map(str::to_owned),
                    owner: request.owner.to_owned(),
                    owner_user_id: Some(request.owner_user_id.to_owned()),
                    identity_binding_state: "bound".to_owned(),
                    workflow: request.workflow.to_owned(),
                    coding_agent_runtime: request.coding_agent_runtime.to_owned(),
                    runtime_uid: None,
                    runtime_namespace: request.runtime_namespace.to_owned(),
                    runtime_name: request.runtime_name.to_owned(),
                    runtime_ownership: request.runtime_ownership,
                    phase: TaskPhase::Submitted,
                    runtime_spec: request.runtime_spec.clone(),
                    agent_command: request.agent_command.to_vec(),
                    input_archive: None,
                    output_archive: None,
                    execute_requested: false,
                    finalize_requested: false,
                    finalized: false,
                    failure_reason: None,
                };
                tasks.push(record.clone());
                Ok(TaskReservation {
                    inserted: true,
                    record,
                })
            })
        }

        fn bind_task_runtime<'a>(
            &'a self,
            task_uid: Uuid,
            runtime_uid: &'a str,
            phase: TaskPhase,
        ) -> BoxFuture<'a, Result<TaskRecord, StoreError>> {
            Box::pin(async move {
                let mut tasks = self.tasks.lock().map_err(|_| {
                    StoreError::Database("fake task ledger lock was poisoned".to_owned())
                })?;
                let task = tasks
                    .iter_mut()
                    .find(|task| task.task_uid == task_uid)
                    .ok_or(StoreError::TaskNotFound)?;
                task.runtime_uid = Some(runtime_uid.to_owned());
                task.phase = phase;
                Ok(task.clone())
            })
        }

        fn put_task_inputs<'a>(
            &'a self,
            task_uid: Uuid,
            submitter_service: &'a str,
            owner_user_id: &'a str,
            archive: &'a [u8],
        ) -> BoxFuture<'a, Result<TaskRecord, StoreError>> {
            Box::pin(async move {
                let mut tasks = self.tasks.lock().map_err(|_| {
                    StoreError::Database("fake task ledger lock was poisoned".to_owned())
                })?;
                let task = tasks
                    .iter_mut()
                    .find(|task| {
                        task.task_uid == task_uid
                            && task.submitter_service == submitter_service
                            && task.owner_user_id.as_deref() == Some(owner_user_id)
                    })
                    .ok_or(StoreError::TaskNotFound)?;
                if task.execute_requested
                    || !matches!(task.phase, TaskPhase::Submitted | TaskPhase::Parked)
                    || task
                        .input_archive
                        .as_deref()
                        .is_some_and(|existing| existing != archive)
                {
                    return Err(StoreError::InvalidTaskTransition);
                }
                task.input_archive = Some(archive.to_vec());
                Ok(task.clone())
            })
        }

        fn request_task_execution<'a>(
            &'a self,
            task_uid: Uuid,
            submitter_service: &'a str,
            owner_user_id: &'a str,
        ) -> BoxFuture<'a, Result<TaskRecord, StoreError>> {
            Box::pin(async move {
                let mut tasks = self.tasks.lock().map_err(|_| {
                    StoreError::Database("fake task ledger lock was poisoned".to_owned())
                })?;
                let task = tasks
                    .iter_mut()
                    .find(|task| {
                        task.task_uid == task_uid
                            && task.submitter_service == submitter_service
                            && task.owner_user_id.as_deref() == Some(owner_user_id)
                    })
                    .ok_or(StoreError::TaskNotFound)?;
                if task.input_archive.is_none() || task.finalize_requested {
                    return Err(StoreError::InvalidTaskTransition);
                }
                task.execute_requested = true;
                if task.phase == TaskPhase::Submitted {
                    task.phase = TaskPhase::Queued;
                }
                Ok(task.clone())
            })
        }

        fn task_for_submitter<'a>(
            &'a self,
            task_uid: Uuid,
            submitter_service: &'a str,
            owner_user_id: &'a str,
        ) -> BoxFuture<'a, Result<Option<TaskRecord>, StoreError>> {
            Box::pin(async move {
                self.tasks
                    .lock()
                    .map_err(|_| {
                        StoreError::Database("fake task ledger lock was poisoned".to_owned())
                    })
                    .map(|tasks| {
                        tasks
                            .iter()
                            .find(|task| {
                                task.task_uid == task_uid
                                    && task.submitter_service == submitter_service
                                    && task.owner_user_id.as_deref() == Some(owner_user_id)
                            })
                            .cloned()
                    })
            })
        }

        fn request_task_finalization<'a>(
            &'a self,
            task_uid: Uuid,
            submitter_service: &'a str,
            owner_user_id: &'a str,
        ) -> BoxFuture<'a, Result<TaskRecord, StoreError>> {
            Box::pin(async move {
                let mut tasks = self.tasks.lock().map_err(|_| {
                    StoreError::Database("fake task ledger lock was poisoned".to_owned())
                })?;
                let task = tasks
                    .iter_mut()
                    .find(|task| {
                        task.task_uid == task_uid
                            && task.submitter_service == submitter_service
                            && task.owner_user_id.as_deref() == Some(owner_user_id)
                    })
                    .ok_or(StoreError::TaskNotFound)?;
                task.finalize_requested = true;
                if matches!(
                    task.phase,
                    TaskPhase::Submitted | TaskPhase::Parked | TaskPhase::Queued
                ) {
                    task.phase = TaskPhase::Cancelled;
                }
                Ok(task.clone())
            })
        }
    }

    fn runtime() -> AgentRuntime {
        let spec = AgentRuntimeSpec {
            principal: Principal::User {
                acting_user: Email("alice@example.com".to_owned()),
            },
            owner: Email("alice@example.com".to_owned()),
            canonical_authority: None,
            agent_type: AgentType {
                name: "base".to_owned(),
            },
            llms: vec![ModelRef {
                provider: "provider-a".to_owned(),
                model: "model-a".to_owned(),
            }],
            tools: Vec::new(),
            budget: Budget {
                monthly_limit: "100.00".to_owned(),
                currency: "USD".to_owned(),
            },
            ttl: Duration("24h".to_owned()),
            bindings: None,
        };
        let mut runtime = AgentRuntime::new("runtime-a", spec);
        runtime.metadata.namespace = Some("team-a".to_owned());
        runtime.metadata.uid = Some("runtime-uid-a".to_owned());
        runtime.metadata.annotations = Some(std::collections::BTreeMap::from([(
            "agents.apelogic.ai/member-role".to_owned(),
            "engineer".to_owned(),
        )]));
        runtime.metadata.resource_version = Some("1".to_owned());
        runtime
    }

    fn ledger() -> FakeLedger {
        FakeLedger {
            envelope: Arc::new(Mutex::new(Envelope {
                revision: 3,
                spec: EnvelopeSpec {
                    llms: vec![ModelRef {
                        provider: "provider-a".to_owned(),
                        model: "model-a".to_owned(),
                    }],
                    tools: Vec::new(),
                    budget: Budget {
                        monthly_limit: "200.00".to_owned(),
                        currency: "USD".to_owned(),
                    },
                    ttl: Duration("24h".to_owned()),
                },
            })),
            grants: Vec::new(),
            parked: Arc::new(Mutex::new(Vec::new())),
            decision_references: Arc::new(Mutex::new(Vec::new())),
            decision_filing_claim: Arc::new(Mutex::new(None)),
            revoke_rows: 0,
            reversion: None,
            application: Arc::new(Mutex::new(None)),
            application_committed_during_park: Arc::new(Mutex::new(None)),
            application_revoked_during_retirement: Arc::new(Mutex::new(false)),
            tasks: Arc::new(Mutex::new(Vec::new())),
            agent_runs: Arc::new(Mutex::new(Vec::new())),
            agent_run_events: Arc::new(Mutex::new(Vec::new())),
            service_envelope_authors: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn task_workflow(monthly_limit: &str) -> TaskWorkflow {
        TaskWorkflow {
            name: "code-review".to_owned(),
            namespace: "team-a".to_owned(),
            coding_agent_runtime: "agent-v1".to_owned(),
            llms: vec![ModelRef {
                provider: "provider-a".to_owned(),
                model: "model-a".to_owned(),
            }],
            tools: Vec::new(),
            budget: Budget {
                monthly_limit: monthly_limit.to_owned(),
                currency: "USD".to_owned(),
            },
            ttl: Duration("24h".to_owned()),
            command: vec!["agent-v1".to_owned()],
        }
    }

    #[derive(Clone, Default)]
    struct FlakyDecisionChannel {
        attempts: Arc<Mutex<usize>>,
    }

    impl DecisionChannel for FlakyDecisionChannel {
        async fn request(
            &self,
            _request: &DecisionRequest,
        ) -> Result<DecisionReference, PortError> {
            let mut attempts = self.attempts.lock().map_err(|_| PortError::Failed {
                reason: "flaky channel lock was poisoned".to_owned(),
            })?;
            *attempts += 1;
            if *attempts == 1 {
                return Err(PortError::Failed {
                    reason: "channel temporarily unavailable".to_owned(),
                });
            }
            Ok(DecisionReference {
                key: "PROJ-123".to_owned(),
                evidence_url: "https://jira.example.com/browse/PROJ-123".to_owned(),
            })
        }

        async fn record_resolution(
            &self,
            _resolution: &DecisionResolution,
        ) -> Result<(), PortError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn sequential_edits_are_admitted_against_the_composed_absolute_manifest()
    -> Result<(), String> {
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let ledger = ledger();
        let decisions = FakeDecisionChannel::default();
        let context = AdmissionContext {
            actor: "alice@example.com".to_owned(),
            member_role: "engineer".to_owned(),
        };
        let edit = BudgetIncrease {
            amount: "60.00".to_owned(),
        };

        let first = submit_budget_increase(
            &runtimes,
            &ledger,
            &decisions,
            &context,
            "team-a",
            "runtime-a",
            &edit,
        )
        .await;
        assert!(
            matches!(first, Ok(SubmissionOutcome::Applied { .. })),
            "the first edit should compose to 160.00 and remain inside the envelope: {first:?}"
        );
        let second = submit_budget_increase(
            &runtimes,
            &ledger,
            &decisions,
            &context,
            "team-a",
            "runtime-a",
            &edit,
        )
        .await;
        let expected_delta = AdmissionDelta::Budget {
            requested: "220.00".to_owned(),
            ceiling: "200.00".to_owned(),
            currency: "USD".to_owned(),
        };
        assert!(
            matches!(
                second,
                Ok(SubmissionOutcome::Parked {
                    ref proposed_spec,
                    ref deltas,
                    ..
                }) if proposed_spec.budget.monthly_limit == "220.00"
                    && deltas == std::slice::from_ref(&expected_delta)
            ),
            "the second edit must be evaluated as absolute 220.00 and parked: {second:?}"
        );
        let parked = ledger
            .parked
            .lock()
            .map_err(|_| "fake ledger lock was poisoned")?;
        assert_eq!(parked.len(), 1);
        assert_eq!(parked[0].runtime_uid, "runtime-uid-a");
        assert_eq!(parked[0].deltas, vec![expected_delta]);
        assert_eq!(parked[0].proposed_spec.budget.monthly_limit, "220.00");
        let envelope = ledger
            .envelope
            .lock()
            .map_err(|_| "fake envelope lock was poisoned")?
            .clone();
        assert_eq!(
            steward_admission::evaluate(&parked[0].proposed_spec, &envelope),
            Ok(AdmissionDecision::Reject {
                deltas: parked[0].deltas.clone()
            })
        );
        drop(parked);
        let requests = decisions
            .requests
            .lock()
            .map_err(|_| "fake decision-request lock was poisoned")?;
        assert_eq!(
            requests.as_slice(),
            &[DecisionRequest {
                request_id: Uuid::nil().to_string(),
                runtime_uid: "runtime-uid-a".to_owned(),
                actor: "alice@example.com".to_owned(),
                member_role: "engineer".to_owned(),
                counterexample:
                    "envelope exceeded: budget.monthlyLimit requested 220.00 USD, ceiling 200.00 USD"
                        .to_owned(),
            }],
            "a parked over-envelope request must be filed with its structured counterexample"
        );
        let references = ledger
            .decision_references
            .lock()
            .map_err(|_| "fake decision-reference lock was poisoned")?;
        assert_eq!(
            references.as_slice(),
            &[(
                Uuid::nil(),
                "PROJ-123".to_owned(),
                "https://jira.example.com/browse/PROJ-123".to_owned(),
            )],
            "the returned Jira reference must be bound to the parked approval"
        );
        Ok(())
    }

    #[tokio::test]
    async fn rest_admission_honors_the_runtime_grants_used_by_the_webhook() -> Result<(), String> {
        let mut granted_runtime = runtime();
        granted_runtime.spec.budget.monthly_limit = "220.00".to_owned();
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(granted_runtime)),
        };
        let mut ledger = ledger();
        ledger.grants = vec![AdmissionDelta::Budget {
            requested: "220.00".to_owned(),
            ceiling: "200.00".to_owned(),
            currency: "USD".to_owned(),
        }];
        let result = submit_budget_increase(
            &runtimes,
            &ledger,
            &FakeDecisionChannel::default(),
            &AdmissionContext {
                actor: "alice@example.com".to_owned(),
                member_role: "engineer".to_owned(),
            },
            "team-a",
            "runtime-a",
            &BudgetIncrease {
                amount: "0.00".to_owned(),
            },
        )
        .await;
        assert!(
            matches!(result, Ok(SubmissionOutcome::Applied { .. })),
            "the REST front door must honor the same instance grant as the webhook: {result:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn runtime_creation_rejects_principal_impersonation_before_writing_desired_state()
    -> Result<(), String> {
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let runtime_state = runtimes.runtime.clone();
        let app = router(
            runtimes,
            ledger(),
            FakeAuthenticator,
            FakeDecisionChannel::default(),
        );
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/namespaces/team-a/runtimes")
                    .header("authorization", "Bearer user-session")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "runtime-b",
                            "spec": {
                                "principal": {
                                    "kind": "user",
                                    "actingUser": "bob@example.org",
                                },
                                "owner": "bob@example.org",
                                "agentType": {"name": "base"},
                                "llms": [{
                                    "provider": "provider-a",
                                    "model": "model-a",
                                }],
                                "tools": [],
                                "budget": {
                                    "monthlyLimit": "100.00",
                                    "currency": "USD",
                                },
                                "ttl": "24h",
                            },
                        })
                        .to_string(),
                    ))
                    .map_err(|error| format!("failed to build create request: {error}"))?,
            )
            .await
            .map_err(|error| format!("create request failed: {error}"))?;
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "an authenticated member must not create a runtime for another principal"
        );
        assert_eq!(
            runtime_state
                .lock()
                .map_err(|_| "fake runtime lock was poisoned")?
                .name_any(),
            "runtime-a",
            "a rejected create must not write desired state"
        );
        Ok(())
    }

    #[tokio::test]
    async fn runtime_creation_rejects_caller_supplied_canonical_authority_before_writing()
    -> Result<(), String> {
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let runtime_state = runtimes.runtime.clone();
        let app = router(
            runtimes,
            ledger(),
            FakeAuthenticator,
            FakeDecisionChannel::default(),
        );
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/namespaces/team-a/runtimes")
                    .header("authorization", "Bearer user-session")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "runtime-b",
                            "spec": {
                                "principal": {
                                    "kind": "user",
                                    "actingUser": "alice@example.com",
                                },
                                "owner": "alice@example.com",
                                "canonicalAuthority": {
                                    "schemaVersion": "steward/canonical-authority-binding/v1",
                                    "ownerUserId": "usr_abcdef0123456789abcdef0123456789",
                                    "actingUserId": "usr_abcdef0123456789abcdef0123456789"
                                },
                                "agentType": {"name": "base"},
                                "llms": [{
                                    "provider": "provider-a",
                                    "model": "model-a",
                                }],
                                "tools": [],
                                "budget": {
                                    "monthlyLimit": "100.00",
                                    "currency": "USD",
                                },
                                "ttl": "24h",
                            },
                        })
                        .to_string(),
                    ))
                    .map_err(|error| format!("failed to build canonical injection: {error}"))?,
            )
            .await
            .map_err(|error| format!("canonical injection request failed: {error}"))?;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            runtime_state
                .lock()
                .map_err(|_| "fake runtime lock was poisoned")?
                .name_any(),
            "runtime-a",
            "caller-supplied canonical authority must not write desired state"
        );
        Ok(())
    }

    #[tokio::test]
    async fn authenticated_member_can_create_an_in_envelope_runtime() -> Result<(), String> {
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let runtime_state = runtimes.runtime.clone();
        let app = router(
            runtimes,
            ledger(),
            FakeAuthenticator,
            FakeDecisionChannel::default(),
        );
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/namespaces/team-a/runtimes")
                    .header("authorization", "Bearer user-session")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "runtime-b",
                            "spec": {
                                "principal": {
                                    "kind": "user",
                                    "actingUser": "alice@example.com",
                                },
                                "owner": "bob@example.org",
                                "agentType": {"name": "base"},
                                "llms": [{
                                    "provider": "provider-a",
                                    "model": "model-a",
                                }],
                                "tools": [],
                                "budget": {
                                    "monthlyLimit": "100.00",
                                    "currency": "USD",
                                },
                                "ttl": "24h",
                            },
                        })
                        .to_string(),
                    ))
                    .map_err(|error| format!("failed to build create request: {error}"))?,
            )
            .await
            .map_err(|error| format!("create request failed: {error}"))?;
        assert_eq!(
            response.status(),
            StatusCode::CREATED,
            "an authenticated member's in-envelope request must create desired state"
        );
        let created = runtime_state
            .lock()
            .map_err(|_| "fake runtime lock was poisoned")?;
        assert_eq!(created.name_any(), "runtime-b");
        assert_eq!(created.namespace().as_deref(), Some("team-a"));
        assert_eq!(
            created.spec.owner.0, "bob@example.org",
            "the accountable owner may differ from the acting user"
        );
        assert_eq!(
            created
                .metadata
                .annotations
                .as_ref()
                .and_then(|annotations| annotations.get("agents.apelogic.ai/member-role"))
                .map(String::as_str),
            Some("engineer"),
            "the API must bind the authenticated member role into desired state"
        );
        Ok(())
    }

    #[tokio::test]
    async fn runtime_creation_preserves_kubernetes_client_error_statuses() -> Result<(), String> {
        for expected in [
            StatusCode::NOT_FOUND,
            StatusCode::CONFLICT,
            StatusCode::UNPROCESSABLE_ENTITY,
        ] {
            let app = router(
                RejectedCreateRepository {
                    runtime: runtime(),
                    status: expected.as_u16(),
                },
                ledger(),
                FakeAuthenticator,
                FakeDecisionChannel::default(),
            );
            let response = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/namespaces/team-a/runtimes")
                        .header("authorization", "Bearer user-session")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::json!({
                                "name": "runtime-b",
                                "spec": {
                                    "principal": {
                                        "kind": "user",
                                        "actingUser": "alice@example.com",
                                    },
                                    "owner": "alice@example.com",
                                    "agentType": {"name": "base"},
                                    "llms": [{
                                        "provider": "provider-a",
                                        "model": "model-a",
                                    }],
                                    "tools": [],
                                    "budget": {
                                        "monthlyLimit": "100.00",
                                        "currency": "USD",
                                    },
                                    "ttl": "24h",
                                },
                            })
                            .to_string(),
                        ))
                        .map_err(|error| format!("failed to build create request: {error}"))?,
                )
                .await
                .map_err(|error| format!("create request failed: {error}"))?;
            assert_eq!(
                response.status(),
                expected,
                "a deterministic Kubernetes client error must not be reported as a retryable API outage"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn runtime_creation_parks_an_over_envelope_manifest_without_provisioning_it()
    -> Result<(), String> {
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let runtime_state = runtimes.runtime.clone();
        let ledger = ledger();
        let parked_state = ledger.parked.clone();
        let decisions = FakeDecisionChannel::default();
        let decision_requests = decisions.requests.clone();
        let app = router(runtimes, ledger, FakeAuthenticator, decisions);
        let body = serde_json::json!({
            "name": "runtime-b",
            "spec": {
                "principal": {
                    "kind": "user",
                    "actingUser": "alice@example.com",
                },
                "owner": "alice@example.com",
                "agentType": {"name": "base"},
                "llms": [{
                    "provider": "provider-a",
                    "model": "model-a",
                }],
                "tools": [],
                "budget": {
                    "monthlyLimit": "201.00",
                    "currency": "USD",
                },
                "ttl": "24h",
            },
        })
        .to_string();
        let request = || {
            Request::builder()
                .method("POST")
                .uri("/v1/namespaces/team-a/runtimes")
                .header("authorization", "Bearer user-session")
                .header("content-type", "application/json")
                .body(Body::from(body.clone()))
                .map_err(|error| format!("failed to build create request: {error}"))
        };
        let response = app
            .clone()
            .oneshot(request()?)
            .await
            .map_err(|error| format!("create request failed: {error}"))?;
        assert_eq!(
            response.status(),
            StatusCode::ACCEPTED,
            "an over-envelope create must be parked by the REST admission door"
        );
        {
            let pending = runtime_state
                .lock()
                .map_err(|_| "fake runtime lock was poisoned")?;
            assert_eq!(pending.name_any(), "runtime-b");
            assert!(
                pending.spec.llms.is_empty()
                    && pending.spec.tools.is_empty()
                    && pending.spec.budget.monthly_limit == "0",
                "the persisted placeholder must carry no usable model, tool, or budget authority"
            );
            assert!(
                pending
                    .annotations()
                    .contains_key("agents.apelogic.ai/pending-approval"),
                "the controller must be able to hold the placeholder inert until approval"
            );
        }
        assert_eq!(
            parked_state
                .lock()
                .map_err(|_| "fake parked lock was poisoned")?
                .len(),
            1,
            "the rejected initial manifest must create one instance-bound approval"
        );
        assert_eq!(
            decision_requests
                .lock()
                .map_err(|_| "fake decision-request lock was poisoned")?
                .len(),
            1,
            "the parked initial manifest must be filed with the decision channel"
        );
        let replay = app
            .oneshot(request()?)
            .await
            .map_err(|error| format!("replayed create request failed: {error}"))?;
        assert_eq!(
            replay.status(),
            StatusCode::ACCEPTED,
            "a retry after the inert placeholder was created must resume the same parked request"
        );
        assert_eq!(
            parked_state
                .lock()
                .map_err(|_| "fake parked lock was poisoned")?
                .len(),
            1,
            "retrying an initial create must not create a second approval"
        );
        assert_eq!(
            decision_requests
                .lock()
                .map_err(|_| "fake decision-request lock was poisoned")?
                .len(),
            1,
            "retrying a filed initial create must not create another external decision"
        );
        Ok(())
    }

    #[tokio::test]
    async fn runtime_creation_retry_reuses_an_approved_unapplied_request() -> Result<(), String> {
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let runtime_state = runtimes.runtime.clone();
        let ledger = ledger();
        let decisions = FakeDecisionChannel::default();
        let context = AdmissionContext {
            actor: "alice@example.com".to_owned(),
            member_role: "engineer".to_owned(),
        };
        let mut proposed_spec = runtime().spec;
        proposed_spec.budget.monthly_limit = "201.00".to_owned();
        let request = CreateRuntimeRequest {
            name: "runtime-b".to_owned(),
            spec: proposed_spec.clone(),
        };

        let first = super::submit_runtime_request(
            &runtimes, &ledger, &decisions, &context, "team-a", &request,
        )
        .await
        .map_err(|error| format!("initial create was not parked: {error:?}"))?;
        assert!(
            matches!(first, SubmissionOutcome::Parked { .. }),
            "the over-envelope create must be parked before approval"
        );
        let placeholder = runtime_state
            .lock()
            .map_err(|_| "fake runtime lock was poisoned")?
            .clone();
        *ledger
            .application
            .lock()
            .map_err(|_| "fake approved-application lock was poisoned")? = Some(GrantReversion {
            runtime_uid: placeholder
                .metadata
                .uid
                .clone()
                .ok_or_else(|| "placeholder is missing its runtime UID".to_owned())?,
            runtime_namespace: "team-a".to_owned(),
            runtime_name: "runtime-b".to_owned(),
            actor: context.actor.clone(),
            member_role: context.member_role.clone(),
            base_pending_approval_digest: placeholder
                .annotations()
                .get(PENDING_APPROVAL_ANNOTATION)
                .cloned(),
            base_spec: placeholder.spec.clone(),
            proposed_spec: proposed_spec.clone(),
        });

        let retry = super::submit_runtime_request(
            &runtimes, &ledger, &decisions, &context, "team-a", &request,
        )
        .await
        .map_err(|error| format!("approved create retry failed: {error:?}"))?;

        assert!(
            matches!(retry, SubmissionOutcome::Applied { .. }),
            "an approved-but-unapplied create must converge instead of parking another approval: {retry:?}"
        );
        assert_eq!(
            ledger
                .parked
                .lock()
                .map_err(|_| "fake parked lock was poisoned")?
                .len(),
            0,
            "recovery must retire the durable parked loser before applying its winning approval"
        );
        let restored = runtime_state
            .lock()
            .map_err(|_| "fake runtime lock was poisoned")?;
        assert_eq!(restored.spec, proposed_spec);
        assert!(
            !restored
                .annotations()
                .contains_key("agents.apelogic.ai/pending-approval"),
            "recovery must clear the inert-placeholder marker"
        );
        Ok(())
    }

    #[tokio::test]
    async fn runtime_creation_retry_recovers_when_approval_commits_during_parking()
    -> Result<(), String> {
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let runtime_state = runtimes.runtime.clone();
        let ledger = ledger();
        let decisions = FakeDecisionChannel::default();
        let context = AdmissionContext {
            actor: "alice@example.com".to_owned(),
            member_role: "engineer".to_owned(),
        };
        let mut proposed_spec = runtime().spec;
        proposed_spec.budget.monthly_limit = "201.00".to_owned();
        let request = CreateRuntimeRequest {
            name: "runtime-b".to_owned(),
            spec: proposed_spec.clone(),
        };

        let first = super::submit_runtime_request(
            &runtimes, &ledger, &decisions, &context, "team-a", &request,
        )
        .await
        .map_err(|error| format!("initial create was not parked: {error:?}"))?;
        assert!(matches!(first, SubmissionOutcome::Parked { .. }));
        let placeholder = runtime_state
            .lock()
            .map_err(|_| "fake runtime lock was poisoned")?
            .clone();
        *ledger
            .application_committed_during_park
            .lock()
            .map_err(|_| "fake approval-race lock was poisoned")? = Some(GrantReversion {
            runtime_uid: placeholder
                .metadata
                .uid
                .clone()
                .ok_or_else(|| "placeholder is missing its runtime UID".to_owned())?,
            runtime_namespace: "team-a".to_owned(),
            runtime_name: "runtime-b".to_owned(),
            actor: context.actor.clone(),
            member_role: context.member_role.clone(),
            base_pending_approval_digest: placeholder
                .annotations()
                .get(PENDING_APPROVAL_ANNOTATION)
                .cloned(),
            base_spec: placeholder.spec,
            proposed_spec: proposed_spec.clone(),
        });

        let retry = super::submit_runtime_request(
            &runtimes, &ledger, &decisions, &context, "team-a", &request,
        )
        .await
        .map_err(|error| {
            format!("concurrent approval must be recovered instead of surfacing a store error: {error:?}")
        })?;

        assert!(
            matches!(retry, SubmissionOutcome::Applied { .. }),
            "the retry must converge the approval that won the parking race: {retry:?}"
        );
        assert_eq!(
            ledger
                .parked
                .lock()
                .map_err(|_| "fake parked lock was poisoned")?
                .len(),
            0,
            "the losing park attempt must retire its newly created pending approval"
        );
        Ok(())
    }

    #[tokio::test]
    async fn runtime_creation_converges_when_approval_commits_during_decision_filing()
    -> Result<(), String> {
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let ledger = ledger();
        let context = AdmissionContext {
            actor: "alice@example.com".to_owned(),
            member_role: "engineer".to_owned(),
        };
        let mut proposed_spec = runtime().spec;
        proposed_spec.budget.monthly_limit = "201.00".to_owned();
        let mut placeholder_spec = proposed_spec.clone();
        placeholder_spec.llms.clear();
        placeholder_spec.tools.clear();
        placeholder_spec.budget.monthly_limit = "0".to_owned();
        placeholder_spec.bindings = None;
        let decisions = ApprovalDuringDecisionFiling {
            application_slot: ledger.application.clone(),
            application: GrantReversion {
                runtime_uid: "runtime-b-uid".to_owned(),
                runtime_namespace: "team-a".to_owned(),
                runtime_name: "runtime-b".to_owned(),
                actor: context.actor.clone(),
                member_role: context.member_role.clone(),
                base_pending_approval_digest: Some(
                    spec_digest(&proposed_spec)
                        .map_err(|error| format!("failed to digest approved spec: {error:?}"))?,
                ),
                base_spec: placeholder_spec,
                proposed_spec: proposed_spec.clone(),
            },
        };
        let request = CreateRuntimeRequest {
            name: "runtime-b".to_owned(),
            spec: proposed_spec,
        };

        let outcome = super::submit_runtime_request(
            &runtimes, &ledger, &decisions, &context, "team-a", &request,
        )
        .await
        .map_err(|error| format!("decision-filing race failed: {error:?}"))?;

        assert!(
            matches!(outcome, SubmissionOutcome::Applied { .. }),
            "an approval committed during decision filing must converge in the same request: {outcome:?}"
        );
        assert_eq!(
            ledger
                .parked
                .lock()
                .map_err(|_| "fake parked lock was poisoned")?
                .len(),
            0,
            "the approval that lost during decision filing must be terminal before convergence"
        );
        Ok(())
    }

    #[tokio::test]
    async fn runtime_creation_does_not_retire_the_loser_after_winner_revocation()
    -> Result<(), String> {
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let runtime_state = runtimes.runtime.clone();
        let ledger = ledger();
        let decisions = FakeDecisionChannel::default();
        let context = AdmissionContext {
            actor: "alice@example.com".to_owned(),
            member_role: "engineer".to_owned(),
        };
        let mut proposed_spec = runtime().spec;
        proposed_spec.budget.monthly_limit = "201.00".to_owned();
        let request = CreateRuntimeRequest {
            name: "runtime-b".to_owned(),
            spec: proposed_spec.clone(),
        };
        let first = super::submit_runtime_request(
            &runtimes, &ledger, &decisions, &context, "team-a", &request,
        )
        .await
        .map_err(|error| format!("initial create was not parked: {error:?}"))?;
        assert!(matches!(first, SubmissionOutcome::Parked { .. }));
        let placeholder = runtime_state
            .lock()
            .map_err(|_| "fake runtime lock was poisoned")?
            .clone();
        *ledger
            .application_committed_during_park
            .lock()
            .map_err(|_| "fake approval-race lock was poisoned")? = Some(GrantReversion {
            runtime_uid: placeholder
                .metadata
                .uid
                .clone()
                .ok_or_else(|| "placeholder is missing its runtime UID".to_owned())?,
            runtime_namespace: "team-a".to_owned(),
            runtime_name: "runtime-b".to_owned(),
            actor: context.actor.clone(),
            member_role: context.member_role.clone(),
            base_pending_approval_digest: placeholder
                .annotations()
                .get(PENDING_APPROVAL_ANNOTATION)
                .cloned(),
            base_spec: placeholder.spec,
            proposed_spec,
        });
        *ledger
            .application_revoked_during_retirement
            .lock()
            .map_err(|_| "fake retirement-race lock was poisoned")? = true;

        let retry = super::submit_runtime_request(
            &runtimes, &ledger, &decisions, &context, "team-a", &request,
        )
        .await
        .map_err(|error| format!("revoked-winner retry failed: {error:?}"))?;

        assert!(
            matches!(retry, SubmissionOutcome::Parked { .. }),
            "a winner revoked before retirement must leave the current escalation parked: {retry:?}"
        );
        assert_eq!(
            ledger
                .parked
                .lock()
                .map_err(|_| "fake parked lock was poisoned")?
                .len(),
            1,
            "the valid escalation must remain pending when its alleged winner is inactive"
        );
        Ok(())
    }

    #[tokio::test]
    async fn runtime_creation_preserves_an_in_flight_loser_filing_lease() -> Result<(), String> {
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let runtime_state = runtimes.runtime.clone();
        let ledger = ledger();
        let decisions = FakeDecisionChannel::default();
        let context = AdmissionContext {
            actor: "alice@example.com".to_owned(),
            member_role: "engineer".to_owned(),
        };
        let mut proposed_spec = runtime().spec;
        proposed_spec.budget.monthly_limit = "201.00".to_owned();
        let request = CreateRuntimeRequest {
            name: "runtime-b".to_owned(),
            spec: proposed_spec.clone(),
        };
        let first = super::submit_runtime_request(
            &runtimes, &ledger, &decisions, &context, "team-a", &request,
        )
        .await
        .map_err(|error| format!("initial create was not parked: {error:?}"))?;
        assert!(matches!(first, SubmissionOutcome::Parked { .. }));
        let placeholder = runtime_state
            .lock()
            .map_err(|_| "fake runtime lock was poisoned")?
            .clone();
        *ledger
            .application_committed_during_park
            .lock()
            .map_err(|_| "fake approval-race lock was poisoned")? = Some(GrantReversion {
            runtime_uid: placeholder
                .metadata
                .uid
                .clone()
                .ok_or_else(|| "placeholder is missing its runtime UID".to_owned())?,
            runtime_namespace: "team-a".to_owned(),
            runtime_name: "runtime-b".to_owned(),
            actor: context.actor.clone(),
            member_role: context.member_role.clone(),
            base_pending_approval_digest: placeholder
                .annotations()
                .get(PENDING_APPROVAL_ANNOTATION)
                .cloned(),
            base_spec: placeholder.spec,
            proposed_spec,
        });
        *ledger
            .decision_filing_claim
            .lock()
            .map_err(|_| "fake filing claim lock was poisoned")? = Some(Uuid::new_v4());

        let retry = super::submit_runtime_request(
            &runtimes, &ledger, &decisions, &context, "team-a", &request,
        )
        .await
        .map_err(|error| format!("filing-race retry failed: {error:?}"))?;

        assert!(
            matches!(retry, SubmissionOutcome::Applied { .. }),
            "an in-flight filing must not keep a superseded approval live: {retry:?}"
        );
        assert_eq!(
            ledger
                .parked
                .lock()
                .map_err(|_| "fake parked lock was poisoned")?
                .len(),
            0,
            "the superseded approval must become terminal while its filing lease finishes"
        );
        Ok(())
    }

    #[tokio::test]
    async fn approving_an_initial_create_replaces_and_unblocks_its_placeholder()
    -> Result<(), String> {
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let runtime_state = runtimes.runtime.clone();
        let ledger = ledger();
        let mut proposed_spec = runtime().spec;
        proposed_spec.budget.monthly_limit = "201.00".to_owned();
        let outcome = super::submit_runtime_request(
            &runtimes,
            &ledger,
            &FakeDecisionChannel::default(),
            &AdmissionContext {
                actor: "alice@example.com".to_owned(),
                member_role: "engineer".to_owned(),
            },
            "team-a",
            &CreateRuntimeRequest {
                name: "runtime-b".to_owned(),
                spec: proposed_spec.clone(),
            },
        )
        .await
        .map_err(|error| format!("initial create was not parked: {error:?}"))?;
        let SubmissionOutcome::Parked { approval_id, .. } = outcome else {
            return Err("over-envelope initial create was not parked".to_owned());
        };

        super::approve_parked_request(
            &runtimes,
            &ledger,
            &FakeDecisionChannel::default(),
            &super::AdminContext {
                actor: "admin@example.com".to_owned(),
            },
            approval_id,
            &super::ApprovalRequest {
                rationale: "approved for this runtime".to_owned(),
                evidence_url: "https://jira.example.com/browse/PROJ-123".to_owned(),
                expires_at: "2999-01-01T00:00:00Z".to_owned(),
            },
        )
        .await
        .map_err(|error| format!("initial create approval failed: {error:?}"))?;

        let approved = runtime_state
            .lock()
            .map_err(|_| "fake runtime lock was poisoned")?;
        assert_eq!(approved.spec, proposed_spec);
        assert!(
            !approved
                .annotations()
                .contains_key("agents.apelogic.ai/pending-approval"),
            "approval must remove the controller's inert-placeholder hold"
        );
        Ok(())
    }

    #[tokio::test]
    async fn approval_rejects_a_placeholder_with_mismatched_stored_provenance() -> Result<(), String>
    {
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let runtime_state = runtimes.runtime.clone();
        let ledger = ledger();
        let mut proposed_spec = runtime().spec;
        proposed_spec.budget.monthly_limit = "201.00".to_owned();
        let outcome = super::submit_runtime_request(
            &runtimes,
            &ledger,
            &FakeDecisionChannel::default(),
            &AdmissionContext {
                actor: "alice@example.com".to_owned(),
                member_role: "engineer".to_owned(),
            },
            "team-a",
            &CreateRuntimeRequest {
                name: "runtime-b".to_owned(),
                spec: proposed_spec.clone(),
            },
        )
        .await
        .map_err(|error| format!("initial create was not parked: {error:?}"))?;
        let SubmissionOutcome::Parked { approval_id, .. } = outcome else {
            return Err("over-envelope initial create was not parked".to_owned());
        };
        runtime_state
            .lock()
            .map_err(|_| "fake runtime lock was poisoned")?
            .metadata
            .annotations
            .get_or_insert_default()
            .insert(
                PENDING_APPROVAL_ANNOTATION.to_owned(),
                "different-request-digest".to_owned(),
            );

        let result = super::approve_parked_request(
            &runtimes,
            &ledger,
            &FakeDecisionChannel::default(),
            &super::AdminContext {
                actor: "admin@example.com".to_owned(),
            },
            approval_id,
            &super::ApprovalRequest {
                rationale: "approved for this runtime".to_owned(),
                evidence_url: "https://jira.example.com/browse/PROJ-123".to_owned(),
                expires_at: "2999-01-01T00:00:00Z".to_owned(),
            },
        )
        .await;

        assert!(
            matches!(result, Err(ApiError::Conflict(_))),
            "approval must not release a placeholder whose marker no longer matches: {result:?}"
        );
        assert_ne!(
            runtime_state
                .lock()
                .map_err(|_| "fake runtime lock was poisoned")?
                .spec,
            proposed_spec,
            "mismatched provenance must leave the inert placeholder unchanged"
        );
        Ok(())
    }

    #[tokio::test]
    async fn envelope_expansion_retry_releases_the_matching_pending_create() -> Result<(), String> {
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let runtime_state = runtimes.runtime.clone();
        let ledger = ledger();
        let context = AdmissionContext {
            actor: "alice@example.com".to_owned(),
            member_role: "engineer".to_owned(),
        };
        let mut proposed_spec = runtime().spec;
        proposed_spec.budget.monthly_limit = "201.00".to_owned();
        let request = CreateRuntimeRequest {
            name: "runtime-b".to_owned(),
            spec: proposed_spec.clone(),
        };

        let parked = super::submit_runtime_request(
            &runtimes,
            &ledger,
            &FakeDecisionChannel::default(),
            &context,
            "team-a",
            &request,
        )
        .await
        .map_err(|error| format!("initial create did not park: {error:?}"))?;
        assert!(matches!(parked, SubmissionOutcome::Parked { .. }));
        {
            let mut envelope = ledger
                .envelope
                .lock()
                .map_err(|_| "fake envelope lock was poisoned")?;
            envelope.revision += 1;
            envelope.spec.budget.monthly_limit = "300.00".to_owned();
        }

        let retry = super::submit_runtime_request(
            &runtimes,
            &ledger,
            &FakeDecisionChannel::default(),
            &context,
            "team-a",
            &request,
        )
        .await;

        assert!(
            matches!(retry, Ok(SubmissionOutcome::Applied { .. })),
            "an expanded envelope must release its matching hold instead of returning 409: {retry:?}"
        );
        let restored = runtime_state
            .lock()
            .map_err(|_| "fake runtime lock was poisoned")?;
        assert_eq!(restored.spec, proposed_spec);
        assert!(
            !restored
                .annotations()
                .contains_key(PENDING_APPROVAL_ANNOTATION),
            "envelope admission must remove the pending hold"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_repeated_member_role_group_is_one_authenticated_role() -> Result<(), String> {
        let app = router(
            FakeRuntimeRepository {
                runtime: Arc::new(Mutex::new(runtime())),
            },
            ledger(),
            FakeAuthenticator,
            FakeDecisionChannel::default(),
        );
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/v1/namespaces/team-a/runtimes/runtime-a/budget")
                    .header("authorization", "Bearer user-duplicate-role-session")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"amount":"1.00"}"#))
                    .map_err(|error| format!("failed to build duplicate-role request: {error}"))?,
            )
            .await
            .map_err(|error| format!("duplicate-role request failed: {error}"))?;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "repeated copies of the same authenticated group must match webhook semantics"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_channel_outage_can_be_retried_without_duplicate_approvals() -> Result<(), String> {
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let ledger = ledger();
        let decisions = FlakyDecisionChannel::default();
        let context = AdmissionContext {
            actor: "alice@example.com".to_owned(),
            member_role: "engineer".to_owned(),
        };
        let edit = BudgetIncrease {
            amount: "120.00".to_owned(),
        };
        let first = submit_budget_increase(
            &runtimes,
            &ledger,
            &decisions,
            &context,
            "team-a",
            "runtime-a",
            &edit,
        )
        .await;
        assert!(
            matches!(first, Err(ApiError::DecisionChannel(_))),
            "the first channel outage must fail closed: {first:?}"
        );
        let second = submit_budget_increase(
            &runtimes,
            &ledger,
            &decisions,
            &context,
            "team-a",
            "runtime-a",
            &edit,
        )
        .await;
        assert!(
            matches!(second, Ok(SubmissionOutcome::Parked { .. })),
            "resubmitting the same rejected manifest must retry channel filing: {second:?}"
        );
        let third = submit_budget_increase(
            &runtimes,
            &ledger,
            &decisions,
            &context,
            "team-a",
            "runtime-a",
            &edit,
        )
        .await;
        assert!(
            matches!(third, Ok(SubmissionOutcome::Parked { .. })),
            "a filed pending approval must remain an idempotent submission: {third:?}"
        );
        assert_eq!(
            ledger
                .parked
                .lock()
                .map_err(|_| "fake ledger lock was poisoned")?
                .len(),
            1,
            "channel recovery must reuse one durable approval"
        );
        assert_eq!(
            *decisions
                .attempts
                .lock()
                .map_err(|_| "flaky channel lock was poisoned")?,
            2,
            "a retry after the durable decision reference exists must not create another issue"
        );
        Ok(())
    }

    #[tokio::test]
    async fn an_unfiled_approval_has_an_authenticated_recovery_route() -> Result<(), String> {
        let ledger = ledger();
        let decisions = FlakyDecisionChannel::default();
        let app = router(
            FakeRuntimeRepository {
                runtime: Arc::new(Mutex::new(runtime())),
            },
            ledger,
            FakeAuthenticator,
            decisions.clone(),
        );
        let budget_request = || {
            Request::builder()
                .method("PATCH")
                .uri("/v1/namespaces/team-a/runtimes/runtime-a/budget")
                .header("authorization", "Bearer user-session")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"amount":"120.00"}"#))
                .map_err(|error| format!("failed to build budget request: {error}"))
        };
        let outage = app
            .clone()
            .oneshot(budget_request()?)
            .await
            .map_err(|error| format!("budget outage request failed: {error}"))?;
        assert_eq!(outage.status(), StatusCode::SERVICE_UNAVAILABLE);
        let filed = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/approvals/00000000-0000-0000-0000-000000000000/file")
                    .header("authorization", "Bearer admin-session")
                    .body(Body::empty())
                    .map_err(|error| format!("failed to build filing request: {error}"))?,
            )
            .await
            .map_err(|error| format!("filing request failed: {error}"))?;
        assert_eq!(
            filed.status(),
            StatusCode::OK,
            "an administrator must be able to finish a decision-channel write after a crash window"
        );
        let retried = app
            .oneshot(budget_request()?)
            .await
            .map_err(|error| format!("retried budget request failed: {error}"))?;
        assert_eq!(retried.status(), StatusCode::ACCEPTED);
        assert_eq!(
            *decisions
                .attempts
                .lock()
                .map_err(|_| "flaky channel lock was poisoned")?,
            2,
            "the recovered durable reference must prevent a second external decision request"
        );
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_retries_file_exactly_one_external_decision() -> Result<(), String> {
        let ledger = ledger();
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let parked = submit_budget_increase(
            &runtimes,
            &ledger,
            &FlakyDecisionChannel::default(),
            &AdmissionContext {
                actor: "alice@example.com".to_owned(),
                member_role: "engineer".to_owned(),
            },
            "team-a",
            "runtime-a",
            &BudgetIncrease {
                amount: "120.00".to_owned(),
            },
        )
        .await;
        assert!(matches!(parked, Err(ApiError::DecisionChannel(_))));
        let decisions = SlowDecisionChannel::default();
        let (left, right) = tokio::join!(
            super::file_decision_reference(&ledger, &decisions, Uuid::nil()),
            super::file_decision_reference(&ledger, &decisions, Uuid::nil()),
        );
        assert!(
            left.is_ok() || right.is_ok(),
            "one filing attempt must complete successfully"
        );
        assert_eq!(
            decisions.requests.load(Ordering::SeqCst),
            1,
            "a durable filing claim must serialize the outbound decision-channel call"
        );
        Ok(())
    }

    #[tokio::test]
    async fn stale_envelope_blocks_approval_before_desired_state_changes() -> Result<(), String> {
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let runtime_state = runtimes.runtime.clone();
        let ledger = ledger();
        let parked = submit_budget_increase(
            &runtimes,
            &ledger,
            &FakeDecisionChannel::default(),
            &AdmissionContext {
                actor: "alice@example.com".to_owned(),
                member_role: "engineer".to_owned(),
            },
            "team-a",
            "runtime-a",
            &BudgetIncrease {
                amount: "120.00".to_owned(),
            },
        )
        .await;
        assert!(matches!(parked, Ok(SubmissionOutcome::Parked { .. })));
        ledger
            .envelope
            .lock()
            .map_err(|_| "fake envelope lock was poisoned")?
            .revision = 4;
        let result = super::approve_parked_request(
            &runtimes,
            &ledger,
            &FakeDecisionChannel::default(),
            &super::AdminContext {
                actor: "admin@example.com".to_owned(),
            },
            Uuid::nil(),
            &super::ApprovalRequest {
                rationale: "old authority must not cross revisions".to_owned(),
                evidence_url: "https://jira.example.com/browse/PROJ-123".to_owned(),
                expires_at: "2999-01-01T00:00:00Z".to_owned(),
            },
        )
        .await;
        assert!(
            matches!(result, Err(ApiError::Conflict(_))),
            "an approval against a superseded envelope must fail closed: {result:?}"
        );
        assert_eq!(
            runtime_state
                .lock()
                .map_err(|_| "fake runtime lock was poisoned")?
                .spec
                .budget
                .monthly_limit,
            "100.00"
        );
        Ok(())
    }

    #[tokio::test]
    async fn admin_faults_are_not_reported_as_retryable_outages() -> Result<(), String> {
        let app = router(
            FakeRuntimeRepository {
                runtime: Arc::new(Mutex::new(runtime())),
            },
            ledger(),
            FakeAuthenticator,
            FakeDecisionChannel::default(),
        );
        let missing_approval = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/approvals/00000000-0000-0000-0000-000000000000/approve")
                    .header("authorization", "Bearer admin-session")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"rationale":"bounded exception","evidenceUrl":"https://jira.example.com/browse/PROJ-123","expiresAt":"2999-01-01T00:00:00Z"}"#,
                    ))
                    .map_err(|error| format!("failed to build missing approval request: {error}"))?,
            )
            .await
            .map_err(|error| format!("missing approval request failed: {error}"))?;
        assert_eq!(missing_approval.status(), StatusCode::NOT_FOUND);

        let no_grants = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/runtimes/runtime-uid-a/grants/revoke")
                    .header("authorization", "Bearer admin-session")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"reason":"scope ended"}"#))
                    .map_err(|error| format!("failed to build empty revoke request: {error}"))?,
            )
            .await
            .map_err(|error| format!("empty revoke request failed: {error}"))?;
        assert_eq!(
            no_grants.status(),
            StatusCode::NOT_FOUND,
            "revoking a runtime with no active grant must not claim success"
        );
        Ok(())
    }

    #[tokio::test]
    async fn revocation_restores_an_unchanged_escalated_runtime() -> Result<(), String> {
        let base = runtime();
        let mut escalated = base.clone();
        escalated.spec.budget.monthly_limit = "220.00".to_owned();
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(escalated.clone())),
        };
        let runtime_state = runtimes.runtime.clone();
        let mut ledger = ledger();
        ledger.revoke_rows = 1;
        ledger.reversion = Some(GrantReversion {
            runtime_uid: "runtime-uid-a".to_owned(),
            runtime_namespace: "team-a".to_owned(),
            runtime_name: "runtime-a".to_owned(),
            actor: "alice@example.com".to_owned(),
            member_role: "engineer".to_owned(),
            base_spec: base.spec,
            proposed_spec: escalated.spec,
            base_pending_approval_digest: None,
        });
        let response = router(
            runtimes,
            ledger,
            FakeAuthenticator,
            FakeDecisionChannel::default(),
        )
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/runtimes/runtime-uid-a/grants/revoke")
                .header("authorization", "Bearer admin-session")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"reason":"scope ended"}"#))
                .map_err(|error| format!("failed to build revoke request: {error}"))?,
        )
        .await
        .map_err(|error| format!("revoke request failed: {error}"))?;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            runtime_state
                .lock()
                .map_err(|_| "fake runtime lock was poisoned")?
                .spec
                .budget
                .monthly_limit,
            "100.00",
            "revoking an active grant must remove its already-applied authority"
        );
        assert!(
            !runtime_state
                .lock()
                .map_err(|_| "fake runtime lock was poisoned")?
                .annotations()
                .contains_key(PENDING_APPROVAL_ANNOTATION),
            "edit-grant reversion must not add a pending marker"
        );
        Ok(())
    }

    #[tokio::test]
    async fn initial_create_revocation_restores_its_exact_pending_marker() -> Result<(), String> {
        let base = runtime();
        let mut applied = base.clone();
        applied.spec.budget.monthly_limit = "220.00".to_owned();
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(applied.clone())),
        };
        let runtime_state = runtimes.runtime.clone();
        let mut ledger = ledger();
        ledger.reversion = Some(GrantReversion {
            runtime_uid: "runtime-uid-a".to_owned(),
            runtime_namespace: "team-a".to_owned(),
            runtime_name: "runtime-a".to_owned(),
            actor: "alice@example.com".to_owned(),
            member_role: "engineer".to_owned(),
            base_spec: base.spec.clone(),
            proposed_spec: applied.spec,
            base_pending_approval_digest: Some("request-digest".to_owned()),
        });

        super::reconcile_grant_reversion(&runtimes, &ledger, "runtime-uid-a")
            .await
            .map_err(|error| format!("initial-create reversion failed: {error:?}"))?;

        let restored = runtime_state
            .lock()
            .map_err(|_| "fake runtime lock was poisoned")?;
        assert_eq!(restored.spec, base.spec);
        assert_eq!(
            restored
                .annotations()
                .get(PENDING_APPROVAL_ANNOTATION)
                .map(String::as_str),
            Some("request-digest"),
            "API recovery must restore the durable pending marker verbatim"
        );
        Ok(())
    }

    #[tokio::test]
    async fn privileged_routes_reject_missing_authentication() -> Result<(), String> {
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let app = router(
            runtimes,
            ledger(),
            FakeAuthenticator,
            FakeDecisionChannel::default(),
        );

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/approvals")
                    .body(Body::empty())
                    .map_err(|error| format!("failed to build unauthenticated request: {error}"))?,
            )
            .await
            .map_err(|error| format!("unauthenticated request failed: {error}"))?;

        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "a router with no authenticated caller must reject privileged routes explicitly"
        );
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/approvals")
                    .header("authorization", "Bearer user-session")
                    .body(Body::empty())
                    .map_err(|error| {
                        format!("failed to build unauthorized-admin request: {error}")
                    })?,
            )
            .await
            .map_err(|error| format!("unauthorized-admin request failed: {error}"))?;
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "an authenticated non-admin must not reach privileged admin routes"
        );
        for (authorization, expected) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some("Bearer user-session"), StatusCode::FORBIDDEN),
        ] {
            for (uri, body) in [
                (
                    "/admin/approvals/00000000-0000-0000-0000-000000000000/approve",
                    r#"{"rationale":"not authorized","evidenceUrl":"https://jira.example.com/browse/PROJ-123"}"#,
                ),
                (
                    "/admin/approvals/00000000-0000-0000-0000-000000000000/file",
                    "{}",
                ),
                (
                    "/admin/runtimes/runtime-uid-a/grants/revoke",
                    r#"{"reason":"not authorized"}"#,
                ),
                (
                    "/admin/service-envelopes/scheduled-scanner",
                    r#"{"revision":1,"spec":{"llms":[],"tools":[],"budget":{"monthlyLimit":"1.00","currency":"USD"},"ttl":"1h"}}"#,
                ),
            ] {
                let mut request = Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json");
                if let Some(authorization) = authorization {
                    request = request.header("authorization", authorization);
                }
                let response = app
                    .clone()
                    .oneshot(request.body(Body::from(body)).map_err(|error| {
                        format!("failed to build unauthorized grant request: {error}")
                    })?)
                    .await
                    .map_err(|error| format!("unauthorized grant request failed: {error}"))?;
                assert_eq!(
                    response.status(),
                    expected,
                    "every grant-authority route must enforce admin authentication: {uri}"
                );
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn service_envelope_bootstrap_authority_is_denied_every_other_admin_route()
    -> Result<(), String> {
        let ledger = ledger();
        let envelope_body = serde_json::to_vec(
            &*ledger
                .envelope
                .lock()
                .map_err(|_| "fake envelope lock was poisoned")?,
        )
        .map_err(|error| format!("failed to serialize service envelope: {error}"))?;
        let app = router(
            FakeRuntimeRepository {
                runtime: Arc::new(Mutex::new(runtime())),
            },
            ledger,
            BootstrapAuthenticator,
            FakeDecisionChannel::default(),
        );

        for (method, uri, body) in [
            ("GET", "/admin/approvals", ""),
            (
                "POST",
                "/admin/envelopes/engineer",
                std::str::from_utf8(&envelope_body)
                    .map_err(|error| format!("envelope body was not UTF-8: {error}"))?,
            ),
            (
                "POST",
                "/admin/service-envelopes/other-service",
                std::str::from_utf8(&envelope_body)
                    .map_err(|error| format!("envelope body was not UTF-8: {error}"))?,
            ),
            (
                "PUT",
                "/admin/service-envelopes/steward-run",
                std::str::from_utf8(&envelope_body)
                    .map_err(|error| format!("envelope body was not UTF-8: {error}"))?,
            ),
            (
                "POST",
                "/admin/approvals/00000000-0000-0000-0000-000000000000/approve",
                r#"{"rationale":"not authorized","evidenceUrl":"https://jira.example.com/browse/PROJ-123","expiresAt":"2099-01-01T00:00:00Z"}"#,
            ),
            (
                "POST",
                "/admin/approvals/00000000-0000-0000-0000-000000000000/file",
                "{}",
            ),
            (
                "POST",
                "/admin/runtimes/runtime-uid-a/grants/revoke",
                r#"{"reason":"not authorized"}"#,
            ),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(uri)
                        .header("authorization", "Bearer bootstrap-session")
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_owned()))
                        .map_err(|error| format!("failed to build bootstrap request: {error}"))?,
                )
                .await
                .map_err(|error| format!("bootstrap request failed: {error}"))?;
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "service-envelope bootstrap authority must be denied on {method} {uri}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn service_envelope_bootstrap_authority_can_write_only_its_envelope_and_audits_username()
    -> Result<(), String> {
        let ledger = ledger();
        let authors = ledger.service_envelope_authors.clone();
        let mut envelope = ledger
            .envelope
            .lock()
            .map_err(|_| "fake envelope lock was poisoned")?
            .clone();
        envelope.revision += 1;
        let envelope_body = serde_json::to_vec(&envelope)
            .map_err(|error| format!("failed to serialize service envelope: {error}"))?;
        let app = router(
            FakeRuntimeRepository {
                runtime: Arc::new(Mutex::new(runtime())),
            },
            ledger,
            BootstrapAuthenticator,
            FakeDecisionChannel::default(),
        );

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/service-envelopes/steward-run")
                    .header("authorization", "Bearer bootstrap-session")
                    .header("content-type", "application/json")
                    .body(Body::from(envelope_body))
                    .map_err(|error| format!("failed to build bootstrap request: {error}"))?,
            )
            .await
            .map_err(|error| format!("bootstrap request failed: {error}"))?;

        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(
            *authors
                .lock()
                .map_err(|_| "fake service-envelope author lock was poisoned")?,
            vec![("steward-run".to_owned(), "bootstrap@example.com".to_owned())],
            "the audit record must preserve the TokenReview username"
        );
        Ok(())
    }

    #[tokio::test]
    async fn rest_route_returns_the_parked_shared_counterexample() -> Result<(), String> {
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let ledger = ledger();
        let envelope_body = serde_json::to_vec(
            &*ledger
                .envelope
                .lock()
                .map_err(|_| "fake envelope lock was poisoned")?,
        )
        .map_err(|error| format!("failed to serialize envelope: {error}"))?;
        let app = router(
            runtimes,
            ledger,
            FakeAuthenticator,
            FakeDecisionChannel::default(),
        );
        let authored = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/envelopes/engineer")
                    .header("authorization", "Bearer admin-session")
                    .header("content-type", "application/json")
                    .body(Body::from(envelope_body))
                    .map_err(|error| format!("failed to build envelope request: {error}"))?,
            )
            .await
            .map_err(|error| format!("envelope authoring request failed: {error}"))?;
        assert_eq!(
            authored.status(),
            StatusCode::CREATED,
            "an authenticated admin must be able to author an immutable envelope revision"
        );
        let request = || {
            Request::builder()
                .method("PATCH")
                .uri("/v1/namespaces/team-a/runtimes/runtime-a/budget")
                .header("authorization", "Bearer user-session")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"amount":"60.00"}"#))
                .map_err(|error| format!("failed to build API request: {error}"))
        };

        let first = app
            .clone()
            .oneshot(request()?)
            .await
            .map_err(|error| format!("first API request failed: {error}"))?;
        assert_eq!(
            first.status(),
            StatusCode::OK,
            "first composed value should apply"
        );
        let second = app
            .clone()
            .oneshot(request()?)
            .await
            .map_err(|error| format!("second API request failed: {error}"))?;
        assert_eq!(
            second.status(),
            StatusCode::ACCEPTED,
            "second composed value should park"
        );
        let body = to_bytes(second.into_body(), 1024 * 1024)
            .await
            .map_err(|error| format!("failed to read parked response: {error}"))?;
        let response = serde_json::from_slice::<serde_json::Value>(&body)
            .map_err(|error| format!("parked response was not JSON: {error}"))?;
        assert_eq!(
            response
                .get("counterexample")
                .and_then(|value| value.as_str()),
            Some("envelope exceeded: budget.monthlyLimit requested 220.00 USD, ceiling 200.00 USD")
        );
        assert_eq!(
            response.pointer("/proposedSpec/budget/monthlyLimit"),
            Some(&serde_json::json!("220.00"))
        );
        assert_eq!(
            response.get("decisionKey"),
            Some(&serde_json::json!("PROJ-123"))
        );
        assert_eq!(
            response.get("evidenceUrl"),
            Some(&serde_json::json!(
                "https://jira.example.com/browse/PROJ-123"
            ))
        );
        let queue = app
            .oneshot(
                Request::builder()
                    .uri("/admin/approvals")
                    .header("authorization", "Bearer admin-session")
                    .body(Body::empty())
                    .map_err(|error| format!("failed to build queue request: {error}"))?,
            )
            .await
            .map_err(|error| format!("approval queue request failed: {error}"))?;
        assert_eq!(queue.status(), StatusCode::OK);
        let queue_body = to_bytes(queue.into_body(), 1024 * 1024)
            .await
            .map_err(|error| format!("failed to read approval queue: {error}"))?;
        let queue_html = String::from_utf8(queue_body.to_vec())
            .map_err(|error| format!("approval queue was not UTF-8: {error}"))?;
        for expected in [
            "runtime-uid-a",
            "engineer",
            "alice@example.com",
            "requested 220.00 USD",
            "ceiling 200.00 USD",
            "PROJ-123",
            "https://jira.example.com/browse/PROJ-123",
        ] {
            assert!(
                queue_html.contains(expected),
                "approval queue must render {expected:?} from the parked row"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn task_submission_outside_service_envelope_returns_a_structured_delta()
    -> Result<(), String> {
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let runtime_state = runtimes.runtime.clone();
        let ledger = ledger();
        let decisions = FakeDecisionChannel::default();
        let app = router(
            runtimes.clone(),
            ledger.clone(),
            FakeAuthenticator,
            decisions.clone(),
        )
        .merge(task_router(
            runtimes,
            ledger,
            decisions,
            FakeTaskIdentityResolver,
            StaticTaskWorkflowCatalog::new([TaskWorkflow {
                name: "wide-review".to_owned(),
                namespace: "team-a".to_owned(),
                coding_agent_runtime: "agent-v1".to_owned(),
                llms: vec![ModelRef {
                    provider: "provider-a".to_owned(),
                    model: "model-a".to_owned(),
                }],
                tools: Vec::new(),
                budget: Budget {
                    monthly_limit: "250.00".to_owned(),
                    currency: "USD".to_owned(),
                },
                ttl: Duration("24h".to_owned()),
                command: vec!["agent-v1".to_owned()],
            }]),
        ));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/tasks")
                    .header("authorization", "Bearer github-assertion")
                    .header("idempotency-key", "github-job-123")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"workflow":"wide-review","codingAgentRuntime":"agent-v1"}"#,
                    ))
                    .map_err(|error| format!("failed to build task submission: {error}"))?,
            )
            .await
            .map_err(|error| format!("task submission failed: {error}"))?;

        assert_eq!(
            response.status(),
            StatusCode::ACCEPTED,
            "an over-envelope workflow must be parked without provisioning"
        );
        let body = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .map_err(|error| format!("failed to read task rejection: {error}"))?;
        let response = serde_json::from_slice::<serde_json::Value>(&body)
            .map_err(|error| format!("task rejection was not JSON: {error}"))?;
        assert_eq!(
            response.pointer("/deltas/0/dimension"),
            Some(&serde_json::json!("budget")),
            "task anti-ratchet rejection must preserve the admission delta"
        );
        assert_eq!(
            response.pointer("/phase"),
            Some(&serde_json::json!("parked"))
        );
        let parked = runtime_state
            .lock()
            .map_err(|_| "fake runtime lock was poisoned")?;
        assert!(
            parked
                .annotations()
                .contains_key(PENDING_APPROVAL_ANNOTATION),
            "the parked task runtime must carry the controller-enforced pending marker"
        );
        assert!(parked.spec.llms.is_empty() && parked.spec.tools.is_empty());
        assert_eq!(parked.spec.budget.monthly_limit, "0");
        Ok(())
    }

    #[tokio::test]
    async fn task_input_archive_is_persisted_for_the_resolved_submitter() -> Result<(), String> {
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let runtime_state = runtimes.runtime.clone();
        let ledger = ledger();
        let task_rows = ledger.tasks.clone();
        let decisions = FakeDecisionChannel::default();
        let app = router(
            runtimes.clone(),
            ledger.clone(),
            FakeAuthenticator,
            decisions.clone(),
        )
        .merge(task_router(
            runtimes,
            ledger,
            decisions,
            FakeTaskIdentityResolver,
            StaticTaskWorkflowCatalog::new([task_workflow("100.00")]),
        ));
        let submitted = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/tasks")
                    .header("authorization", "Bearer github-assertion")
                    .header("idempotency-key", "github-job-inputs")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"workflow":"code-review","codingAgentRuntime":"agent-v1"}"#,
                    ))
                    .map_err(|error| format!("failed to build task submission: {error}"))?,
            )
            .await
            .map_err(|error| format!("task submission failed: {error}"))?;
        assert_eq!(submitted.status(), StatusCode::CREATED);
        {
            let runtime = runtime_state
                .lock()
                .map_err(|_| "fake runtime lock was poisoned")?;
            let authority =
                runtime.spec.canonical_authority.as_ref().ok_or_else(|| {
                    "delegated Task runtime lacked canonical authority".to_owned()
                })?;
            assert_eq!(
                authority.owner_user_id.as_str(),
                "usr_0123456789abcdef0123456789abcdef"
            );
            assert_eq!(
                authority.acting_user_id.as_ref(),
                Some(&authority.owner_user_id)
            );
        }
        let body = to_bytes(submitted.into_body(), 1024 * 1024)
            .await
            .map_err(|error| format!("failed to read task submission: {error}"))?;
        let task_uid = serde_json::from_slice::<serde_json::Value>(&body)
            .map_err(|error| format!("task submission was not JSON: {error}"))?
            .get("taskUid")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "task submission did not return taskUid".to_owned())?
            .to_owned();
        let cross_user = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/v1/tasks/{task_uid}"))
                    .header("authorization", "Bearer github-bob-assertion")
                    .body(Body::empty())
                    .map_err(|error| format!("failed to build cross-user task read: {error}"))?,
            )
            .await
            .map_err(|error| format!("cross-user task read failed: {error}"))?;
        assert_eq!(
            cross_user.status(),
            StatusCode::NOT_FOUND,
            "task lookup must bind to the full resolved submitting identity, not only its service"
        );
        let renamed_same_user = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/v1/tasks/{task_uid}"))
                    .header("authorization", "Bearer github-renamed-assertion")
                    .body(Body::empty())
                    .map_err(|error| format!("failed to build renamed-user task read: {error}"))?,
            )
            .await
            .map_err(|error| format!("renamed-user task read failed: {error}"))?;
        assert_eq!(
            renamed_same_user.status(),
            StatusCode::OK,
            "task ownership must survive a verified display-email rename for the same canonical user"
        );
        const CONTRACT_MAX_TASK_ARCHIVE_BYTES: usize = 64 * 1024 * 1024;
        let oversized = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/v1/tasks/{task_uid}/inputs"))
                    .header("authorization", "Bearer github-assertion")
                    .header("content-type", "application/x-tar")
                    .body(Body::from(vec![0; CONTRACT_MAX_TASK_ARCHIVE_BYTES + 1]))
                    .map_err(|error| format!("failed to build oversized Task input: {error}"))?,
            )
            .await
            .map_err(|error| format!("oversized Task input request failed: {error}"))?;
        assert_eq!(
            oversized.status(),
            StatusCode::PAYLOAD_TOO_LARGE,
            "Task inputs larger than the published 64 MiB limit must be rejected"
        );
        assert_eq!(
            oversized
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("application/json"),
            "the documented Task error response must remain machine-readable at the size boundary"
        );
        let oversized_body = to_bytes(oversized.into_body(), 1024 * 1024)
            .await
            .map_err(|error| format!("failed to read oversized Task error: {error}"))?;
        assert!(
            serde_json::from_slice::<serde_json::Value>(&oversized_body)
                .ok()
                .and_then(|body| body.get("error").cloned())
                .is_some(),
            "the 413 response must match TaskErrorResponse"
        );

        let archive = vec![0; 2 * 1024 * 1024 + 1];
        let staged = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/v1/tasks/{task_uid}/inputs"))
                    .header("authorization", "Bearer github-assertion")
                    .header("content-type", "application/x-tar")
                    .body(Body::from(archive.clone()))
                    .map_err(|error| format!("failed to build task input request: {error}"))?,
            )
            .await
            .map_err(|error| format!("task input request failed: {error}"))?;

        assert_eq!(
            staged.status(),
            StatusCode::NO_CONTENT,
            "the explicit Task limit must replace axum's smaller default while preserving opaque tar bytes"
        );
        {
            let rows = task_rows
                .lock()
                .map_err(|_| "fake task ledger lock was poisoned")?;
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].input_archive.as_deref(), Some(archive.as_slice()));
        }

        let execute = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/v1/tasks/{task_uid}/execute"))
                    .header("authorization", "Bearer github-assertion")
                    .body(Body::empty())
                    .map_err(|error| format!("failed to build task execute request: {error}"))?,
            )
            .await
            .map_err(|error| format!("task execute request failed: {error}"))?;
        assert_eq!(
            execute.status(),
            StatusCode::ACCEPTED,
            "execute must record desired work instead of running it in the HTTP handler"
        );
        let status = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/v1/tasks/{task_uid}"))
                    .header("authorization", "Bearer github-assertion")
                    .body(Body::empty())
                    .map_err(|error| format!("failed to build task status request: {error}"))?,
            )
            .await
            .map_err(|error| format!("task status request failed: {error}"))?;
        assert_eq!(status.status(), StatusCode::OK);
        let body = to_bytes(status.into_body(), 1024 * 1024)
            .await
            .map_err(|error| format!("failed to read task status: {error}"))?;
        let status = serde_json::from_slice::<serde_json::Value>(&body)
            .map_err(|error| format!("task status was not JSON: {error}"))?;
        assert_eq!(status.pointer("/phase"), Some(&serde_json::json!("queued")));

        let cancelled = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/v1/tasks/{task_uid}"))
                    .header("authorization", "Bearer github-assertion")
                    .body(Body::empty())
                    .map_err(|error| format!("failed to build task cancellation: {error}"))?,
            )
            .await
            .map_err(|error| format!("task cancellation failed: {error}"))?;
        assert_eq!(
            cancelled.status(),
            StatusCode::ACCEPTED,
            "cancellation must record desired cleanup and return before controller teardown"
        );
        {
            let rows = task_rows
                .lock()
                .map_err(|_| "fake task ledger lock was poisoned")?;
            assert_eq!(rows[0].phase, TaskPhase::Cancelled);
            assert!(rows[0].finalize_requested);
            assert!(!rows[0].finalized);
        }

        {
            let mut rows = task_rows
                .lock()
                .map_err(|_| "fake task ledger lock was poisoned")?;
            rows[0].phase = TaskPhase::Failed;
            rows[0].failure_reason = Some("task agent exited with code 23".to_owned());
        }
        let failed = app
            .oneshot(
                Request::builder()
                    .uri(format!("/v1/tasks/{task_uid}"))
                    .header("authorization", "Bearer github-assertion")
                    .body(Body::empty())
                    .map_err(|error| format!("failed to build failed task status: {error}"))?,
            )
            .await
            .map_err(|error| format!("failed task status request failed: {error}"))?;
        let body = to_bytes(failed.into_body(), 1024 * 1024)
            .await
            .map_err(|error| format!("failed to read failed task status: {error}"))?;
        let failed = serde_json::from_slice::<serde_json::Value>(&body)
            .map_err(|error| format!("failed task status was not JSON: {error}"))?;
        assert_eq!(
            failed.pointer("/failureReason"),
            Some(&serde_json::json!("task agent exited with code 23")),
            "a failed Task must expose its safe server-generated reason"
        );
        Ok(())
    }

    #[tokio::test]
    async fn canonical_owners_share_an_idempotency_key_without_sharing_a_runtime()
    -> Result<(), String> {
        let runtimes = MultiRuntimeRepository::default();
        let runtime_state = runtimes.runtimes.clone();
        let ledger = ledger();
        let task_rows = ledger.tasks.clone();
        let app = router(
            runtimes.clone(),
            ledger.clone(),
            FakeAuthenticator,
            FakeDecisionChannel::default(),
        )
        .merge(task_router(
            runtimes,
            ledger,
            FakeDecisionChannel::default(),
            FakeTaskIdentityResolver,
            StaticTaskWorkflowCatalog::new([task_workflow("100.00")]),
        ));
        let mut submissions = Vec::new();
        for bearer in ["github-assertion", "github-bob-assertion"] {
            let submit = || {
                Request::builder()
                    .method("POST")
                    .uri("/v1/tasks")
                    .header("authorization", format!("Bearer {bearer}"))
                    .header("idempotency-key", "shared-github-job")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"workflow":"code-review","codingAgentRuntime":"agent-v1"}"#,
                    ))
                    .map_err(|error| format!("failed to build owner-scoped submission: {error}"))
            };
            let response = app
                .clone()
                .oneshot(submit()?)
                .await
                .map_err(|error| format!("owner-scoped submission failed: {error}"))?;
            assert_eq!(
                response.status(),
                StatusCode::CREATED,
                "each canonical owner must receive an independent runtime"
            );
            let body = to_bytes(response.into_body(), 1024 * 1024)
                .await
                .map_err(|error| format!("failed to read owner-scoped submission: {error}"))?;
            let first = serde_json::from_slice::<serde_json::Value>(&body)
                .map_err(|error| format!("owner-scoped submission was not JSON: {error}"))?;
            let retried = app
                .clone()
                .oneshot(submit()?)
                .await
                .map_err(|error| format!("owner-scoped retry failed: {error}"))?;
            assert_eq!(retried.status(), StatusCode::CREATED);
            let retry_body = to_bytes(retried.into_body(), 1024 * 1024)
                .await
                .map_err(|error| format!("failed to read owner-scoped retry: {error}"))?;
            let retry = serde_json::from_slice::<serde_json::Value>(&retry_body)
                .map_err(|error| format!("owner-scoped retry was not JSON: {error}"))?;
            assert_eq!(
                retry.get("taskUid"),
                first.get("taskUid"),
                "an identical retry must converge within its canonical-owner scope"
            );
            assert_eq!(
                retry.get("runtimeUid"),
                first.get("runtimeUid"),
                "an identical retry must retain its own runtime"
            );
            submissions.push((bearer, first));
        }

        {
            let rows = task_rows
                .lock()
                .map_err(|_| "fake task ledger lock was poisoned".to_owned())?;
            assert_eq!(rows.len(), 2);
            assert_ne!(rows[0].task_uid, rows[1].task_uid);
            assert_ne!(rows[0].runtime_name, rows[1].runtime_name);
            for row in rows.iter() {
                assert!(row.runtime_name.starts_with("task-"));
                assert!(row.runtime_name.len() <= 63);
                assert!(
                    row.runtime_name.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
                    }),
                    "stable runtime names must remain DNS-label safe"
                );
            }
        }
        assert_eq!(
            runtime_state
                .lock()
                .map_err(|_| "multi-runtime repository lock was poisoned".to_owned())?
                .len(),
            2,
            "both owner-scoped reservations must bind distinct live runtimes"
        );

        for ((_, task), other_bearer) in submissions
            .iter()
            .zip(["github-bob-assertion", "github-assertion"])
        {
            let task_uid = task
                .get("taskUid")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "owner-scoped submission lacked taskUid".to_owned())?;
            let observed = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/v1/tasks/{task_uid}"))
                        .header("authorization", format!("Bearer {other_bearer}"))
                        .body(Body::empty())
                        .map_err(|error| format!("failed to build cross-owner read: {error}"))?,
                )
                .await
                .map_err(|error| format!("cross-owner read failed: {error}"))?;
            assert_eq!(observed.status(), StatusCode::NOT_FOUND);
            let deleted = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("DELETE")
                        .uri(format!("/v1/tasks/{task_uid}"))
                        .header("authorization", format!("Bearer {other_bearer}"))
                        .body(Body::empty())
                        .map_err(|error| format!("failed to build cross-owner delete: {error}"))?,
                )
                .await
                .map_err(|error| format!("cross-owner delete failed: {error}"))?;
            assert_eq!(deleted.status(), StatusCode::NOT_FOUND);
        }
        Ok(())
    }

    #[tokio::test]
    async fn pure_service_task_is_attributed_to_its_server_resolved_owner() -> Result<(), String> {
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let runtime_state = runtimes.runtime.clone();
        let ledger = ledger();
        let task_rows = ledger.tasks.clone();
        let decisions = FakeDecisionChannel::default();
        let mut workflow = task_workflow("100.00");
        workflow.name = "scheduled-review".to_owned();
        let app = router(
            runtimes.clone(),
            ledger.clone(),
            FakeAuthenticator,
            decisions.clone(),
        )
        .merge(task_router(
            runtimes,
            ledger,
            decisions,
            FakeTaskIdentityResolver,
            StaticTaskWorkflowCatalog::new([workflow]),
        ));
        let request = || {
            Request::builder()
                .method("POST")
                .uri("/v1/tasks")
                .header("authorization", "Bearer scheduled-assertion")
                .header("idempotency-key", "schedule-firing-123")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"workflow":"scheduled-review","codingAgentRuntime":"agent-v1"}"#,
                ))
                .map_err(|error| format!("failed to build scheduled task: {error}"))
        };
        let response = app
            .clone()
            .oneshot(request()?)
            .await
            .map_err(|error| format!("scheduled task submission failed: {error}"))?;
        assert_eq!(response.status(), StatusCode::CREATED);
        let first_body = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .map_err(|error| format!("failed to read scheduled task: {error}"))?;
        let first_uid = serde_json::from_slice::<serde_json::Value>(&first_body)
            .map_err(|error| format!("scheduled task was not JSON: {error}"))?
            .get("taskUid")
            .cloned();
        let retried = app
            .oneshot(request()?)
            .await
            .map_err(|error| format!("scheduled task retry failed: {error}"))?;
        assert_eq!(retried.status(), StatusCode::CREATED);
        let retry_body = to_bytes(retried.into_body(), 1024 * 1024)
            .await
            .map_err(|error| format!("failed to read scheduled retry: {error}"))?;
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&retry_body)
                .map_err(|error| format!("scheduled retry was not JSON: {error}"))?
                .get("taskUid")
                .cloned(),
            first_uid,
            "the same submitter idempotency key must return the same task"
        );
        let rows = task_rows
            .lock()
            .map_err(|_| "fake task ledger lock was poisoned")?;
        assert_eq!(rows[0].acting_user, None);
        assert_eq!(
            rows.len(),
            1,
            "idempotent retry must not create a second task"
        );
        assert_eq!(rows[0].owner, "owner@example.org");
        let runtime = runtime_state
            .lock()
            .map_err(|_| "fake runtime lock was poisoned")?;
        assert_eq!(runtime.spec.owner, Email("owner@example.org".to_owned()));
        let authority = runtime
            .spec
            .canonical_authority
            .as_ref()
            .ok_or_else(|| "pure-service Task runtime lacked owner authority".to_owned())?;
        assert_eq!(
            authority.owner_user_id.as_str(),
            "usr_456789abcdef0123456789abcdef0123"
        );
        assert_eq!(authority.acting_user_id, None);
        assert_eq!(
            runtime.spec.principal,
            Principal::Service {
                name: "scheduled-scanner".to_owned(),
                acting_user: None,
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn admin_can_author_a_service_envelope_in_a_distinct_scope() -> Result<(), String> {
        let ledger = ledger();
        let mut envelope = ledger
            .envelope
            .lock()
            .map_err(|_| "fake envelope lock was poisoned")?
            .clone();
        envelope.revision += 1;
        let envelope_body = serde_json::to_vec(&envelope)
            .map_err(|error| format!("failed to serialize service envelope: {error}"))?;
        let app = router(
            FakeRuntimeRepository {
                runtime: Arc::new(Mutex::new(runtime())),
            },
            ledger,
            FakeAuthenticator,
            FakeDecisionChannel::default(),
        );

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/service-envelopes/scheduled-scanner")
                    .header("authorization", "Bearer admin-session")
                    .header("content-type", "application/json")
                    .body(Body::from(envelope_body))
                    .map_err(|error| {
                        format!("failed to build service envelope request: {error}")
                    })?,
            )
            .await
            .map_err(|error| format!("service envelope authoring request failed: {error}"))?;

        assert_eq!(
            response.status(),
            StatusCode::CREATED,
            "a service envelope must have an administrator-authored authority path distinct from member roles"
        );
        Ok(())
    }

    #[tokio::test]
    async fn reposting_an_identical_service_envelope_is_idempotent() -> Result<(), String> {
        let ledger = ledger();
        let envelope_body = serde_json::to_vec(
            &*ledger
                .envelope
                .lock()
                .map_err(|_| "fake envelope lock was poisoned")?,
        )
        .map_err(|error| format!("failed to serialize service envelope: {error}"))?;
        let app = router(
            FakeRuntimeRepository {
                runtime: Arc::new(Mutex::new(runtime())),
            },
            ledger,
            FakeAuthenticator,
            FakeDecisionChannel::default(),
        );

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/service-envelopes/steward-run")
                    .header("authorization", "Bearer admin-session")
                    .header("content-type", "application/json")
                    .body(Body::from(envelope_body))
                    .map_err(|error| {
                        format!("failed to build service envelope request: {error}")
                    })?,
            )
            .await
            .map_err(|error| format!("service envelope retry failed: {error}"))?;

        assert_eq!(
            response.status(),
            StatusCode::OK,
            "an exact service-envelope retry must succeed without creating a new revision"
        );
        Ok(())
    }

    #[tokio::test]
    async fn envelope_authoring_rejects_malformed_authority_before_persistence()
    -> Result<(), String> {
        let app = router(
            FakeRuntimeRepository {
                runtime: Arc::new(Mutex::new(runtime())),
            },
            ledger(),
            FakeAuthenticator,
            FakeDecisionChannel::default(),
        );
        for body in [
            r#"{"revision":4,"spec":{"llms":[],"tools":[],"budget":{"monthlyLimit":"invalid","currency":"USD"},"ttl":"24h"}}"#,
            r#"{"revision":4,"spec":{"llms":[],"tools":[],"budget":{"monthlyLimit":"100.00","currency":"US dollars"},"ttl":"24h"}}"#,
            r#"{"revision":4,"spec":{"llms":[],"tools":[],"budget":{"monthlyLimit":"100.00","currency":"USD"},"ttl":"forever"}}"#,
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/admin/envelopes/engineer")
                        .header("authorization", "Bearer admin-session")
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .map_err(|error| {
                            format!("failed to build invalid envelope request: {error}")
                        })?,
                )
                .await
                .map_err(|error| format!("invalid envelope request failed: {error}"))?;
            assert_eq!(
                response.status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "malformed envelope authority must not be persisted"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn only_the_steward_admin_approval_route_applies_a_parked_manifest() -> Result<(), String>
    {
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let runtime_state = runtimes.runtime.clone();
        let ledger = ledger();
        let decisions = FakeDecisionChannel::default();
        let app = router(runtimes, ledger, FakeAuthenticator, decisions.clone());
        let parked = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/v1/namespaces/team-a/runtimes/runtime-a/budget")
                    .header("authorization", "Bearer user-session")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"amount":"120.00"}"#))
                    .map_err(|error| format!("failed to build parked request: {error}"))?,
            )
            .await
            .map_err(|error| format!("parked request failed: {error}"))?;
        assert_eq!(parked.status(), StatusCode::ACCEPTED);
        assert_eq!(
            runtime_state
                .lock()
                .map_err(|_| "fake runtime lock was poisoned")?
                .spec
                .budget
                .monthly_limit,
            "100.00",
            "filing or transitioning a Jira item must not mutate Steward desired state"
        );

        let approved = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/approvals/00000000-0000-0000-0000-000000000000/approve")
                    .header("authorization", "Bearer admin-session")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"rationale":"approved for this runtime","evidenceUrl":"https://jira.example.com/browse/PROJ-123","expiresAt":"2999-01-01T00:00:00Z"}"#,
                    ))
                    .map_err(|error| format!("failed to build approval request: {error}"))?,
            )
            .await
            .map_err(|error| format!("approval request failed: {error}"))?;
        assert_eq!(
            approved.status(),
            StatusCode::OK,
            "the authenticated Steward approval route must apply the parked manifest"
        );
        assert_eq!(
            runtime_state
                .lock()
                .map_err(|_| "fake runtime lock was poisoned")?
                .spec
                .budget
                .monthly_limit,
            "220.00"
        );
        let resolutions = decisions
            .resolutions
            .lock()
            .map_err(|_| "fake decision-resolution lock was poisoned")?;
        assert_eq!(resolutions.len(), 1);
        assert_eq!(resolutions[0].key, "PROJ-123");
        assert_eq!(resolutions[0].decided_by, "admin@example.com");
        Ok(())
    }

    #[tokio::test]
    async fn approval_never_overwrites_a_runtime_changed_after_parking() -> Result<(), String> {
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let runtime_state = runtimes.runtime.clone();
        let app = router(
            runtimes,
            ledger(),
            FakeAuthenticator,
            FakeDecisionChannel::default(),
        );
        let parked = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/v1/namespaces/team-a/runtimes/runtime-a/budget")
                    .header("authorization", "Bearer user-session")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"amount":"120.00"}"#))
                    .map_err(|error| format!("failed to build parked request: {error}"))?,
            )
            .await
            .map_err(|error| format!("parked request failed: {error}"))?;
        assert_eq!(parked.status(), StatusCode::ACCEPTED);
        {
            let mut runtime = runtime_state
                .lock()
                .map_err(|_| "fake runtime lock was poisoned")?;
            runtime.spec.llms[0].model = "model-b".to_owned();
            runtime.metadata.resource_version = Some("2".to_owned());
        }
        let approved = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/approvals/00000000-0000-0000-0000-000000000000/approve")
                    .header("authorization", "Bearer admin-session")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"rationale":"stale request","evidenceUrl":"https://jira.example.com/browse/PROJ-123","expiresAt":"2999-01-01T00:00:00Z"}"#,
                    ))
                    .map_err(|error| format!("failed to build stale approval request: {error}"))?,
            )
            .await
            .map_err(|error| format!("stale approval request failed: {error}"))?;
        assert_eq!(
            approved.status(),
            StatusCode::CONFLICT,
            "approval must fail before issuing a grant when the parked resourceVersion is stale"
        );
        assert_eq!(
            runtime_state
                .lock()
                .map_err(|_| "fake runtime lock was poisoned")?
                .spec
                .llms[0]
                .model,
            "model-b",
            "stale approval must preserve the intervening runtime change"
        );
        Ok(())
    }

    #[tokio::test]
    async fn admin_dashboard_shell_and_versioned_bootstrap_require_exact_admin_authority()
    -> Result<(), String> {
        let app = router(
            FakeRuntimeRepository {
                runtime: Arc::new(Mutex::new(runtime())),
            },
            ledger(),
            FakeAuthenticator,
            FakeDecisionChannel::default(),
        );

        for (authorization, expected) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some("Bearer user-session"), StatusCode::FORBIDDEN),
        ] {
            for uri in ["/admin", "/admin/api/v1/bootstrap"] {
                let mut request = Request::builder().uri(uri);
                if let Some(authorization) = authorization {
                    request = request.header("authorization", authorization);
                }
                let response = app
                    .clone()
                    .oneshot(request.body(Body::empty()).map_err(|error| {
                        format!("failed to build dashboard authorization request: {error}")
                    })?)
                    .await
                    .map_err(|error| format!("dashboard authorization request failed: {error}"))?;
                assert_eq!(
                    response.status(),
                    expected,
                    "{uri} must share the existing exact Steward administrator boundary"
                );
            }
        }

        let shell = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin")
                    .header("authorization", "Bearer admin-session")
                    .body(Body::empty())
                    .map_err(|error| format!("failed to build dashboard shell request: {error}"))?,
            )
            .await
            .map_err(|error| format!("dashboard shell request failed: {error}"))?;
        assert_eq!(shell.status(), StatusCode::OK);
        assert_eq!(
            shell
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("text/html; charset=utf-8")
        );
        let shell_body = to_bytes(shell.into_body(), 1024 * 1024)
            .await
            .map_err(|error| format!("failed to read dashboard shell: {error}"))?;
        let shell_html = String::from_utf8(shell_body.to_vec())
            .map_err(|error| format!("dashboard shell was not UTF-8: {error}"))?;
        for surface in ["Approvals", "Envelope templates", "Agent Runs"] {
            assert!(
                shell_html.contains(surface),
                "the dashboard shell must expose the {surface} navigation surface"
            );
        }
        for forbidden_fixture in ["leo@", "maya@", "openclaw-a1b2"] {
            assert!(
                !shell_html.contains(forbidden_fixture),
                "the production shell must not embed mock operational data: {forbidden_fixture}"
            );
        }

        let bootstrap = app
            .oneshot(
                Request::builder()
                    .uri("/admin/api/v1/bootstrap")
                    .header("authorization", "Bearer admin-session")
                    .body(Body::empty())
                    .map_err(|error| {
                        format!("failed to build dashboard bootstrap request: {error}")
                    })?,
            )
            .await
            .map_err(|error| format!("dashboard bootstrap request failed: {error}"))?;
        assert_eq!(bootstrap.status(), StatusCode::OK);
        let bootstrap_body = to_bytes(bootstrap.into_body(), 1024 * 1024)
            .await
            .map_err(|error| format!("failed to read dashboard bootstrap: {error}"))?;
        let bootstrap_json = serde_json::from_slice::<serde_json::Value>(&bootstrap_body)
            .map_err(|error| format!("dashboard bootstrap was not JSON: {error}"))?;
        assert_eq!(
            bootstrap_json,
            serde_json::json!({
                "apiVersion": "steward.admin/v1",
                "actor": "admin@example.com",
                "surfaces": ["approvals", "envelope", "fleet"]
            })
        );
        Ok(())
    }

    #[tokio::test]
    async fn admin_dashboard_responses_set_fail_closed_browser_headers() -> Result<(), String> {
        let app = router(
            FakeRuntimeRepository {
                runtime: Arc::new(Mutex::new(runtime())),
            },
            ledger(),
            FakeAuthenticator,
            FakeDecisionChannel::default(),
        );

        for uri in [
            "/admin",
            "/admin/assets/admin.css",
            "/admin/assets/admin.js",
            "/admin/assets/icon.svg",
            "/admin/api/v1/bootstrap",
            "/admin/approvals",
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(uri)
                        .header("authorization", "Bearer admin-session")
                        .body(Body::empty())
                        .map_err(|error| {
                            format!("failed to build dashboard header request: {error}")
                        })?,
                )
                .await
                .map_err(|error| format!("dashboard header request failed: {error}"))?;
            assert_eq!(response.status(), StatusCode::OK, "dashboard asset {uri}");
            assert_eq!(
                response
                    .headers()
                    .get("cache-control")
                    .and_then(|value| value.to_str().ok()),
                Some("no-store")
            );
            assert_eq!(
                response
                    .headers()
                    .get("x-content-type-options")
                    .and_then(|value| value.to_str().ok()),
                Some("nosniff")
            );
            assert_eq!(
                response
                    .headers()
                    .get("x-frame-options")
                    .and_then(|value| value.to_str().ok()),
                Some("DENY")
            );
            assert!(
                response.headers().contains_key("content-security-policy"),
                "dashboard asset {uri} must carry a CSP"
            );
        }

        let unauthenticated = app
            .oneshot(
                Request::builder()
                    .uri("/admin")
                    .body(Body::empty())
                    .map_err(|error| {
                        format!("failed to build unauthenticated dashboard request: {error}")
                    })?,
            )
            .await
            .map_err(|error| format!("unauthenticated dashboard request failed: {error}"))?;
        assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            unauthenticated
                .headers()
                .get("cache-control")
                .and_then(|value| value.to_str().ok()),
            Some("no-store"),
            "authentication failures on administrator routes must retain browser headers"
        );
        Ok(())
    }

    #[test]
    fn admin_dashboard_static_contract_is_accessible_responsive_and_storage_free() {
        let html = include_str!("../assets/admin/index.html");
        let css = include_str!("../assets/admin/admin.css");
        let javascript = include_str!("../assets/admin/admin.js");

        for required in [
            "lang=\"en\"",
            "name=\"viewport\"",
            "rel=\"icon\" href=\"/admin/assets/icon.svg\"",
            "class=\"skip-link\"",
            "aria-label=\"Steward administrator surfaces\"",
            "role=\"tablist\"",
            "role=\"tab\"",
            "role=\"tabpanel\"",
            "aria-controls=\"approvals-panel\"",
            "aria-selected=\"true\"",
            "role=\"alert\"",
        ] {
            assert!(
                html.contains(required),
                "dashboard shell is missing accessible contract {required:?}"
            );
        }
        assert!(
            css.contains("@media (max-width: 38rem)"),
            "dashboard shell must define its narrow viewport layout"
        );
        assert!(
            css.contains("prefers-reduced-motion"),
            "dashboard shell must honor reduced-motion preferences"
        );
        for key in ["ArrowLeft", "ArrowRight", "Home", "End"] {
            assert!(
                javascript.contains(key),
                "dashboard tabs must support the {key} keyboard command"
            );
        }
        for forbidden in [
            "localStorage",
            "sessionStorage",
            "document.cookie",
            "Authorization",
            "innerHTML",
            "outerHTML",
        ] {
            assert!(
                !javascript.contains(forbidden),
                "dashboard JavaScript must not use forbidden credential or HTML sink {forbidden}"
            );
        }
    }

    #[test]
    fn admin_dashboard_human_review_presents_left_aligned_navigation_links() {
        let html = include_str!("../assets/admin/index.html");
        let css = include_str!("../assets/admin/admin.css");
        let javascript = include_str!("../assets/admin/admin.js");

        for required in [
            "href=\"#approvals\"",
            "href=\"#envelope\"",
            "href=\"#fleet\"",
            "aria-current=\"page\"",
            "role=\"tab\"",
        ] {
            assert!(
                html.contains(required),
                "reviewed navigation link contract is missing {required:?}"
            );
        }
        for removed_presentation in [
            "<span>administration</span>",
            "class=\"contract\"",
            "<button id=\"approvals-tab\"",
        ] {
            assert!(
                !html.contains(removed_presentation),
                "human-requested presentation must remove {removed_presentation:?}"
            );
        }
        assert!(
            css.contains("justify-content: flex-start"),
            "reviewed navigation must remain left aligned"
        );
        for required in [
            "setAttribute(\"aria-current\", \"page\")",
            "removeAttribute(\"aria-current\")",
            "window.location.hash.slice(1) || \"approvals\"",
        ] {
            assert!(
                javascript.contains(required),
                "navigation script must preserve current-page semantics with {required:?}"
            );
        }
    }

    #[tokio::test]
    async fn agent_runs_list_is_versioned_empty_and_admin_only() -> Result<(), String> {
        let app = router(
            FakeRuntimeRepository {
                runtime: Arc::new(Mutex::new(runtime())),
            },
            ledger(),
            FakeAuthenticator,
            FakeDecisionChannel::default(),
        );

        for (authorization, expected) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some("Bearer user-session"), StatusCode::FORBIDDEN),
        ] {
            let mut request = Request::builder().uri("/admin/api/v1/runs");
            if let Some(authorization) = authorization {
                request = request.header("authorization", authorization);
            }
            let response = app
                .clone()
                .oneshot(request.body(Body::empty()).map_err(|error| {
                    format!("failed to build Agent Runs authorization request: {error}")
                })?)
                .await
                .map_err(|error| format!("Agent Runs authorization request failed: {error}"))?;
            assert_eq!(
                response.status(),
                expected,
                "ordinary identities must not read the administrator run ledger"
            );
        }

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/admin/api/v1/runs")
                    .header("authorization", "Bearer admin-session")
                    .body(Body::empty())
                    .map_err(|error| format!("failed to build Agent Runs request: {error}"))?,
            )
            .await
            .map_err(|error| format!("Agent Runs request failed: {error}"))?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .map_err(|error| format!("failed to read Agent Runs response: {error}"))?;
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body)
                .map_err(|error| format!("Agent Runs response was not JSON: {error}"))?,
            serde_json::json!({
                "apiVersion": "steward.admin/runs/v1",
                "runs": [],
                "nextCursor": null
            })
        );

        let bootstrap_only = router(
            FakeRuntimeRepository {
                runtime: Arc::new(Mutex::new(runtime())),
            },
            ledger(),
            BootstrapAuthenticator,
            FakeDecisionChannel::default(),
        );
        let response = bootstrap_only
            .oneshot(
                Request::builder()
                    .uri("/admin/api/v1/runs")
                    .header("authorization", "Bearer bootstrap-session")
                    .body(Body::empty())
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "route-scoped envelope bootstrap authority must not read Agent Runs"
        );
        Ok(())
    }

    fn sample_agent_run(task_uid: Uuid, created_at: &str) -> AgentRunRecord {
        AgentRunRecord {
            task_uid,
            submitter_service: "steward-run".to_owned(),
            acting_user: Some("alice@example.com".to_owned()),
            owner: "alice@example.com".to_owned(),
            owner_user_id: Some("usr_0123456789abcdef0123456789abcdef".to_owned()),
            workflow: "code-review".to_owned(),
            coding_agent_runtime: "agent-v1".to_owned(),
            runtime_uid: Some(format!("runtime-{task_uid}")),
            runtime_ownership: steward_types::RuntimeOwnership::Provisioned,
            phase: TaskPhase::Failed,
            runtime_spec: AgentRuntimeSpec {
                principal: Principal::Service {
                    name: "steward-run".to_owned(),
                    acting_user: Some(Email("alice@example.com".to_owned())),
                },
                owner: Email("alice@example.com".to_owned()),
                agent_type: AgentType {
                    name: "agent-v1".to_owned(),
                },
                llms: vec![ModelRef {
                    provider: "provider-a".to_owned(),
                    model: "model-a".to_owned(),
                }],
                tools: vec![steward_types::ToolGrant {
                    provider: "github".to_owned(),
                    resource: "issues".to_owned(),
                    action: "read".to_owned(),
                }],
                budget: Budget {
                    monthly_limit: "100.00".to_owned(),
                    currency: "USD".to_owned(),
                },
                ttl: Duration("24h".to_owned()),
                canonical_authority: None,
                bindings: None,
            },
            envelope_revision: Some(7),
            finalize_requested: true,
            finalized: true,
            failure_reason: Some("provider returned secret diagnostic payload".to_owned()),
            created_at: created_at.to_owned(),
            updated_at: "2026-08-12T12:01:00.000000Z".to_owned(),
            spend: Some(AgentRunSpend {
                observed_amount: "1.25".to_owned(),
                currency: "USD".to_owned(),
                exhausted: false,
                observed_at: "2026-08-12T12:00:30.000000Z".to_owned(),
            }),
            history_partial: false,
        }
    }

    #[tokio::test]
    async fn agent_run_detail_and_timeline_are_source_bounded_and_privacy_safe()
    -> Result<(), String> {
        let task_uid = Uuid::parse_str("11111111-1111-4111-8111-111111111111")
            .map_err(|error| error.to_string())?;
        let ledger = ledger();
        ledger
            .agent_runs
            .lock()
            .map_err(|_| "fake agent-run lock was poisoned")?
            .push(sample_agent_run(task_uid, "2026-08-12T12:00:00.000000Z"));
        ledger
            .agent_run_events
            .lock()
            .map_err(|_| "fake timeline lock was poisoned")?
            .push((
                task_uid,
                vec![
                    AgentRunTimelineEvent {
                        kind: AgentRunTimelineKind::Phase(TaskPhase::Submitted),
                        provenance: AgentRunTimelineProvenance::Backfilled,
                        at: "2026-08-12T12:00:00.000000Z".to_owned(),
                    },
                    AgentRunTimelineEvent {
                        kind: AgentRunTimelineKind::Phase(TaskPhase::Failed),
                        provenance: AgentRunTimelineProvenance::Recorded,
                        at: "2026-08-12T12:01:00.000000Z".to_owned(),
                    },
                ],
            ));
        let app = router(
            FakeRuntimeRepository {
                runtime: Arc::new(Mutex::new(runtime())),
            },
            ledger,
            FakeAuthenticator,
            FakeDecisionChannel::default(),
        );

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/admin/api/v1/runs/{task_uid}"))
                    .header("authorization", "Bearer admin-session")
                    .body(Body::empty())
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .map_err(|error| error.to_string())?;
        let body_text = String::from_utf8(body.to_vec()).map_err(|error| error.to_string())?;
        assert!(
            !body_text.contains("secret diagnostic payload"),
            "raw failure text must never cross the administrator read-model boundary"
        );
        let body: serde_json::Value =
            serde_json::from_str(&body_text).map_err(|error| error.to_string())?;
        assert_eq!(body["apiVersion"], AGENT_RUNS_API_VERSION);
        assert_eq!(body["run"]["errorCategory"], "execution-failed");
        assert_eq!(body["run"]["observedSpend"]["observedAmount"], "1.25");
        assert_eq!(body["run"]["toolActivity"]["availability"], "unavailable");
        assert_eq!(body["run"]["toolActivity"]["source"], "none");
        assert_eq!(body["run"]["toolActivity"]["reason"], "notPersisted");
        assert_eq!(
            body["run"]["inferenceActivity"]["availability"],
            "unavailable"
        );
        assert_eq!(body["run"]["githubRun"]["reason"], "notRecorded");

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/admin/api/v1/runs/{task_uid}/timeline"))
                    .header("authorization", "Bearer admin-session")
                    .body(Body::empty())
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .map_err(|error| error.to_string())?;
        let body: serde_json::Value =
            serde_json::from_slice(&body).map_err(|error| error.to_string())?;
        assert_eq!(body["history"]["availability"], "partial");
        assert_eq!(body["events"][0]["provenance"], "backfilled");
        assert_eq!(body["events"][1]["phase"], "failed");
        Ok(())
    }

    #[tokio::test]
    async fn agent_runs_pagination_filters_and_query_validation_fail_closed() -> Result<(), String>
    {
        let newer_uid = Uuid::parse_str("22222222-2222-4222-8222-222222222222")
            .map_err(|error| error.to_string())?;
        let older_uid = Uuid::parse_str("11111111-1111-4111-8111-111111111111")
            .map_err(|error| error.to_string())?;
        let ledger = ledger();
        {
            let mut runs = ledger
                .agent_runs
                .lock()
                .map_err(|_| "fake agent-run lock was poisoned")?;
            runs.push(sample_agent_run(older_uid, "2026-08-12T11:00:00.000000Z"));
            let mut newer = sample_agent_run(newer_uid, "2026-08-12T12:00:00.000000Z");
            newer.workflow = "incident-response".to_owned();
            newer.phase = TaskPhase::Running;
            runs.push(newer);
        }
        let app = router(
            FakeRuntimeRepository {
                runtime: Arc::new(Mutex::new(runtime())),
            },
            ledger,
            FakeAuthenticator,
            FakeDecisionChannel::default(),
        );
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/api/v1/runs?limit=1")
                    .header("authorization", "Bearer admin-session")
                    .body(Body::empty())
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .map_err(|error| error.to_string())?;
        let body: serde_json::Value =
            serde_json::from_slice(&body).map_err(|error| error.to_string())?;
        assert_eq!(body["runs"][0]["taskUid"], newer_uid.to_string());
        assert_eq!(body["nextCursor"], newer_uid.to_string());

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/admin/api/v1/runs?cursor={newer_uid}&limit=1"))
                    .header("authorization", "Bearer admin-session")
                    .body(Body::empty())
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
        let body = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .map_err(|error| error.to_string())?;
        let body: serde_json::Value =
            serde_json::from_slice(&body).map_err(|error| error.to_string())?;
        assert_eq!(body["runs"][0]["taskUid"], older_uid.to_string());
        assert!(body["nextCursor"].is_null());

        for uri in [
            "/admin/api/v1/runs?limit=0".to_owned(),
            "/admin/api/v1/runs?unexpected=true".to_owned(),
            "/admin/api/v1/runs?cursor=33333333-3333-4333-8333-333333333333".to_owned(),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(uri)
                        .header("authorization", "Bearer admin-session")
                        .body(Body::empty())
                        .map_err(|error| error.to_string())?,
                )
                .await
                .map_err(|error| error.to_string())?;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
        Ok(())
    }
}
