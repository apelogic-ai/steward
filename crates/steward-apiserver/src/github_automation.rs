//! Owner-scoped GitHub repository automation backed only by governed Connections operations.

use std::collections::BTreeMap;
use std::hash::Hash;

use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use steward_store::AgentRunRecord;
use steward_types::direct_package::{ExecutionLogMode, TaskOrigin};
use steward_types::{CanonicalUserId, TaskPhase};
use uuid::Uuid;

use crate::browser_auth::{BrowserAuthService, BrowserSessionBinding, protect_browser_routes};
use crate::connections::{
    ConnectionBrokerError, ConnectionMutationProof, ConnectionSession, adapt_browser_context,
};
use crate::github_actions::{
    DirectPackageGithubActionsWorkflowContext, GithubActionsEnvelopeSelection, StewardRunRelease,
    StewardRunWorkflowInstallationMode, render_direct_package_github_actions_workflow,
    steward_run_supports_package_path_invocation,
};
use crate::governed_connections::{
    AutomationOperationIdentity, ConnectionOperationKind, GovernedConnectionsBroker,
    ProviderConnectionStatusSource, SplitConnectionsBroker,
};
use crate::tasks::TaskApiConfig;
use crate::{AgentRunLedger, BoxFuture};

pub const GITHUB_AUTOMATION_API_VERSION: &str = "steward.github-automation/v1";
const DEFAULT_PAGE_SIZE: u32 = 30;
const MAX_PAGE_SIZE: u32 = 100;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GithubAutomationIdentity {
    pub(crate) idempotency_identity: String,
    pub(crate) idempotency_scope: Option<String>,
    pub(crate) publication_subject: Option<String>,
    pub(crate) allow_result_cache: bool,
}

pub trait GithubAutomationBroker<B>: Clone + Send + Sync + 'static
where
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    fn execute<'a>(
        &'a self,
        session: &'a ConnectionSession<B>,
        operation: ConnectionOperationKind,
        request: Value,
        identity: &'a GithubAutomationIdentity,
    ) -> BoxFuture<'a, Result<Value, ConnectionBrokerError>>;

    fn evidence<'a>(
        &'a self,
        canonical_user_id: &'a CanonicalUserId,
        operation: ConnectionOperationKind,
        idempotency_identity_prefix: &'a str,
    ) -> BoxFuture<'a, Result<Option<Value>, ConnectionBrokerError>>;
}

impl<B> GithubAutomationBroker<B> for GovernedConnectionsBroker<B>
where
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    fn execute<'a>(
        &'a self,
        session: &'a ConnectionSession<B>,
        operation: ConnectionOperationKind,
        request: Value,
        identity: &'a GithubAutomationIdentity,
    ) -> BoxFuture<'a, Result<Value, ConnectionBrokerError>> {
        Box::pin(async move {
            self.run_automation_operation(
                session,
                operation,
                request,
                AutomationOperationIdentity {
                    idempotency_identity: &identity.idempotency_identity,
                    idempotency_scope: identity.idempotency_scope.as_deref(),
                    publication_subject: identity.publication_subject.as_deref(),
                    allow_result_cache: identity.allow_result_cache,
                },
            )
            .await
        })
    }

    fn evidence<'a>(
        &'a self,
        canonical_user_id: &'a CanonicalUserId,
        operation: ConnectionOperationKind,
        idempotency_identity_prefix: &'a str,
    ) -> BoxFuture<'a, Result<Option<Value>, ConnectionBrokerError>> {
        Box::pin(async move {
            self.stored_automation_result(canonical_user_id, operation, idempotency_identity_prefix)
                .await
        })
    }
}

impl<B, M, S> GithubAutomationBroker<B> for SplitConnectionsBroker<M, S>
where
    B: Clone + Eq + Hash + Send + Sync + 'static,
    M: GithubAutomationBroker<B>,
    S: ProviderConnectionStatusSource<B>,
{
    fn execute<'a>(
        &'a self,
        session: &'a ConnectionSession<B>,
        operation: ConnectionOperationKind,
        request: Value,
        identity: &'a GithubAutomationIdentity,
    ) -> BoxFuture<'a, Result<Value, ConnectionBrokerError>> {
        self.governed_mutations()
            .execute(session, operation, request, identity)
    }

    fn evidence<'a>(
        &'a self,
        canonical_user_id: &'a CanonicalUserId,
        operation: ConnectionOperationKind,
        idempotency_identity_prefix: &'a str,
    ) -> BoxFuture<'a, Result<Option<Value>, ConnectionBrokerError>> {
        self.governed_mutations().evidence(
            canonical_user_id,
            operation,
            idempotency_identity_prefix,
        )
    }
}

#[derive(Clone)]
pub struct GithubAutomationConfig {
    task_api: TaskApiConfig,
    steward_run_release: StewardRunRelease,
    workflow_installation_mode: StewardRunWorkflowInstallationMode,
    task_identity_discovery_enabled: bool,
}

impl GithubAutomationConfig {
    pub fn new(
        task_api: TaskApiConfig,
        steward_run_release: StewardRunRelease,
        workflow_installation_mode: StewardRunWorkflowInstallationMode,
        task_identity_discovery_enabled: bool,
    ) -> Self {
        Self {
            task_api,
            steward_run_release,
            workflow_installation_mode,
            task_identity_discovery_enabled,
        }
    }
}

#[derive(Clone)]
pub(crate) struct GithubAutomationState<L, P> {
    ledger: L,
    broker: P,
    config: GithubAutomationConfig,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RepositoryQuery {
    /// Empty lists repositories owned by the authenticated GitHub user.
    #[serde(default)]
    query: String,
    #[serde(default = "first_page")]
    page: u32,
    #[serde(default = "default_page_size")]
    per_page: u32,
    /// Bypass a completed cached listing while still joining an identical in-flight request.
    #[serde(default)]
    refresh: bool,
}

const fn first_page() -> u32 {
    1
}

const fn default_page_size() -> u32 {
    DEFAULT_PAGE_SIZE
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GithubRepositoryView {
    owner: String,
    owner_id: String,
    name: String,
    repository_id: String,
    default_branch: String,
    private: bool,
    url: String,
    ready: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    missing_prerequisite: Option<&'static str>,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GithubRepositoriesResponse {
    api_version: &'static str,
    login: String,
    repositories: Vec<GithubRepositoryView>,
    page: u32,
    has_next_page: bool,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RepositoryTargetRequest {
    owner: String,
    repository: String,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PublishTaskRequest {
    owner: String,
    repository: String,
    idempotency_key: String,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DispatchTaskRequest {
    owner: String,
    repository: String,
    #[serde(default)]
    inputs: BTreeMap<String, String>,
    idempotency_key: String,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RunStatusQuery {
    owner: String,
    repository: String,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorkflowDetectionResponse {
    api_version: &'static str,
    path: String,
    exists: bool,
    compatible: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    sha: Option<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PublishTaskResponse {
    api_version: &'static str,
    pull_request_url: String,
    pull_request_number: u64,
    branch: String,
    package_digest: String,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DispatchTaskResponse {
    api_version: &'static str,
    run_id: u64,
    url: String,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GithubTaskBundleResponse {
    api_version: &'static str,
    files: BTreeMap<String, String>,
    workflow_path: String,
    package_digest: String,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GithubAutomationEvidenceResponse {
    api_version: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    publication: Option<PublishTaskResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dispatch: Option<DispatchTaskResponse>,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GithubOnboardingEvidenceResponse {
    api_version: &'static str,
    publication_observed: bool,
    workflow_observed: bool,
    dispatch_observed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GithubJobView {
    id: u64,
    name: String,
    status: String,
    conclusion: Option<String>,
    url: String,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GithubRunStatusResponse {
    api_version: &'static str,
    run_id: u64,
    run_attempt: u32,
    phase: String,
    conclusion: Option<String>,
    url: String,
    jobs: Vec<GithubJobView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_log: Option<String>,
    #[schema(value_type = Option<String>, format = "uuid")]
    #[serde(skip_serializing_if = "Option::is_none")]
    linked_task_uid: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    linked_task_phase: Option<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GithubAutomationErrorResponse {
    api_version: &'static str,
    error: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    manual_files: Option<BTreeMap<String, String>>,
}

struct ExactRepositoryBundle {
    files: BTreeMap<String, String>,
    workflow_path: String,
    workflow_content: String,
    package_digest: String,
}

pub fn protected_router<L, P>(
    ledger: L,
    broker: P,
    config: GithubAutomationConfig,
    browser_auth: BrowserAuthService,
) -> Router
where
    L: AgentRunLedger,
    P: GithubAutomationBroker<BrowserSessionBinding>,
{
    let routes = Router::new()
        .route(
            "/app/api/v1/github/repositories",
            get(list_repositories::<L, P>),
        )
        .route(
            "/app/api/v1/runs/{task_uid}/github/workflow",
            post(detect_workflow::<L, P>),
        )
        .route(
            "/app/api/v1/runs/{task_uid}/github/bundle",
            get(github_task_bundle::<L, P>),
        )
        .route(
            "/app/api/v1/runs/{task_uid}/github/evidence",
            get(github_automation_evidence::<L, P>),
        )
        .route(
            "/app/api/v1/runs/{task_uid}/github/onboarding",
            get(github_onboarding_evidence::<L, P>),
        )
        .route(
            "/app/api/v1/runs/{task_uid}/github/publish",
            post(publish_task::<L, P>),
        )
        .route(
            "/app/api/v1/runs/{task_uid}/github/dispatch",
            post(dispatch_task::<L, P>),
        )
        .route(
            "/app/api/v1/runs/{task_uid}/github/runs/{run_id}",
            get(github_run_status::<L, P>),
        )
        .with_state(GithubAutomationState {
            ledger,
            broker,
            config,
        })
        .route_layer(middleware::from_fn(adapt_browser_context));
    protect_browser_routes(routes, browser_auth)
}

#[utoipa::path(
    get,
    path = "/app/api/v1/runs/{task_uid}/github/bundle",
    params(("task_uid" = String, Path, format = "uuid")),
    responses(
        (status = 200, body = GithubTaskBundleResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 404, description = "Run is unavailable"),
        (status = 409, description = "Run is not publishable"),
        (status = 503, body = GithubAutomationErrorResponse)
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn github_task_bundle<L, P>(
    session: Option<Extension<ConnectionSession<BrowserSessionBinding>>>,
    State(state): State<GithubAutomationState<L, P>>,
    Path(task_uid): Path<Uuid>,
) -> Response
where
    L: AgentRunLedger,
    P: GithubAutomationBroker<BrowserSessionBinding>,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    match exact_bundle(&state, &session, task_uid).await {
        Ok(bundle) => no_store_json(GithubTaskBundleResponse {
            api_version: GITHUB_AUTOMATION_API_VERSION,
            files: bundle.files,
            workflow_path: bundle.workflow_path,
            package_digest: bundle.package_digest,
        }),
        Err(response) => response,
    }
}

#[utoipa::path(
    get,
    path = "/app/api/v1/runs/{task_uid}/github/evidence",
    params(("task_uid" = String, Path, format = "uuid"), RunStatusQuery),
    responses(
        (status = 200, body = GithubAutomationEvidenceResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Repository is not admitted"),
        (status = 404, description = "Run or repository is unavailable"),
        (status = 409, description = "Run evidence is unavailable or invalid"),
        (status = 503, body = GithubAutomationErrorResponse)
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn github_automation_evidence<L, P>(
    session: Option<Extension<ConnectionSession<BrowserSessionBinding>>>,
    State(state): State<GithubAutomationState<L, P>>,
    Path(task_uid): Path<Uuid>,
    Query(query): Query<RunStatusQuery>,
) -> Response
where
    L: AgentRunLedger,
    P: GithubAutomationBroker<BrowserSessionBinding>,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    match resolve_repository(&state, &session, &query.owner, &query.repository).await {
        Ok(repository) if repository.ready => {}
        Ok(_) => return StatusCode::FORBIDDEN.into_response(),
        Err(response) => return response,
    }
    let run = match owned_successful_browser_run(&state.ledger, &session, task_uid).await {
        Ok(run) => run,
        Err(response) => return response,
    };
    let package_digest = match run.browser_task_evidence {
        Some(evidence) if evidence.validate().is_ok() => evidence.closure_digest,
        _ => return StatusCode::CONFLICT.into_response(),
    };
    let subject = automation_subject(task_uid, &query.owner, &query.repository);
    let publication = match state
        .broker
        .evidence(
            &session.subject.canonical_user_id,
            ConnectionOperationKind::Publish,
            &operation_subject("publish", &subject),
        )
        .await
    {
        Ok(Some(value)) => match publish_response(value, package_digest.as_str().to_owned()) {
            Ok(response) => Some(response),
            Err(()) => return unavailable(),
        },
        Ok(None) => None,
        Err(error) => return automation_error(error),
    };
    let dispatch = match state
        .broker
        .evidence(
            &session.subject.canonical_user_id,
            ConnectionOperationKind::Dispatch,
            &operation_subject("dispatch", &subject),
        )
        .await
    {
        Ok(Some(value)) => match dispatch_response(value) {
            Ok(response) => Some(response),
            Err(()) => return unavailable(),
        },
        Ok(None) => None,
        Err(error) => return automation_error(error),
    };
    no_store_json(GithubAutomationEvidenceResponse {
        api_version: GITHUB_AUTOMATION_API_VERSION,
        publication,
        dispatch,
    })
}

#[utoipa::path(
    get,
    path = "/app/api/v1/runs/{task_uid}/github/onboarding",
    params(("task_uid" = String, Path, format = "uuid")),
    responses(
        (status = 200, body = GithubOnboardingEvidenceResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 404, description = "Run is unavailable"),
        (status = 409, description = "Run evidence is unavailable or invalid"),
        (status = 503, body = GithubAutomationErrorResponse)
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn github_onboarding_evidence<L, P>(
    session: Option<Extension<ConnectionSession<BrowserSessionBinding>>>,
    State(state): State<GithubAutomationState<L, P>>,
    Path(task_uid): Path<Uuid>,
) -> Response
where
    L: AgentRunLedger,
    P: GithubAutomationBroker<BrowserSessionBinding>,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let run = match owned_successful_browser_run(&state.ledger, &session, task_uid).await {
        Ok(run) => run,
        Err(response) => return response,
    };
    let package_digest = match run.browser_task_evidence {
        Some(evidence) if evidence.validate().is_ok() => evidence.closure_digest,
        _ => return StatusCode::CONFLICT.into_response(),
    };
    let publication = match state
        .broker
        .evidence(
            &session.subject.canonical_user_id,
            ConnectionOperationKind::Publish,
            &task_operation_prefix("publish", task_uid),
        )
        .await
    {
        Ok(Some(value)) => match publish_response(value, package_digest.as_str().to_owned()) {
            Ok(_) => true,
            Err(()) => return unavailable(),
        },
        Ok(None) => false,
        Err(error) => return automation_error(error),
    };
    let workflow = match state
        .broker
        .evidence(
            &session.subject.canonical_user_id,
            ConnectionOperationKind::Workflow,
            &task_operation_prefix("workflow", task_uid),
        )
        .await
    {
        Ok(Some(value)) => match workflow_response(value) {
            Ok(response) => response.compatible,
            Err(()) => return unavailable(),
        },
        Ok(None) => false,
        Err(error) => return automation_error(error),
    };
    let dispatch = match state
        .broker
        .evidence(
            &session.subject.canonical_user_id,
            ConnectionOperationKind::Dispatch,
            &task_operation_prefix("dispatch", task_uid),
        )
        .await
    {
        Ok(Some(value)) => match dispatch_response(value) {
            Ok(_) => true,
            Err(()) => return unavailable(),
        },
        Ok(None) => false,
        Err(error) => return automation_error(error),
    };
    no_store_json(GithubOnboardingEvidenceResponse {
        api_version: GITHUB_AUTOMATION_API_VERSION,
        publication_observed: publication,
        workflow_observed: workflow,
        dispatch_observed: dispatch,
    })
}

#[utoipa::path(
    get,
    path = "/app/api/v1/github/repositories",
    params(RepositoryQuery),
    responses(
        (status = 200, body = GithubRepositoriesResponse),
        (status = 400, description = "Repository query is invalid"),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 503, body = GithubAutomationErrorResponse)
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn list_repositories<L, P>(
    session: Option<Extension<ConnectionSession<BrowserSessionBinding>>>,
    State(state): State<GithubAutomationState<L, P>>,
    Query(query): Query<RepositoryQuery>,
) -> Response
where
    L: AgentRunLedger,
    P: GithubAutomationBroker<BrowserSessionBinding>,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if query.page == 0
        || query.per_page == 0
        || query.per_page > MAX_PAGE_SIZE
        || query.query.len() > 200
        || query.query.chars().any(char::is_control)
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let request = json!({
        "query": query.query,
        "page": query.page,
        "perPage": query.per_page,
    });
    let identity = cached_read_operation_identity("repositories", &request, !query.refresh);
    let result = match state
        .broker
        .execute(
            &session,
            ConnectionOperationKind::Repositories,
            request,
            &identity,
        )
        .await
    {
        Ok(result) => result,
        Err(error) => return automation_error(error),
    };
    match repository_response(&state.config.task_api, result) {
        Ok(response) => no_store_json(response),
        Err(()) => unavailable(),
    }
}

#[utoipa::path(
    post,
    path = "/app/api/v1/runs/{task_uid}/github/workflow",
    params(("task_uid" = String, Path, format = "uuid"), ("X-Steward-CSRF" = String, Header)),
    request_body = RepositoryTargetRequest,
    responses(
        (status = 200, body = WorkflowDetectionResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Mutation proof is invalid"),
        (status = 404, description = "Run or repository is unavailable"),
        (status = 409, description = "Run is not publishable"),
        (status = 503, body = GithubAutomationErrorResponse)
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn detect_workflow<L, P>(
    session: Option<Extension<ConnectionSession<BrowserSessionBinding>>>,
    proof: Option<Extension<ConnectionMutationProof>>,
    State(state): State<GithubAutomationState<L, P>>,
    Path(task_uid): Path<Uuid>,
    Json(request): Json<RepositoryTargetRequest>,
) -> Response
where
    L: AgentRunLedger,
    P: GithubAutomationBroker<BrowserSessionBinding>,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if proof.is_none() {
        return StatusCode::FORBIDDEN.into_response();
    }
    let repository =
        match resolve_repository(&state, &session, &request.owner, &request.repository).await {
            Ok(repository) => repository,
            Err(response) => return response,
        };
    let bundle = match exact_bundle(&state, &session, task_uid).await {
        Ok(bundle) => bundle,
        Err(response) => return response,
    };
    let subject = automation_subject(task_uid, &repository.owner, &repository.name);
    let identity = scoped_read_operation_identity("workflow", &subject);
    let result = match state
        .broker
        .execute(
            &session,
            ConnectionOperationKind::Workflow,
            json!({
                "owner": repository.owner,
                "repo": repository.name,
                "path": bundle.workflow_path,
                "ref": repository.default_branch,
                "expectedContent": bundle.workflow_content,
            }),
            &identity,
        )
        .await
    {
        Ok(result) => result,
        Err(error) => return automation_error(error),
    };
    match workflow_response(result) {
        Ok(response) => no_store_json(response),
        Err(()) => unavailable(),
    }
}

#[utoipa::path(
    post,
    path = "/app/api/v1/runs/{task_uid}/github/publish",
    params(("task_uid" = String, Path, format = "uuid"), ("X-Steward-CSRF" = String, Header)),
    request_body = PublishTaskRequest,
    responses(
        (status = 200, body = PublishTaskResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Mutation proof is invalid or repository is not admitted"),
        (status = 404, description = "Run or repository is unavailable"),
        (status = 409, description = "Run is not publishable"),
        (status = 422, description = "Publication request is invalid"),
        (status = 503, body = GithubAutomationErrorResponse)
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn publish_task<L, P>(
    session: Option<Extension<ConnectionSession<BrowserSessionBinding>>>,
    proof: Option<Extension<ConnectionMutationProof>>,
    State(state): State<GithubAutomationState<L, P>>,
    Path(task_uid): Path<Uuid>,
    Json(request): Json<PublishTaskRequest>,
) -> Response
where
    L: AgentRunLedger,
    P: GithubAutomationBroker<BrowserSessionBinding>,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if proof.is_none() {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !valid_idempotency_key(&request.idempotency_key) {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    }
    let repository =
        match resolve_repository(&state, &session, &request.owner, &request.repository).await {
            Ok(repository) if repository.ready => repository,
            Ok(_) => return StatusCode::FORBIDDEN.into_response(),
            Err(response) => return response,
        };
    let bundle = match exact_bundle(&state, &session, task_uid).await {
        Ok(bundle) => bundle,
        Err(response) => return response,
    };
    let branch = format!("steward/task-{}", task_uid.simple());
    let files = bundle
        .files
        .iter()
        .map(|(path, content)| json!({"path": path, "content": content}))
        .collect::<Vec<_>>();
    let operation_request = json!({
        "owner": repository.owner,
        "repo": repository.name,
        "baseBranch": repository.default_branch,
        "branch": branch,
        "title": "chore: add Steward governed task",
        "body": format!("Publishes the exact governed package tested by Steward Task `{task_uid}`."),
        "files": files,
    });
    let publication_subject = automation_subject(task_uid, &repository.owner, &repository.name);
    let identity = write_operation_identity(
        "publish",
        &request.idempotency_key,
        &operation_request,
        Some(&publication_subject),
    );
    let result = match state
        .broker
        .execute(
            &session,
            ConnectionOperationKind::Publish,
            operation_request,
            &identity,
        )
        .await
    {
        Ok(result) => result,
        Err(error) => return automation_error(error),
    };
    match publish_response(result, bundle.package_digest) {
        Ok(response) => no_store_json(response),
        Err(()) => unavailable(),
    }
}

#[utoipa::path(
    post,
    path = "/app/api/v1/runs/{task_uid}/github/dispatch",
    params(("task_uid" = String, Path, format = "uuid"), ("X-Steward-CSRF" = String, Header)),
    request_body = DispatchTaskRequest,
    responses(
        (status = 200, body = DispatchTaskResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Mutation proof is invalid or repository is not admitted"),
        (status = 404, description = "Run, repository, or workflow is unavailable"),
        (status = 409, description = "Published workflow does not match the tested task"),
        (status = 422, description = "Dispatch inputs are invalid"),
        (status = 503, body = GithubAutomationErrorResponse)
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn dispatch_task<L, P>(
    session: Option<Extension<ConnectionSession<BrowserSessionBinding>>>,
    proof: Option<Extension<ConnectionMutationProof>>,
    State(state): State<GithubAutomationState<L, P>>,
    Path(task_uid): Path<Uuid>,
    Json(request): Json<DispatchTaskRequest>,
) -> Response
where
    L: AgentRunLedger,
    P: GithubAutomationBroker<BrowserSessionBinding>,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if proof.is_none() {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !valid_idempotency_key(&request.idempotency_key)
        || request.inputs.len() > 20
        || request.inputs.iter().any(|(name, value)| {
            !valid_input_name(name) || value.len() > 8 * 1024 || value.chars().any(char::is_control)
        })
    {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    }
    let repository =
        match resolve_repository(&state, &session, &request.owner, &request.repository).await {
            Ok(repository) if repository.ready => repository,
            Ok(_) => return StatusCode::FORBIDDEN.into_response(),
            Err(response) => return response,
        };
    let bundle = match exact_bundle(&state, &session, task_uid).await {
        Ok(bundle) => bundle,
        Err(response) => return response,
    };
    let operation_request = json!({
        "owner": repository.owner,
        "repo": repository.name,
        "workflowId": bundle.workflow_path,
        "ref": repository.default_branch,
        "inputs": request.inputs,
        "expectedContent": bundle.workflow_content,
    });
    let identity = write_operation_identity(
        "dispatch",
        &request.idempotency_key,
        &operation_request,
        Some(&automation_subject(
            task_uid,
            &repository.owner,
            &repository.name,
        )),
    );
    let result = match state
        .broker
        .execute(
            &session,
            ConnectionOperationKind::Dispatch,
            operation_request,
            &identity,
        )
        .await
    {
        Ok(result) => result,
        Err(error) => return automation_error(error),
    };
    match dispatch_response(result) {
        Ok(response) => no_store_json(response),
        Err(()) => unavailable(),
    }
}

#[utoipa::path(
    get,
    path = "/app/api/v1/runs/{task_uid}/github/runs/{run_id}",
    params(("task_uid" = String, Path, format = "uuid"), ("run_id" = u64, Path), RunStatusQuery),
    responses(
        (status = 200, body = GithubRunStatusResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 404, description = "Run or repository is unavailable"),
        (status = 503, body = GithubAutomationErrorResponse)
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn github_run_status<L, P>(
    session: Option<Extension<ConnectionSession<BrowserSessionBinding>>>,
    State(state): State<GithubAutomationState<L, P>>,
    Path((task_uid, run_id)): Path<(Uuid, u64)>,
    Query(query): Query<RunStatusQuery>,
) -> Response
where
    L: AgentRunLedger,
    P: GithubAutomationBroker<BrowserSessionBinding>,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if owned_successful_browser_run(&state.ledger, &session, task_uid)
        .await
        .is_err()
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let repository =
        match resolve_repository(&state, &session, &query.owner, &query.repository).await {
            Ok(repository) => repository,
            Err(response) => return response,
        };
    let identity = fresh_operation_identity("run-status");
    let result = match state
        .broker
        .execute(
            &session,
            ConnectionOperationKind::RunStatus,
            json!({"owner": repository.owner, "repo": repository.name, "runId": run_id}),
            &identity,
        )
        .await
    {
        Ok(result) => result,
        Err(error) => return automation_error(error),
    };
    let run_attempt = result
        .get("runAttempt")
        .and_then(Value::as_u64)
        .and_then(|attempt| u32::try_from(attempt).ok())
        .filter(|attempt| *attempt > 0);
    let repository_name = format!("{}/{}", repository.owner, repository.name);
    let linked = state
        .ledger
        .github_rerun_task(
            session.subject.canonical_user_id.as_str(),
            &repository_name,
            &run_id.to_string(),
            run_attempt.unwrap_or(1).saturating_sub(1),
        )
        .await
        .ok()
        .flatten();
    match run_status_response(result, linked.map(|task| (task.task_uid, task.phase))) {
        Ok(response) => no_store_json(response),
        Err(()) => unavailable(),
    }
}

async fn resolve_repository<L, P>(
    state: &GithubAutomationState<L, P>,
    session: &ConnectionSession<BrowserSessionBinding>,
    owner: &str,
    repository: &str,
) -> Result<GithubRepositoryView, Response>
where
    L: AgentRunLedger,
    P: GithubAutomationBroker<BrowserSessionBinding>,
{
    if !valid_repository_component(owner, 39) || !valid_repository_component(repository, 100) {
        return Err(StatusCode::NOT_FOUND.into_response());
    }
    let query = format!("repo:{owner}/{repository}");
    let identity = fresh_operation_identity("repository");
    let result = state
        .broker
        .execute(
            session,
            ConnectionOperationKind::Repositories,
            json!({"query": query, "page": 1, "perPage": 10}),
            &identity,
        )
        .await
        .map_err(automation_error)?;
    let response =
        repository_response(&state.config.task_api, result).map_err(|()| unavailable())?;
    response
        .repositories
        .into_iter()
        .find(|candidate| candidate.owner == owner && candidate.name == repository)
        .ok_or_else(|| StatusCode::NOT_FOUND.into_response())
}

async fn exact_bundle<L, P>(
    state: &GithubAutomationState<L, P>,
    session: &ConnectionSession<BrowserSessionBinding>,
    task_uid: Uuid,
) -> Result<ExactRepositoryBundle, Response>
where
    L: AgentRunLedger,
    P: GithubAutomationBroker<BrowserSessionBinding>,
{
    let run = owned_successful_browser_run(&state.ledger, session, task_uid).await?;
    let evidence = run
        .browser_task_evidence
        .ok_or_else(|| StatusCode::CONFLICT.into_response())?;
    if evidence.source != "inline" || evidence.validate().is_err() {
        return Err(StatusCode::CONFLICT.into_response());
    }
    let mut files = evidence
        .inline_files
        .ok_or_else(|| StatusCode::CONFLICT.into_response())?;
    if files.len() != 1 || !files.contains_key(evidence.path.as_str()) {
        return Err(StatusCode::CONFLICT.into_response());
    }
    if !steward_run_supports_package_path_invocation(&state.config.steward_run_release) {
        return Err(automation_problem(
            "steward_run_release_unsupported",
            Some(files),
        ));
    }
    let envelope = GithubActionsEnvelopeSelection {
        id: run
            .user_envelope_instance_id
            .ok_or_else(|| StatusCode::CONFLICT.into_response())?,
        revision: u64::try_from(
            run.user_envelope_revision
                .ok_or_else(|| StatusCode::CONFLICT.into_response())?,
        )
        .map_err(|_| StatusCode::CONFLICT.into_response())?,
        digest: run
            .user_envelope_digest
            .ok_or_else(|| StatusCode::CONFLICT.into_response())?,
    };
    let generated =
        render_direct_package_github_actions_workflow(&DirectPackageGithubActionsWorkflowContext {
            envelope,
            invocation_path: None,
            package_path: Some(evidence.path.as_str().to_owned()),
            execution_log: ExecutionLogMode::Full,
            reviewed_release: state.config.steward_run_release.clone(),
            workflow_installation_mode: state.config.workflow_installation_mode,
            task_identity_discovery_enabled: state.config.task_identity_discovery_enabled,
        })
        .map_err(|_| StatusCode::CONFLICT.into_response())?;
    if files
        .insert(generated.suggested_path.clone(), generated.yaml.clone())
        .is_some()
        || files.len() != 2
    {
        return Err(StatusCode::CONFLICT.into_response());
    }
    Ok(ExactRepositoryBundle {
        files,
        workflow_path: generated.suggested_path,
        workflow_content: generated.yaml,
        package_digest: evidence.closure_digest.as_str().to_owned(),
    })
}

async fn owned_successful_browser_run<L>(
    ledger: &L,
    session: &ConnectionSession<BrowserSessionBinding>,
    task_uid: Uuid,
) -> Result<AgentRunRecord, Response>
where
    L: AgentRunLedger,
{
    match ledger.agent_run(task_uid).await {
        Ok(Some(run))
            if run.owner_user_id.as_deref() == Some(session.subject.canonical_user_id.as_str())
                && run.phase == TaskPhase::Succeeded
                && run.finalized
                && run.task_origin == TaskOrigin::Browser =>
        {
            Ok(run)
        }
        Ok(Some(_) | None) => Err(StatusCode::NOT_FOUND.into_response()),
        Err(_) => Err(unavailable()),
    }
}

fn repository_response(
    config: &TaskApiConfig,
    value: Value,
) -> Result<GithubRepositoriesResponse, ()> {
    let object = value.as_object().ok_or(())?;
    let login = object.get("login").and_then(Value::as_str).ok_or(())?;
    let page = object
        .get("page")
        .and_then(Value::as_u64)
        .and_then(|page| u32::try_from(page).ok())
        .ok_or(())?;
    let has_next_page = object
        .get("hasNextPage")
        .and_then(Value::as_bool)
        .ok_or(())?;
    let repositories = object
        .get("repositories")
        .and_then(Value::as_array)
        .ok_or(())?
        .iter()
        .map(|repository| repository_view(config, repository))
        .collect::<Result<Vec<_>, ()>>()?;
    Ok(GithubRepositoriesResponse {
        api_version: GITHUB_AUTOMATION_API_VERSION,
        login: login.to_owned(),
        repositories,
        page,
        has_next_page,
    })
}

fn repository_view(config: &TaskApiConfig, value: &Value) -> Result<GithubRepositoryView, ()> {
    let object = value.as_object().ok_or(())?;
    let owner = string_field(object, "owner")?;
    let owner_id = string_field(object, "ownerId")?;
    let name = string_field(object, "name")?;
    let repository_id = string_field(object, "repositoryId")?;
    let ready = config.browser_source_repository_ids_are_authorized(&owner_id, &repository_id);
    Ok(GithubRepositoryView {
        owner,
        owner_id,
        name,
        repository_id,
        default_branch: string_field(object, "defaultBranch")?,
        private: object.get("private").and_then(Value::as_bool).ok_or(())?,
        url: string_field(object, "url")?,
        ready,
        missing_prerequisite: (!ready).then_some("source_repository_not_admitted"),
    })
}

fn workflow_response(value: Value) -> Result<WorkflowDetectionResponse, ()> {
    let object = value.as_object().ok_or(())?;
    Ok(WorkflowDetectionResponse {
        api_version: GITHUB_AUTOMATION_API_VERSION,
        path: string_field(object, "path")?,
        exists: object.get("exists").and_then(Value::as_bool).ok_or(())?,
        compatible: object
            .get("compatible")
            .and_then(Value::as_bool)
            .ok_or(())?,
        sha: object.get("sha").and_then(Value::as_str).map(str::to_owned),
    })
}

fn publish_response(value: Value, package_digest: String) -> Result<PublishTaskResponse, ()> {
    let object = value.as_object().ok_or(())?;
    Ok(PublishTaskResponse {
        api_version: GITHUB_AUTOMATION_API_VERSION,
        pull_request_url: string_field(object, "pullRequestUrl")?,
        pull_request_number: object
            .get("pullRequestNumber")
            .and_then(Value::as_u64)
            .ok_or(())?,
        branch: string_field(object, "branch")?,
        package_digest,
    })
}

fn dispatch_response(value: Value) -> Result<DispatchTaskResponse, ()> {
    let object = value.as_object().ok_or(())?;
    Ok(DispatchTaskResponse {
        api_version: GITHUB_AUTOMATION_API_VERSION,
        run_id: object.get("runId").and_then(Value::as_u64).ok_or(())?,
        url: string_field(object, "url")?,
    })
}

fn run_status_response(
    value: Value,
    linked: Option<(Uuid, TaskPhase)>,
) -> Result<GithubRunStatusResponse, ()> {
    let object = value.as_object().ok_or(())?;
    let jobs = object
        .get("jobs")
        .and_then(Value::as_array)
        .ok_or(())?
        .iter()
        .map(|job| {
            let job = job.as_object().ok_or(())?;
            Ok(GithubJobView {
                id: job.get("id").and_then(Value::as_u64).ok_or(())?,
                name: string_field(job, "name")?,
                status: string_field(job, "status")?,
                conclusion: job
                    .get("conclusion")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                url: string_field(job, "url")?,
            })
        })
        .collect::<Result<Vec<_>, ()>>()?;
    Ok(GithubRunStatusResponse {
        api_version: GITHUB_AUTOMATION_API_VERSION,
        run_id: object.get("runId").and_then(Value::as_u64).ok_or(())?,
        run_attempt: object
            .get("runAttempt")
            .and_then(Value::as_u64)
            .and_then(|attempt| u32::try_from(attempt).ok())
            .filter(|attempt| *attempt > 0)
            .ok_or(())?,
        phase: string_field(object, "phase")?,
        conclusion: object
            .get("conclusion")
            .and_then(Value::as_str)
            .map(str::to_owned),
        url: string_field(object, "url")?,
        jobs,
        failure_log: object
            .get("failureLog")
            .and_then(Value::as_str)
            .map(str::to_owned),
        linked_task_uid: linked.as_ref().map(|(task_uid, _)| *task_uid),
        linked_task_phase: linked.map(|(_, phase)| task_phase_name(phase).to_owned()),
    })
}

const fn task_phase_name(phase: TaskPhase) -> &'static str {
    match phase {
        TaskPhase::Submitted => "submitted",
        TaskPhase::Parked => "parked",
        TaskPhase::Queued => "queued",
        TaskPhase::Running => "running",
        TaskPhase::Succeeded => "succeeded",
        TaskPhase::Failed => "failed",
        TaskPhase::Cancelled => "cancelled",
    }
}

fn string_field(object: &serde_json::Map<String, Value>, field: &str) -> Result<String, ()> {
    object
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or(())
}

fn valid_repository_component(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && !value.starts_with(['.', '-'])
        && !value.ends_with('.')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn valid_input_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

fn valid_idempotency_key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
}

fn fresh_operation_identity(operation: &str) -> GithubAutomationIdentity {
    GithubAutomationIdentity {
        idempotency_identity: format!("github-{operation}:{}", Uuid::new_v4()),
        idempotency_scope: None,
        publication_subject: None,
        allow_result_cache: false,
    }
}

fn cached_read_operation_identity(
    operation: &str,
    payload: &Value,
    allow_result_cache: bool,
) -> GithubAutomationIdentity {
    GithubAutomationIdentity {
        idempotency_identity: format!(
            "github-{operation}:payload:sha256:{:x}",
            Sha256::digest(payload.to_string().as_bytes())
        ),
        idempotency_scope: None,
        publication_subject: None,
        allow_result_cache,
    }
}

fn scoped_read_operation_identity(operation: &str, subject: &str) -> GithubAutomationIdentity {
    GithubAutomationIdentity {
        idempotency_identity: format!(
            "{}:read:{}",
            operation_subject(operation, subject),
            Uuid::new_v4()
        ),
        idempotency_scope: None,
        publication_subject: None,
        allow_result_cache: false,
    }
}

fn write_operation_identity(
    operation: &str,
    client_key: &str,
    payload: &Value,
    subject: Option<&str>,
) -> GithubAutomationIdentity {
    let subject = subject.map(|subject| operation_subject(operation, subject));
    let scope = match subject.as_deref() {
        Some(subject) => format!(
            "{subject}:client:sha256:{:x}",
            Sha256::digest(client_key.as_bytes())
        ),
        None => format!(
            "github-{operation}:client:sha256:{:x}",
            Sha256::digest(client_key.as_bytes())
        ),
    };
    let payload = payload.to_string();
    GithubAutomationIdentity {
        idempotency_identity: format!(
            "{scope}:payload:sha256:{:x}",
            Sha256::digest(payload.as_bytes())
        ),
        idempotency_scope: Some(scope),
        publication_subject: (operation == "publish").then_some(subject).flatten(),
        allow_result_cache: true,
    }
}

fn automation_subject(task_uid: Uuid, owner: &str, repository: &str) -> String {
    format!("{task_uid}:{owner}/{repository}")
}

fn operation_subject(operation: &str, subject: &str) -> String {
    let (task_uid, repository) = subject.split_once(':').unwrap_or((subject, subject));
    format!(
        "{}:subject:sha256:{:x}",
        task_operation_prefix(operation, task_uid),
        Sha256::digest(repository.as_bytes())
    )
}

fn task_operation_prefix(operation: &str, task_uid: impl std::fmt::Display) -> String {
    format!("github-{operation}:task:{task_uid}")
}

fn no_store_json<T: Serialize>(value: T) -> Response {
    (
        StatusCode::OK,
        [(header::CACHE_CONTROL, "no-store")],
        Json(value),
    )
        .into_response()
}

fn automation_error(error: ConnectionBrokerError) -> Response {
    let (status, reason) = match error {
        ConnectionBrokerError::OAuthFlowPending => (StatusCode::CONFLICT, None),
        ConnectionBrokerError::IdempotencyConflict => (StatusCode::UNPROCESSABLE_ENTITY, None),
        ConnectionBrokerError::RuntimeAuthenticationFailed
        | ConnectionBrokerError::ProxyPolicyDenied
        | ConnectionBrokerError::ProviderAuthorizationFailed => (StatusCode::FORBIDDEN, None),
        ConnectionBrokerError::BridgeContractInvalid => {
            (StatusCode::SERVICE_UNAVAILABLE, Some("bridge_contract"))
        }
        _ => (StatusCode::SERVICE_UNAVAILABLE, None),
    };
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(GithubAutomationErrorResponse {
            api_version: GITHUB_AUTOMATION_API_VERSION,
            error: "github_automation_unavailable",
            reason,
            manual_files: None,
        }),
    )
        .into_response()
}

fn automation_problem(
    error: &'static str,
    manual_files: Option<BTreeMap<String, String>>,
) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::CACHE_CONTROL, "no-store")],
        Json(GithubAutomationErrorResponse {
            api_version: GITHUB_AUTOMATION_API_VERSION,
            error,
            reason: None,
            manual_files,
        }),
    )
        .into_response()
}

fn unavailable() -> Response {
    automation_error(ConnectionBrokerError::Unavailable)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use steward_adapter_mcp_gw::{GithubBridgeOperation, GithubBridgeRequest};
    use steward_store::{
        AgentRunExecutionLog, AgentRunLogStream, AgentRunPage, AgentRunQuery, AgentRunRecord,
        AgentRunTimelineEvent, StoreError,
    };
    use steward_types::direct_package::{
        BrowserTaskEvidence, ClosureEntry, ClosureEntryKind, ContentDigest, PackageClosure,
        PromptSourceKind, RelativePath, TaskOrigin, canonical_json_bytes,
    };
    use steward_types::{
        AgentRuntimeSpec, AgentType, Budget, Duration as TaskDuration, Email, ModelRef, Principal,
        RuntimeOwnership, TaskPhase,
    };
    use tower::ServiceExt;

    use super::*;
    use crate::browser_auth::{
        LocalFakeIdentity, browser_auth_router, local_fake_browser_auth_service,
    };

    const OWNER_USER_ID: &str = "usr_0123456789abcdef0123456789abcdef";
    const ORIGIN: &str = "http://127.0.0.1:33001";

    #[derive(Clone, Default)]
    struct FakeLedger {
        records: Arc<Mutex<Vec<AgentRunRecord>>>,
    }

    impl AgentRunLedger for FakeLedger {
        fn agent_runs<'a>(
            &'a self,
            _query: &'a AgentRunQuery,
        ) -> BoxFuture<'a, Result<AgentRunPage, StoreError>> {
            Box::pin(async { Err(StoreError::InvalidRunQuery) })
        }

        fn agent_run(
            &self,
            task_uid: Uuid,
        ) -> BoxFuture<'_, Result<Option<AgentRunRecord>, StoreError>> {
            Box::pin(async move {
                Ok(self
                    .records
                    .lock()
                    .map_err(|_| StoreError::InvalidRunQuery)?
                    .iter()
                    .find(|record| record.task_uid == task_uid)
                    .cloned())
            })
        }

        fn agent_run_phase_facets<'a>(
            &'a self,
            _query: &'a AgentRunQuery,
        ) -> BoxFuture<'a, Result<BTreeMap<String, u64>, StoreError>> {
            Box::pin(async { Err(StoreError::InvalidRunQuery) })
        }

        fn cancel_agent_run<'a>(
            &'a self,
            _task_uid: Uuid,
            _owner_user_id: &'a str,
        ) -> BoxFuture<'a, Result<Option<AgentRunRecord>, StoreError>> {
            Box::pin(async { Err(StoreError::InvalidTaskTransition) })
        }

        fn agent_run_timeline(
            &self,
            _task_uid: Uuid,
        ) -> BoxFuture<'_, Result<Option<Vec<AgentRunTimelineEvent>>, StoreError>> {
            Box::pin(async { Ok(None) })
        }

        fn agent_run_execution_log<'a>(
            &'a self,
            _task_uid: Uuid,
            _owner_user_id: Option<&'a str>,
            _stream: AgentRunLogStream,
        ) -> BoxFuture<'a, Result<Option<AgentRunExecutionLog>, StoreError>> {
            Box::pin(async { Ok(None) })
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct BrokerCall {
        operation: ConnectionOperationKind,
        request: Value,
        idempotency_identity: String,
        allow_result_cache: bool,
    }

    #[derive(Clone, Default)]
    struct FakeBroker {
        calls: Arc<Mutex<Vec<BrokerCall>>>,
    }

    impl GithubAutomationBroker<BrowserSessionBinding> for FakeBroker {
        fn execute<'a>(
            &'a self,
            _session: &'a ConnectionSession<BrowserSessionBinding>,
            operation: ConnectionOperationKind,
            request: Value,
            identity: &'a GithubAutomationIdentity,
        ) -> BoxFuture<'a, Result<Value, ConnectionBrokerError>> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .map_err(|_| ConnectionBrokerError::Unavailable)?
                    .push(BrokerCall {
                        operation,
                        request,
                        idempotency_identity: identity.idempotency_identity.clone(),
                        allow_result_cache: identity.allow_result_cache,
                    });
                match operation {
                    ConnectionOperationKind::Repositories => Ok(json!({
                        "login": "alice",
                        "repositories": [
                            {
                                "owner": "example-org",
                                "ownerId": "100",
                                "name": "agentic-ops",
                                "repositoryId": "200",
                                "defaultBranch": "main",
                                "private": true,
                                "url": "https://github.com/example-org/agentic-ops"
                            },
                            {
                                "owner": "example-org",
                                "ownerId": "100",
                                "name": "not-admitted",
                                "repositoryId": "201",
                                "defaultBranch": "main",
                                "private": true,
                                "url": "https://github.com/example-org/not-admitted"
                            }
                        ],
                        "page": 1,
                        "hasNextPage": false
                    })),
                    ConnectionOperationKind::Workflow => Ok(json!({
                        "path": ".github/workflows/hypershell-hello.yml",
                        "exists": true,
                        "compatible": true,
                        "sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    })),
                    ConnectionOperationKind::Publish => Ok(json!({
                        "pullRequestUrl": "https://github.com/example-org/agentic-ops/pull/42",
                        "pullRequestNumber": 42,
                        "branch": "steward/task-11111111111141118111111111111111"
                    })),
                    ConnectionOperationKind::Dispatch => Ok(json!({
                        "runId": 12345,
                        "url": "https://github.com/example-org/agentic-ops/actions/runs/12345"
                    })),
                    ConnectionOperationKind::RunStatus => Ok(json!({
                        "runId": 12345,
                        "runAttempt": 1,
                        "phase": "completed",
                        "conclusion": "success",
                        "url": "https://github.com/example-org/agentic-ops/actions/runs/12345",
                        "jobs": []
                    })),
                    _ => Err(ConnectionBrokerError::Unavailable),
                }
            })
        }

        fn evidence<'a>(
            &'a self,
            _canonical_user_id: &'a CanonicalUserId,
            operation: ConnectionOperationKind,
            idempotency_identity_prefix: &'a str,
        ) -> BoxFuture<'a, Result<Option<Value>, ConnectionBrokerError>> {
            Box::pin(async move {
                let calls = self
                    .calls
                    .lock()
                    .map_err(|_| ConnectionBrokerError::Unavailable)?;
                if !calls.iter().any(|call| {
                    call.operation == operation
                        && call
                            .idempotency_identity
                            .starts_with(idempotency_identity_prefix)
                }) {
                    return Ok(None);
                }
                match operation {
                    ConnectionOperationKind::Workflow => Ok(Some(json!({
                        "path": ".github/workflows/hypershell-hello.yml",
                        "exists": true,
                        "compatible": true,
                        "sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    }))),
                    ConnectionOperationKind::Publish => Ok(Some(json!({
                        "pullRequestUrl": "https://github.com/example-org/agentic-ops/pull/42",
                        "pullRequestNumber": 42,
                        "branch": "steward/task-11111111111141118111111111111111"
                    }))),
                    ConnectionOperationKind::Dispatch => Ok(Some(json!({
                        "runId": 12345,
                        "url": "https://github.com/example-org/agentic-ops/actions/runs/12345"
                    }))),
                    _ => Ok(None),
                }
            })
        }
    }

    fn browser_run(task_uid: Uuid, owner_user_id: &str) -> Result<AgentRunRecord, String> {
        let content = r#"{"schemaVersion":"steward.task-definition/v2","promptText":"Say hello."}"#;
        let entry_point = RelativePath::parse(".steward/tasks/hello/task-definition.json")?;
        let file_digest = ContentDigest::parse(format!(
            "steward:sha256:{:x}",
            Sha256::digest(content.as_bytes())
        ))?;
        let closure = PackageClosure {
            contract_version: "steward.package-closure/v1".to_owned(),
            entry_point: entry_point.clone(),
            entries: vec![ClosureEntry {
                kind: ClosureEntryKind::TaskDefinition,
                path: entry_point.clone(),
                digest: file_digest,
                size_bytes: u64::try_from(content.len()).map_err(|error| error.to_string())?,
            }],
        };
        let closure_digest = ContentDigest::parse(format!(
            "steward:sha256:{:x}",
            Sha256::digest(canonical_json_bytes(&closure)?)
        ))?;
        let evidence = BrowserTaskEvidence {
            source: "inline".to_owned(),
            revision: closure_digest.as_str().to_owned(),
            path: entry_point.clone(),
            closure: Some(closure),
            closure_digest,
            inline_files: Some(BTreeMap::from([(
                entry_point.as_str().to_owned(),
                content.to_owned(),
            )])),
            diagnostics: Default::default(),
            prompt_source: PromptSourceKind::Inline,
        };
        evidence.validate()?;
        Ok(AgentRunRecord {
            task_uid,
            submitter_service: "browser".to_owned(),
            acting_user: Some("alice@example.com".to_owned()),
            owner: "alice@example.com".to_owned(),
            owner_user_id: Some(owner_user_id.to_owned()),
            owner_display_email: Some("alice@example.com".to_owned()),
            workflow: "browser-task@1".to_owned(),
            workflow_name: None,
            workflow_version: None,
            workflow_digest: None,
            user_envelope_instance_id: Some("envelope-instance-1".to_owned()),
            user_envelope_revision: Some(4),
            user_envelope_digest: Some(format!("sha256:{}", "b".repeat(64))),
            coding_agent_runtime: "agent-v1".to_owned(),
            runtime_uid: Some(format!("runtime-{task_uid}")),
            runtime_ownership: RuntimeOwnership::Provisioned,
            phase: TaskPhase::Succeeded,
            runtime_spec: AgentRuntimeSpec {
                principal: Principal::Service {
                    name: "browser".to_owned(),
                    acting_user: Some(Email("alice@example.com".to_owned())),
                },
                owner: Email("alice@example.com".to_owned()),
                canonical_authority: None,
                agent_type: AgentType {
                    name: "agent-v1".to_owned(),
                },
                llms: vec![ModelRef {
                    provider: "provider-a".to_owned(),
                    model: "model-a".to_owned(),
                }],
                tools: Vec::new(),
                budget: Budget {
                    monthly_limit: "100.00".to_owned(),
                    single_run_limit: None,
                    currency: "USD".to_owned(),
                },
                ttl: TaskDuration("24h".to_owned()),
                runner: steward_types::RunnerRequirements::default(),
                bindings: None,
            },
            envelope_revision: Some(1),
            finalize_requested: true,
            finalized: true,
            failure_reason: None,
            created_at: "2026-08-17T00:00:00.000000Z".to_owned(),
            updated_at: "2026-08-17T00:01:00.000000Z".to_owned(),
            spend: None,
            history_partial: false,
            direct_task_evidence: None,
            task_origin: TaskOrigin::Browser,
            browser_task_evidence: Some(evidence),
        })
    }

    fn config() -> Result<GithubAutomationConfig, String> {
        let bindings = json!({
            "contractVersion": "steward.source-repository-bindings/v1",
            "bindings": [{
                "caller": {"ownerId": "300", "repositoryId": "400"},
                "source": {"ownerId": "100", "repositoryId": "200"}
            }]
        })
        .to_string();
        Ok(GithubAutomationConfig::new(
            TaskApiConfig::default().with_source_repository_bindings_json(Some(&bindings))?,
            StewardRunRelease {
                manifest_schema_version: 3,
                version: "0.8.0".to_owned(),
                workflow_repository: "example-org/steward-run".to_owned(),
                workflow_commit: "a".repeat(40),
                action_commit: "b".repeat(40),
                governed_job_container_image: None,
            },
            StewardRunWorkflowInstallationMode::Remote,
            true,
        ))
    }

    #[tokio::test]
    async fn unsupported_steward_run_release_returns_specific_reason_and_manual_package()
    -> Result<(), String> {
        let task_uid = Uuid::parse_str("11111111-1111-4111-8111-111111111111")
            .map_err(|error| error.to_string())?;
        let ledger = FakeLedger::default();
        ledger
            .records
            .lock()
            .map_err(|_| "lock records")?
            .push(browser_run(task_uid, OWNER_USER_ID)?);
        let mut unsupported = config()?;
        unsupported.steward_run_release.version = "0.7.9".to_owned();
        let (auth, session_cookie, _) = signed_in_cookie_and_csrf().await?;
        let app = protected_router(ledger, FakeBroker::default(), unsupported, auth);

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/app/api/v1/runs/{task_uid}/github/bundle"))
                    .header(header::COOKIE, session_cookie)
                    .body(Body::empty())
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), 64 * 1024)
            .await
            .map_err(|error| error.to_string())?;
        let body: Value = serde_json::from_slice(&body).map_err(|error| error.to_string())?;
        assert_eq!(body["error"], "steward_run_release_unsupported");
        assert!(
            body["manualFiles"][".steward/tasks/hello/task-definition.json"]
                .as_str()
                .is_some()
        );
        Ok(())
    }

    fn cookie(response: &Response, name: &str) -> Result<String, String> {
        response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find(|value| value.starts_with(&format!("{name}=")))
            .and_then(|value| value.split(';').next())
            .map(str::to_owned)
            .ok_or_else(|| format!("response omitted {name} cookie"))
    }

    async fn signed_in_cookie_and_csrf() -> Result<(BrowserAuthService, String, String), String> {
        let service = local_fake_browser_auth_service(ORIGIN, LocalFakeIdentity::User)?;
        let login = browser_auth_router(service.clone())
            .oneshot(
                Request::builder()
                    .uri("/admin/auth/login")
                    .body(Body::empty())
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
        let flow_cookie = cookie(&login, "steward-local-oidc-flow")?;
        let authorize = login
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or("login omitted redirect")?;
        let authorized = browser_auth_router(service.clone())
            .oneshot(
                Request::builder()
                    .uri(authorize)
                    .body(Body::empty())
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
        let callback = authorized
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or("authorize omitted callback")?;
        let callback = browser_auth_router(service.clone())
            .oneshot(
                Request::builder()
                    .uri(callback)
                    .header(header::COOKIE, flow_cookie)
                    .body(Body::empty())
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
        let session_cookie = cookie(&callback, "steward-local-session")?;
        let session = browser_auth_router(service.clone())
            .oneshot(
                Request::builder()
                    .uri("/admin/api/v1/session")
                    .header(header::COOKIE, &session_cookie)
                    .body(Body::empty())
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
        let body = to_bytes(session.into_body(), 64 * 1024)
            .await
            .map_err(|error| error.to_string())?;
        let value: Value = serde_json::from_slice(&body).map_err(|error| error.to_string())?;
        let csrf = value["csrf"]
            .as_str()
            .ok_or("session omitted csrf")?
            .to_owned();
        Ok((service, session_cookie, csrf))
    }

    fn mutation_request(
        path: String,
        cookie: &str,
        csrf: &str,
        body: Value,
    ) -> Result<Request<Body>, String> {
        Request::builder()
            .method("POST")
            .uri(path)
            .header(header::COOKIE, cookie)
            .header(header::ORIGIN, ORIGIN)
            .header("sec-fetch-site", "same-origin")
            .header("x-steward-csrf", csrf)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .map_err(|error| error.to_string())
    }

    #[test]
    fn request_scalars_are_narrowly_bounded() {
        assert!(valid_repository_component("example-org", 39));
        assert!(!valid_repository_component("../other", 39));
        assert!(valid_input_name("task-inputs"));
        assert!(!valid_input_name("TASK INPUTS"));
        assert!(valid_idempotency_key("publish-123"));
        assert!(!valid_idempotency_key("publish/123"));
    }

    #[tokio::test]
    async fn bridge_contract_failure_has_a_bounded_automation_reason() -> Result<(), String> {
        let response = automation_error(ConnectionBrokerError::BridgeContractInvalid);
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), 1024)
            .await
            .map_err(|error| format!("read automation failure body: {error}"))?;
        assert_eq!(
            serde_json::from_slice::<Value>(&body)
                .map_err(|error| format!("parse automation failure body: {error}"))?,
            json!({
                "apiVersion": GITHUB_AUTOMATION_API_VERSION,
                "error": "github_automation_unavailable",
                "reason": "bridge_contract"
            })
        );
        Ok(())
    }

    #[test]
    fn write_idempotency_binds_one_client_key_to_one_semantic_payload() {
        let first_read = fresh_operation_identity("repositories");
        let second_read = fresh_operation_identity("repositories");
        assert_ne!(
            first_read.idempotency_identity,
            second_read.idempotency_identity
        );
        assert_eq!(first_read.idempotency_scope, None);
        assert_eq!(second_read.idempotency_scope, None);

        let first = write_operation_identity(
            "dispatch",
            "client-key",
            &json!({"inputs": {"message": "first"}}),
            None,
        );
        let retry = write_operation_identity(
            "dispatch",
            "client-key",
            &json!({"inputs": {"message": "first"}}),
            None,
        );
        let conflict = write_operation_identity(
            "dispatch",
            "client-key",
            &json!({"inputs": {"message": "other"}}),
            None,
        );
        assert_eq!(first, retry);
        assert_eq!(first.idempotency_scope, conflict.idempotency_scope);
        assert_ne!(first.idempotency_identity, conflict.idempotency_identity);

        let subject = "11111111-1111-4111-8111-111111111111:example-org/agentic-ops";
        let scoped = write_operation_identity(
            "dispatch",
            "client-key",
            &json!({"inputs": {}}),
            Some(subject),
        );
        assert!(scoped.idempotency_identity.starts_with(&format!(
            "{}:client:",
            operation_subject("dispatch", subject)
        )));
        assert_eq!(scoped.publication_subject, None);
    }

    #[tokio::test]
    async fn publication_is_owner_scoped_exact_and_idempotent_for_the_task_and_repository()
    -> Result<(), String> {
        let task_uid = Uuid::parse_str("11111111-1111-4111-8111-111111111111")
            .map_err(|error| error.to_string())?;
        let hidden_task = Uuid::parse_str("22222222-2222-4222-8222-222222222222")
            .map_err(|error| error.to_string())?;
        let source = browser_run(task_uid, OWNER_USER_ID)?;
        let expected_digest = source
            .browser_task_evidence
            .as_ref()
            .ok_or("missing browser evidence")?
            .closure_digest
            .as_str()
            .to_owned();
        let ledger = FakeLedger::default();
        ledger.records.lock().map_err(|_| "lock records")?.extend([
            source,
            browser_run(hidden_task, "usr_abcdefabcdefabcdefabcdefabcdefab")?,
        ]);
        let broker = FakeBroker::default();
        let (auth, session_cookie, csrf) = signed_in_cookie_and_csrf().await?;
        let app = protected_router(ledger, broker.clone(), config()?, auth);

        let bundle = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/app/api/v1/runs/{task_uid}/github/bundle"))
                    .header(header::COOKIE, &session_cookie)
                    .body(Body::empty())
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(bundle.status(), StatusCode::OK);
        let bundle = to_bytes(bundle.into_body(), 64 * 1024)
            .await
            .map_err(|error| error.to_string())?;
        let bundle: Value = serde_json::from_slice(&bundle).map_err(|error| error.to_string())?;
        assert_eq!(
            bundle["workflowPath"],
            ".github/workflows/hypershell-hello.yml"
        );
        assert_eq!(bundle["packageDigest"], expected_digest);
        assert!(
            bundle["files"][".github/workflows/hypershell-hello.yml"]
                .as_str()
                .is_some_and(|workflow| workflow
                    .contains("package-path: .steward/tasks/hello/task-definition.json"))
        );

        let repositories = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/app/api/v1/github/repositories?perPage=100")
                    .header(header::COOKIE, &session_cookie)
                    .body(Body::empty())
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(repositories.status(), StatusCode::OK);
        let repositories = to_bytes(repositories.into_body(), 64 * 1024)
            .await
            .map_err(|error| error.to_string())?;
        let repositories: Value =
            serde_json::from_slice(&repositories).map_err(|error| error.to_string())?;
        assert_eq!(repositories["repositories"][0]["ready"], true);
        assert_eq!(repositories["repositories"][1]["ready"], false);
        assert_eq!(
            repositories["repositories"][1]["missingPrerequisite"],
            "source_repository_not_admitted"
        );
        let default_repository_request = broker
            .calls
            .lock()
            .map_err(|_| "lock broker calls")?
            .iter()
            .find(|call| call.operation == ConnectionOperationKind::Repositories)
            .map(|call| call.request.clone())
            .ok_or("default repository request was not captured")?;
        assert_eq!(default_repository_request["query"], "");
        GithubBridgeRequest::parse(
            GithubBridgeOperation::Repositories,
            &serde_json::to_vec(&default_repository_request)
                .map_err(|error| format!("encode repository request: {error}"))?,
        )
        .map_err(|error| {
            format!("apiserver repository payload violates bridge contract: {error:?}")
        })?;

        for suffix in ["", "&refresh=true"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!(
                            "/app/api/v1/github/repositories?perPage=100{suffix}"
                        ))
                        .header(header::COOKIE, &session_cookie)
                        .body(Body::empty())
                        .map_err(|error| error.to_string())?,
                )
                .await
                .map_err(|error| error.to_string())?;
            assert_eq!(response.status(), StatusCode::OK);
        }
        let repository_calls = broker
            .calls
            .lock()
            .map_err(|_| "lock broker calls")?
            .iter()
            .filter(|call| call.operation == ConnectionOperationKind::Repositories)
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(repository_calls.len(), 3);
        assert_eq!(
            repository_calls[0].idempotency_identity, repository_calls[1].idempotency_identity,
            "identical repository reads must share one cache key"
        );
        assert_eq!(
            repository_calls[0].idempotency_identity, repository_calls[2].idempotency_identity,
            "refresh must bypass the completed value without creating a distinct cache key"
        );
        assert!(repository_calls[0].allow_result_cache);
        assert!(repository_calls[1].allow_result_cache);
        assert!(!repository_calls[2].allow_result_cache);

        for retry_key in ["first", "second"] {
            let response = app
                .clone()
                .oneshot(mutation_request(
                    format!("/app/api/v1/runs/{task_uid}/github/publish"),
                    &session_cookie,
                    &csrf,
                    json!({
                        "owner": "example-org",
                        "repository": "agentic-ops",
                        "idempotencyKey": retry_key
                    }),
                )?)
                .await
                .map_err(|error| error.to_string())?;
            assert_eq!(response.status(), StatusCode::OK);
            let body = to_bytes(response.into_body(), 64 * 1024)
                .await
                .map_err(|error| error.to_string())?;
            let body: Value = serde_json::from_slice(&body).map_err(|error| error.to_string())?;
            assert_eq!(body["packageDigest"], expected_digest);
        }

        let evidence = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/app/api/v1/runs/{task_uid}/github/evidence?owner=example-org&repository=agentic-ops"
                    ))
                    .header(header::COOKIE, &session_cookie)
                    .body(Body::empty())
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(evidence.status(), StatusCode::OK);
        let evidence = to_bytes(evidence.into_body(), 64 * 1024)
            .await
            .map_err(|error| error.to_string())?;
        let evidence: Value =
            serde_json::from_slice(&evidence).map_err(|error| error.to_string())?;
        assert_eq!(evidence["publication"]["pullRequestNumber"], 42);
        assert_eq!(evidence["publication"]["packageDigest"], expected_digest);
        assert!(evidence.get("dispatch").is_none());

        let dispatch = app
            .clone()
            .oneshot(mutation_request(
                format!("/app/api/v1/runs/{task_uid}/github/dispatch"),
                &session_cookie,
                &csrf,
                json!({
                    "owner": "example-org",
                    "repository": "agentic-ops",
                    "inputs": {"task-inputs": "{}"},
                    "idempotencyKey": "dispatch"
                }),
            )?)
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(dispatch.status(), StatusCode::OK);

        let evidence = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/app/api/v1/runs/{task_uid}/github/evidence?owner=example-org&repository=agentic-ops"
                    ))
                    .header(header::COOKIE, &session_cookie)
                    .body(Body::empty())
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
        let evidence = to_bytes(evidence.into_body(), 64 * 1024)
            .await
            .map_err(|error| error.to_string())?;
        let evidence: Value =
            serde_json::from_slice(&evidence).map_err(|error| error.to_string())?;
        assert_eq!(evidence["dispatch"]["runId"], 12345);

        let workflow = app
            .clone()
            .oneshot(mutation_request(
                format!("/app/api/v1/runs/{task_uid}/github/workflow"),
                &session_cookie,
                &csrf,
                json!({
                    "owner": "example-org",
                    "repository": "agentic-ops"
                }),
            )?)
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(workflow.status(), StatusCode::OK);

        let call_count = broker.calls.lock().map_err(|_| "lock broker calls")?.len();
        let onboarding = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/app/api/v1/runs/{task_uid}/github/onboarding"))
                    .header(header::COOKIE, &session_cookie)
                    .body(Body::empty())
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(onboarding.status(), StatusCode::OK);
        let onboarding = to_bytes(onboarding.into_body(), 64 * 1024)
            .await
            .map_err(|error| error.to_string())?;
        let onboarding: Value =
            serde_json::from_slice(&onboarding).map_err(|error| error.to_string())?;
        assert_eq!(onboarding["publicationObserved"], true);
        assert_eq!(onboarding["workflowObserved"], true);
        assert_eq!(onboarding["dispatchObserved"], true);
        assert_eq!(
            broker.calls.lock().map_err(|_| "lock broker calls")?.len(),
            call_count,
            "persisted onboarding evidence must not execute a governed GitHub operation"
        );

        {
            let calls = broker.calls.lock().map_err(|_| "lock broker calls")?;
            let publications = calls
                .iter()
                .filter(|call| call.operation == ConnectionOperationKind::Publish)
                .collect::<Vec<_>>();
            assert_eq!(publications.len(), 2);
            assert_ne!(
                publications[0].idempotency_identity, publications[1].idempotency_identity,
                "distinct client keys must retain distinct operation identities"
            );
            assert_eq!(
                publications[0]
                    .idempotency_identity
                    .split(":client:")
                    .next(),
                publications[1]
                    .idempotency_identity
                    .split(":client:")
                    .next(),
                "all retries of one Task and repository must share the publication subject"
            );
            let files = publications[0].request["files"]
                .as_array()
                .ok_or("publish request omitted files")?;
            assert_eq!(files.len(), 2);
            assert!(
                files
                    .iter()
                    .any(|file| file["path"] == ".steward/tasks/hello/task-definition.json")
            );
            assert!(
                files
                    .iter()
                    .any(|file| file["path"] == ".github/workflows/hypershell-hello.yml")
            );
            assert_eq!(publications[0].request["baseBranch"], "main");
            assert_ne!(publications[0].request["branch"], "main");
        }

        let hidden = app
            .oneshot(mutation_request(
                format!("/app/api/v1/runs/{hidden_task}/github/publish"),
                &session_cookie,
                &csrf,
                json!({
                    "owner": "example-org",
                    "repository": "agentic-ops",
                    "idempotencyKey": "hidden"
                }),
            )?)
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(hidden.status(), StatusCode::NOT_FOUND);
        Ok(())
    }
}
