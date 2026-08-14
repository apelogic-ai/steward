use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
#[cfg(test)]
use k8s_openapi::api::authentication::v1::TokenReviewStatus;
use k8s_openapi::api::authentication::v1::{TokenReview, UserInfo};
use kube::api::{Api, PostParams};
use kube::{Client, ResourceExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use steward_admission::{AdmissionDecision, AdmissionDelta, Envelope, evaluate_with_grants};
use steward_ports::MAX_TASK_INPUT_ARCHIVE_BYTES;
use steward_store::{ParkRejection, PgStore, StoreError, TaskRecord, TaskReservationRequest};
use steward_types::{
    AgentRuntime, AgentRuntimeSpec, Budget, CanonicalAuthorityBinding, CanonicalUserId, Duration,
    Email, ModelRef, PENDING_APPROVAL_ANNOTATION, Principal, RuntimeOwnership, TaskPhase,
    ToolGrant,
};
use uuid::Uuid;

use crate::{
    AdmissionLedger, ApiError, BoxFuture, DecisionChannel, KubernetesTokenReviewAudience,
    RuntimeCreateError, RuntimeRepository, authenticated_token_review_user,
    file_decision_reference, spec_digest, token_review_request,
};

const SERVICE_PRINCIPAL_ANNOTATION: &str = "agents.apelogic.ai/service-principal";
const SERVICE_GROUP_PREFIX: &str = "agents.apelogic.ai/service-principal:";
const ACTING_USER_GROUP_PREFIX: &str = "agents.apelogic.ai/acting-user:";
const TASK_OWNER_GROUP_PREFIX: &str = "agents.apelogic.ai/task-owner:";
const CANONICAL_USER_GROUP_PREFIX: &str = "agents.apelogic.ai/canonical-user:";

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
        let catalog = Self::new(workflows);
        if catalog.workflows.is_empty() {
            return Err("task workflow catalog must not be empty".to_owned());
        }
        Ok(catalog)
    }
}

impl TaskWorkflowCatalog for StaticTaskWorkflowCatalog {
    fn workflow(&self, name: &str) -> Option<TaskWorkflow> {
        self.workflows.get(name).cloned()
    }
}

pub trait TaskSubmissionLedger: Clone + Send + Sync + 'static {
    fn reserve_task<'a>(
        &'a self,
        request: TaskReservationRequest<'a>,
    ) -> BoxFuture<'a, Result<steward_store::TaskReservation, StoreError>>;

    fn bind_task_runtime<'a>(
        &'a self,
        task_uid: Uuid,
        runtime_uid: &'a str,
        phase: TaskPhase,
    ) -> BoxFuture<'a, Result<TaskRecord, StoreError>>;

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
    fn reserve_task<'a>(
        &'a self,
        request: TaskReservationRequest<'a>,
    ) -> BoxFuture<'a, Result<steward_store::TaskReservation, StoreError>> {
        Box::pin(async move { PgStore::reserve_task(self, &request).await })
    }

    fn bind_task_runtime<'a>(
        &'a self,
        task_uid: Uuid,
        runtime_uid: &'a str,
        phase: TaskPhase,
    ) -> BoxFuture<'a, Result<TaskRecord, StoreError>> {
        Box::pin(
            async move { PgStore::bind_task_runtime(self, task_uid, runtime_uid, phase).await },
        )
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
    pub coding_agent_runtime: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_runtime_uid: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TaskStatusResponse {
    #[schema(value_type = String, format = "uuid")]
    pub task_uid: Uuid,
    pub runtime_uid: String,
    pub phase: TaskPhase,
    pub runtime_ownership: RuntimeOwnership,
    pub finalized: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schema(value_type = Vec<TaskAdmissionDelta>)]
    pub deltas: Vec<AdmissionDelta>,
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
struct TaskApiState<R, L, D, I, W> {
    runtimes: R,
    ledger: L,
    decisions: D,
    identities: I,
    workflows: W,
}

pub fn task_router<R, L, D, I, W>(
    runtimes: R,
    ledger: L,
    decisions: D,
    identities: I,
    workflows: W,
) -> Router
where
    R: RuntimeRepository,
    L: AdmissionLedger + TaskSubmissionLedger,
    D: DecisionChannel + Clone,
    I: TaskIdentityResolver,
    W: TaskWorkflowCatalog,
{
    Router::new()
        .route("/v1/tasks", post(submit_task::<R, L, D, I, W>))
        .route(
            "/v1/tasks/{task_uid}/inputs",
            put(put_task_inputs::<R, L, D, I, W>),
        )
        .route(
            "/v1/tasks/{task_uid}/execute",
            post(execute_task::<R, L, D, I, W>),
        )
        .route(
            "/v1/tasks/{task_uid}/outputs",
            get(get_task_outputs::<R, L, D, I, W>),
        )
        .route(
            "/v1/tasks/{task_uid}",
            get(get_task::<R, L, D, I, W>).delete(delete_task::<R, L, D, I, W>),
        )
        .layer(DefaultBodyLimit::max(MAX_TASK_INPUT_ARCHIVE_BYTES))
        .with_state(TaskApiState {
            runtimes,
            ledger,
            decisions,
            identities,
            workflows,
        })
}

async fn get_task_outputs<R, L, D, I, W>(
    State(state): State<TaskApiState<R, L, D, I, W>>,
    Path(task_uid): Path<Uuid>,
    headers: HeaderMap,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger + TaskSubmissionLedger,
    D: DecisionChannel + Clone,
    I: TaskIdentityResolver,
    W: TaskWorkflowCatalog,
{
    let identity = match resolve_task_identity(&state.identities, &headers).await {
        Ok(identity) => identity,
        Err(error) => return error.into_response(),
    };
    let record = match state
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

async fn delete_task<R, L, D, I, W>(
    State(state): State<TaskApiState<R, L, D, I, W>>,
    Path(task_uid): Path<Uuid>,
    headers: HeaderMap,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger + TaskSubmissionLedger,
    D: DecisionChannel + Clone,
    I: TaskIdentityResolver,
    W: TaskWorkflowCatalog,
{
    let identity = match resolve_task_identity(&state.identities, &headers).await {
        Ok(identity) => identity,
        Err(error) => return error.into_response(),
    };
    match state
        .ledger
        .request_task_finalization(
            task_uid,
            &identity.service,
            identity.canonical_user_id.as_str(),
        )
        .await
    {
        Ok(record) => match status_response(record, Vec::new()) {
            Ok(response) => (StatusCode::ACCEPTED, Json(response)).into_response(),
            Err(error) => error.into_response(),
        },
        Err(error) => ApiError::Store(error).into_response(),
    }
}

async fn execute_task<R, L, D, I, W>(
    State(state): State<TaskApiState<R, L, D, I, W>>,
    Path(task_uid): Path<Uuid>,
    headers: HeaderMap,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger + TaskSubmissionLedger,
    D: DecisionChannel + Clone,
    I: TaskIdentityResolver,
    W: TaskWorkflowCatalog,
{
    let identity = match resolve_task_identity(&state.identities, &headers).await {
        Ok(identity) => identity,
        Err(error) => return error.into_response(),
    };
    match state
        .ledger
        .request_task_execution(
            task_uid,
            &identity.service,
            identity.canonical_user_id.as_str(),
        )
        .await
    {
        Ok(record) => match status_response(record, Vec::new()) {
            Ok(response) => (StatusCode::ACCEPTED, Json(response)).into_response(),
            Err(error) => error.into_response(),
        },
        Err(error) => ApiError::Store(error).into_response(),
    }
}

async fn get_task<R, L, D, I, W>(
    State(state): State<TaskApiState<R, L, D, I, W>>,
    Path(task_uid): Path<Uuid>,
    headers: HeaderMap,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger + TaskSubmissionLedger,
    D: DecisionChannel + Clone,
    I: TaskIdentityResolver,
    W: TaskWorkflowCatalog,
{
    let identity = match resolve_task_identity(&state.identities, &headers).await {
        Ok(identity) => identity,
        Err(error) => return error.into_response(),
    };
    let record = match state
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
    match status_response(record, Vec::new()) {
        Ok(response) => (StatusCode::OK, Json(response)).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn put_task_inputs<R, L, D, I, W>(
    State(state): State<TaskApiState<R, L, D, I, W>>,
    Path(task_uid): Path<Uuid>,
    headers: HeaderMap,
    archive: Result<Bytes, BytesRejection>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger + TaskSubmissionLedger,
    D: DecisionChannel + Clone,
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

async fn submit_task<R, L, D, I, W>(
    State(state): State<TaskApiState<R, L, D, I, W>>,
    headers: HeaderMap,
    Json(request): Json<TaskSubmissionRequest>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger + TaskSubmissionLedger,
    D: DecisionChannel + Clone,
    I: TaskIdentityResolver,
    W: TaskWorkflowCatalog,
{
    match submit_task_inner(&state, &headers, &request).await {
        Ok((status, response)) => (status, Json(response)).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn submit_task_inner<R, L, D, I, W>(
    state: &TaskApiState<R, L, D, I, W>,
    headers: &HeaderMap,
    request: &TaskSubmissionRequest,
) -> Result<(StatusCode, TaskStatusResponse), ApiError>
where
    R: RuntimeRepository,
    L: AdmissionLedger + TaskSubmissionLedger,
    D: DecisionChannel + Clone,
    I: TaskIdentityResolver,
    W: TaskWorkflowCatalog,
{
    let idempotency_key = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::Admission("Idempotency-Key is required".to_owned()))?;
    let identity = resolve_task_identity(&state.identities, headers).await?;
    let workflow = state
        .workflows
        .workflow(&request.workflow)
        .ok_or(ApiError::TaskWorkflowNotFound)?;
    if request.coding_agent_runtime != workflow.coding_agent_runtime {
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
        bindings: None,
    };
    let envelope = state
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
        let runtime = state
            .runtimes
            .get_by_uid(runtime_uid)
            .await
            .map_err(ApiError::Runtime)?;
        let runtime_namespace = runtime
            .namespace()
            .ok_or_else(|| ApiError::Runtime("adopted runtime has no namespace".to_owned()))?;
        if runtime_namespace != workflow.namespace
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
        let reservation = state
            .ledger
            .reserve_task(TaskReservationRequest {
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
                coding_agent_runtime: &workflow.coding_agent_runtime,
                runtime_namespace: &runtime_namespace,
                runtime_name: &runtime_name,
                runtime_ownership: RuntimeOwnership::Adopted,
                runtime_spec: &spec,
                agent_command: &workflow.command,
            })
            .await
            .map_err(ApiError::Store)?;
        let record = if reservation.record.runtime_uid.is_none() {
            state
                .ledger
                .bind_task_runtime(
                    reservation.record.task_uid,
                    runtime_uid,
                    TaskPhase::Submitted,
                )
                .await
                .map_err(ApiError::Store)?
        } else {
            reservation.record
        };
        return task_response(record, Vec::new());
    }
    let runtime_name = stable_task_runtime_name(
        &identity.service,
        identity.canonical_user_id.as_str(),
        idempotency_key,
    );
    let reservation = state
        .ledger
        .reserve_task(TaskReservationRequest {
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
            coding_agent_runtime: &workflow.coding_agent_runtime,
            runtime_namespace: &workflow.namespace,
            runtime_name: &runtime_name,
            runtime_ownership: RuntimeOwnership::Provisioned,
            runtime_spec: &spec,
            agent_command: &workflow.command,
        })
        .await
        .map_err(ApiError::Store)?;
    if !reservation.inserted && reservation.record.runtime_uid.is_some() {
        return task_response(reservation.record, Vec::new());
    }

    let (runtime, phase, deltas) = create_task_runtime(
        &state.runtimes,
        &state.ledger,
        &state.decisions,
        TaskRuntimePlan {
            namespace: &workflow.namespace,
            name: &runtime_name,
            service: &identity.service,
            proposed_spec: &spec,
            envelope: &envelope,
            decision,
        },
    )
    .await?;
    let runtime_uid = runtime
        .metadata
        .uid
        .as_deref()
        .ok_or(ApiError::MissingRuntimeUid)?;
    let record = state
        .ledger
        .bind_task_runtime(reservation.record.task_uid, runtime_uid, phase)
        .await
        .map_err(ApiError::Store)?;
    task_response(record, deltas)
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

struct TaskRuntimePlan<'a> {
    namespace: &'a str,
    name: &'a str,
    service: &'a str,
    proposed_spec: &'a AgentRuntimeSpec,
    envelope: &'a Envelope,
    decision: AdmissionDecision,
}

async fn create_task_runtime<R, L, D>(
    runtimes: &R,
    ledger: &L,
    decisions: &D,
    plan: TaskRuntimePlan<'_>,
) -> Result<(AgentRuntime, TaskPhase, Vec<AdmissionDelta>), ApiError>
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel,
{
    let mut runtime = AgentRuntime::new(plan.name, plan.proposed_spec.clone());
    runtime.metadata.namespace = Some(plan.namespace.to_owned());
    runtime.metadata.annotations = Some(BTreeMap::from([(
        SERVICE_PRINCIPAL_ANNOTATION.to_owned(),
        plan.service.to_owned(),
    )]));
    let AdmissionDecision::Reject { deltas } = plan.decision else {
        let created = create_or_get_matching_runtime(runtimes, plan.namespace, &runtime).await?;
        return Ok((created, TaskPhase::Submitted, Vec::new()));
    };
    let proposed_digest = spec_digest(plan.proposed_spec)?;
    runtime.spec.llms.clear();
    runtime.spec.tools.clear();
    runtime.spec.budget.monthly_limit = "0".to_owned();
    runtime.spec.budget.currency = plan.envelope.spec.budget.currency.clone();
    runtime.spec.ttl = plan.envelope.spec.ttl.clone();
    runtime.metadata.annotations.get_or_insert_default().insert(
        PENDING_APPROVAL_ANNOTATION.to_owned(),
        proposed_digest.clone(),
    );
    let created = create_or_get_matching_runtime(runtimes, plan.namespace, &runtime).await?;
    let runtime_uid = created
        .metadata
        .uid
        .as_deref()
        .ok_or(ApiError::MissingRuntimeUid)?;
    let base_digest = spec_digest(&created.spec)?;
    let parked = ledger
        .park_rejection(ParkRejection {
            runtime_uid,
            runtime_namespace: plan.namespace,
            runtime_name: plan.name,
            spec_digest: &proposed_digest,
            base_spec_digest: &base_digest,
            base_pending_approval_digest: Some(&proposed_digest),
            base_spec: &created.spec,
            envelope_revision: plan.envelope.revision,
            deltas: &deltas,
            proposed_spec: plan.proposed_spec,
            actor: plan.service,
            member_role: plan.service,
        })
        .await
        .map_err(ApiError::Store)?;
    if parked.decision_key.is_none() || parked.evidence_url.is_none() {
        file_decision_reference(ledger, decisions, parked.approval_id).await?;
    }
    Ok((created, TaskPhase::Parked, deltas))
}

async fn create_or_get_matching_runtime<R: RuntimeRepository>(
    runtimes: &R,
    namespace: &str,
    runtime: &AgentRuntime,
) -> Result<AgentRuntime, ApiError> {
    match runtimes.create_as_authority(namespace, runtime).await {
        Ok(created) => Ok(created),
        Err(RuntimeCreateError::Kubernetes { status: 409, .. }) => {
            let existing = runtimes
                .get(
                    namespace,
                    &runtime.metadata.name.clone().unwrap_or_default(),
                )
                .await
                .map_err(ApiError::Runtime)?;
            if existing.spec != runtime.spec || existing.annotations() != runtime.annotations() {
                return Err(ApiError::Conflict(
                    "task runtime name is bound to unrelated desired state".to_owned(),
                ));
            }
            Ok(existing)
        }
        Err(error) => Err(ApiError::RuntimeCreate(error)),
    }
}

fn task_response(
    record: TaskRecord,
    deltas: Vec<AdmissionDelta>,
) -> Result<(StatusCode, TaskStatusResponse), ApiError> {
    let status = if record.phase == TaskPhase::Parked {
        StatusCode::ACCEPTED
    } else if record.finalized {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((status, status_response(record, deltas)?))
}

fn status_response(
    record: TaskRecord,
    deltas: Vec<AdmissionDelta>,
) -> Result<TaskStatusResponse, ApiError> {
    let runtime_uid = record.runtime_uid.ok_or(ApiError::TaskNotReady)?;
    Ok(TaskStatusResponse {
        task_uid: record.task_uid,
        runtime_uid,
        phase: record.phase,
        runtime_ownership: record.runtime_ownership,
        finalized: record.finalized,
        failure_reason: record.failure_reason,
        deltas,
    })
}

fn stable_task_runtime_name(
    service: &str,
    canonical_owner_user_id: &str,
    idempotency_key: &str,
) -> String {
    let digest = Sha256::digest(
        format!("{service}\0{canonical_owner_user_id}\0{idempotency_key}").as_bytes(),
    );
    let suffix = digest[..10]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("task-{suffix}")
}
