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
use steward_types::direct_package::{
    BrowserTaskEvidence, ClosureEntryKind, DirectTaskDefinition, ExecutionLogMode, PackageClosure,
    TaskOrigin,
};
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
    ConnectionOperationKind, GovernedConnectionsBroker, ProviderConnectionStatusSource,
    SplitConnectionsBroker,
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
                &identity.idempotency_identity,
                identity.idempotency_scope.as_deref(),
                identity.publication_subject.as_deref(),
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
    /// Why an existing caller is not compatible: `caller_mismatch` when it is neither the
    /// generated caller for this Task nor its earlier digest-less rendering, or
    /// `package_mismatch` when the published package files differ from the tested closure.
    #[serde(skip_serializing_if = "Option::is_none")]
    mismatch: Option<&'static str>,
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

pub(crate) struct ExactRepositoryBundle {
    pub(crate) files: BTreeMap<String, String>,
    pub(crate) workflow_path: String,
    pub(crate) workflow_content: String,
    /// The same caller as rendered before callers recorded the package digest. Existing
    /// callers in this form stay valid when the published package files match exactly.
    pub(crate) previous_workflow_content: String,
    pub(crate) package_digest: String,
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
        (status = 409, body = GithubAutomationErrorResponse, description = "Tested package is not publishable"),
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
        (status = 409, body = GithubAutomationErrorResponse, description = "Run evidence is unavailable or invalid"),
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
        _ => return run_evidence_unavailable(),
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
        (status = 409, body = GithubAutomationErrorResponse, description = "Run evidence is unavailable or invalid"),
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
        _ => return run_evidence_unavailable(),
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
    let identity = fresh_operation_identity("repositories");
    let result = match state
        .broker
        .execute(
            &session,
            ConnectionOperationKind::Repositories,
            json!({
                "query": query.query,
                "page": query.page,
                "perPage": query.per_page,
            }),
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
        (status = 409, body = GithubAutomationErrorResponse, description = "Tested package is not publishable"),
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
    let verified = match verify_published_package(&state, &session, &repository, &bundle, || {
        scoped_read_operation_identity("workflow", &subject)
    })
    .await
    {
        Ok(verified) => verified,
        Err(response) => return response,
    };
    let mismatch = verified.mismatch();
    let mut response = verified.caller;
    response.compatible = mismatch.is_none() && response.exists;
    response.mismatch = mismatch;
    no_store_json(response)
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
        (status = 409, body = GithubAutomationErrorResponse, description = "Tested package is not publishable"),
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
    if let Err(response) = refuse_root_conflicts(&state, &session, &repository, &bundle).await {
        return response;
    }
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
        (status = 409, body = GithubAutomationErrorResponse, description = "Tested package is not publishable or the published workflow does not match it"),
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
    let verified = match verify_published_package(&state, &session, &repository, &bundle, || {
        fresh_operation_identity("dispatch-check")
    })
    .await
    {
        Ok(verified) => verified,
        Err(response) => return response,
    };
    let expected_content = match (verified.mismatch(), verified.matched_caller) {
        (None, Some(content)) if verified.caller.exists => content,
        (mismatch, _) => {
            return published_workflow_mismatch(mismatch.unwrap_or("caller_missing"));
        }
    };
    let operation_request = json!({
        "owner": repository.owner,
        "repo": repository.name,
        "workflowId": bundle.workflow_path,
        "ref": repository.default_branch,
        "inputs": request.inputs,
        "expectedContent": expected_content,
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

struct PublishedPackageVerification {
    /// The latest caller read: path, existence and blob SHA on the default branch.
    caller: WorkflowDetectionResponse,
    /// The accepted caller form found on the default branch, if any.
    matched_caller: Option<String>,
    /// Whether every published package file is byte-identical to the tested closure.
    package_matches: bool,
}

impl PublishedPackageVerification {
    const fn mismatch(&self) -> Option<&'static str> {
        if !self.caller.exists {
            None
        } else if self.matched_caller.is_none() {
            Some("caller_mismatch")
        } else if !self.package_matches {
            Some("package_mismatch")
        } else {
            None
        }
    }
}

/// Verify what the default branch would run. The caller must be the generated caller for
/// this Task, or its earlier digest-less rendering, and the package files at the published
/// paths must be byte-identical to the tested closure. Both caller forms name the same
/// package path, so the package content check is what binds a caller to this Task.
///
/// The caller read uses `caller_identity` only when the package matches, so the latest
/// task-scoped workflow evidence reflects the full decision.
async fn verify_published_package<L, P>(
    state: &GithubAutomationState<L, P>,
    session: &ConnectionSession<BrowserSessionBinding>,
    repository: &GithubRepositoryView,
    bundle: &ExactRepositoryBundle,
    caller_identity: impl Fn() -> GithubAutomationIdentity,
) -> Result<PublishedPackageVerification, Response>
where
    L: AgentRunLedger,
    P: GithubAutomationBroker<BrowserSessionBinding>,
{
    let mut package_matches = true;
    for (path, content) in bundle
        .files
        .iter()
        .filter(|(path, _)| **path != bundle.workflow_path)
    {
        let file = read_default_branch_file(
            state,
            session,
            repository,
            path,
            content,
            &fresh_operation_identity("package-file"),
        )
        .await?;
        package_matches &= file.exists && file.compatible;
    }
    let mut caller = None;
    let mut matched_caller = None;
    for content in [&bundle.workflow_content, &bundle.previous_workflow_content] {
        let identity = if package_matches {
            caller_identity()
        } else {
            fresh_operation_identity("workflow-check")
        };
        let read = read_default_branch_file(
            state,
            session,
            repository,
            &bundle.workflow_path,
            content,
            &identity,
        )
        .await?;
        let (exists, compatible) = (read.exists, read.compatible);
        caller = Some(read);
        if compatible {
            matched_caller = Some(content.clone());
        }
        if compatible || !exists {
            break;
        }
    }
    Ok(PublishedPackageVerification {
        caller: caller.ok_or_else(unavailable)?,
        matched_caller,
        package_matches,
    })
}

async fn read_default_branch_file<L, P>(
    state: &GithubAutomationState<L, P>,
    session: &ConnectionSession<BrowserSessionBinding>,
    repository: &GithubRepositoryView,
    path: &str,
    expected_content: &str,
    identity: &GithubAutomationIdentity,
) -> Result<WorkflowDetectionResponse, Response>
where
    L: AgentRunLedger,
    P: GithubAutomationBroker<BrowserSessionBinding>,
{
    let result = state
        .broker
        .execute(
            session,
            ConnectionOperationKind::Workflow,
            json!({
                "owner": repository.owner,
                "repo": repository.name,
                "path": path,
                "ref": repository.default_branch,
                "expectedContent": expected_content,
            }),
            identity,
        )
        .await
        .map_err(automation_error)?;
    let file = workflow_response(result).map_err(|()| unavailable())?;
    if file.path != path {
        return Err(unavailable());
    }
    Ok(file)
}

/// A legacy package publishes at the repository root. Refuse before any write when the base
/// branch already holds a different file at one of those paths; identical content is fine.
async fn refuse_root_conflicts<L, P>(
    state: &GithubAutomationState<L, P>,
    session: &ConnectionSession<BrowserSessionBinding>,
    repository: &GithubRepositoryView,
    bundle: &ExactRepositoryBundle,
) -> Result<(), Response>
where
    L: AgentRunLedger,
    P: GithubAutomationBroker<BrowserSessionBinding>,
{
    for (path, content) in bundle
        .files
        .iter()
        .filter(|(path, _)| steward_adapter_mcp_gw::legacy_root_package_path(path))
    {
        let existing = read_default_branch_file(
            state,
            session,
            repository,
            path,
            content,
            &fresh_operation_identity("package-file"),
        )
        .await?;
        if existing.exists && !existing.compatible {
            return Err(unpublishable(UnpublishableReason::RepositoryRootConflict));
        }
    }
    Ok(())
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
    tested_package_bundle(
        run.browser_task_evidence.as_ref(),
        run_envelope(&run),
        &state.config,
    )
    .map_err(TestedPackageBundleError::into_response)
}

/// Why a successful browser run's evidence cannot become a repository bundle. Each reason is
/// a stable, bounded wire value returned with `tested_package_unpublishable`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UnpublishableReason {
    /// The run recorded no valid browser package evidence.
    EvidenceUnavailable,
    /// The run executed a repository or registry package, not an inline package.
    SourceNotInline,
    /// The recorded inline files do not form a resolvable package closure.
    PackageFilesInvalid,
    /// The recorded inline files no longer reproduce the tested closure digest.
    ClosureMismatch,
    /// The closure is not a package the governed publication allowlist accepts.
    PackageShapeUnsupported,
    /// The repository's base branch already has a different file at a legacy root path.
    RepositoryRootConflict,
    /// The run recorded no complete User Envelope selection.
    EnvelopeUnavailable,
    /// The caller workflow could not be rendered for the recorded package.
    WorkflowUnavailable,
}

impl UnpublishableReason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::EvidenceUnavailable => "evidence_unavailable",
            Self::SourceNotInline => "source_not_inline",
            Self::PackageFilesInvalid => "package_files_invalid",
            Self::ClosureMismatch => "closure_mismatch",
            Self::PackageShapeUnsupported => "package_shape_unsupported",
            Self::RepositoryRootConflict => "repository_root_conflict",
            Self::EnvelopeUnavailable => "envelope_unavailable",
            Self::WorkflowUnavailable => "workflow_unavailable",
        }
    }
}

#[derive(Debug)]
pub(crate) enum TestedPackageBundleError {
    Unpublishable(UnpublishableReason),
    /// The reviewed `steward-run` release predates `package-path`; the exact package files are
    /// still offered for manual copy.
    ReleaseUnsupported(BTreeMap<String, String>),
}

impl TestedPackageBundleError {
    fn into_response(self) -> Response {
        match self {
            Self::Unpublishable(reason) => unpublishable(reason),
            Self::ReleaseUnsupported(files) => {
                automation_problem("steward_run_release_unsupported", Some(files))
            }
        }
    }
}

impl From<UnpublishableReason> for TestedPackageBundleError {
    fn from(reason: UnpublishableReason) -> Self {
        Self::Unpublishable(reason)
    }
}

fn run_envelope(run: &AgentRunRecord) -> Option<GithubActionsEnvelopeSelection> {
    Some(GithubActionsEnvelopeSelection {
        id: run.user_envelope_instance_id.clone()?,
        revision: u64::try_from(run.user_envelope_revision?).ok()?,
        digest: run.user_envelope_digest.clone()?,
    })
}

/// Render the exact tested inline package as a same-repository bundle.
///
/// Every inline file of the tested closure is published unchanged at its tested path, so the
/// package Steward resolves from the published commit has the tested closure digest. The
/// caller always uses `package-path`: same-repository `packagePath` resolution reads a
/// path-backed prompt from the same triggered commit with the same closure rules as
/// `invocation-path`, so single-file `promptText` packages and earlier two-file packages
/// (a `prompt` path beside the Task definition) share one invocation form.
pub(crate) fn tested_package_bundle(
    evidence: Option<&BrowserTaskEvidence>,
    envelope: Option<GithubActionsEnvelopeSelection>,
    config: &GithubAutomationConfig,
) -> Result<ExactRepositoryBundle, TestedPackageBundleError> {
    let evidence = evidence
        .filter(|evidence| evidence.validate().is_ok())
        .ok_or(UnpublishableReason::EvidenceUnavailable)?;
    if evidence.source != "inline" {
        return Err(UnpublishableReason::SourceNotInline.into());
    }
    let tested_closure = evidence
        .closure
        .as_ref()
        .ok_or(UnpublishableReason::EvidenceUnavailable)?;
    let files = evidence
        .inline_files
        .as_ref()
        .ok_or(UnpublishableReason::PackageFilesInvalid)?;
    let definition_source = files
        .get(evidence.path.as_str())
        .ok_or(UnpublishableReason::PackageFilesInvalid)?;
    let definition = serde_json::from_str::<DirectTaskDefinition>(definition_source)
        .map_err(|_| UnpublishableReason::PackageFilesInvalid)?;
    definition
        .validate()
        .map_err(|_| UnpublishableReason::PackageFilesInvalid)?;
    let (_, closure, closure_digest) = crate::tasks::resolve_inline_package_closure(
        &evidence.path,
        &definition,
        definition_source.as_bytes(),
        files,
    )
    .map_err(|_| UnpublishableReason::PackageFilesInvalid)?;
    if closure_digest != evidence.closure_digest || &closure != tested_closure {
        return Err(UnpublishableReason::ClosureMismatch.into());
    }
    if !publishable_closure_shape(&closure) {
        return Err(UnpublishableReason::PackageShapeUnsupported.into());
    }
    let mut files = files.clone();
    if !steward_run_supports_package_path_invocation(&config.steward_run_release) {
        return Err(TestedPackageBundleError::ReleaseUnsupported(files));
    }
    let envelope = envelope.ok_or(UnpublishableReason::EnvelopeUnavailable)?;
    let render = |package_digest: Option<String>| {
        render_direct_package_github_actions_workflow(&DirectPackageGithubActionsWorkflowContext {
            envelope: envelope.clone(),
            invocation_path: None,
            package_path: Some(evidence.path.as_str().to_owned()),
            package_digest,
            execution_log: ExecutionLogMode::Full,
            reviewed_release: config.steward_run_release.clone(),
            workflow_installation_mode: config.workflow_installation_mode,
            task_identity_discovery_enabled: config.task_identity_discovery_enabled,
        })
        .map_err(|_| UnpublishableReason::WorkflowUnavailable)
    };
    let generated = render(Some(closure_digest.as_str().to_owned()))?;
    let previous = render(None)?;
    if previous.suggested_path != generated.suggested_path {
        return Err(UnpublishableReason::WorkflowUnavailable.into());
    }
    if files
        .insert(generated.suggested_path.clone(), generated.yaml.clone())
        .is_some()
    {
        return Err(UnpublishableReason::PackageShapeUnsupported.into());
    }
    Ok(ExactRepositoryBundle {
        files,
        workflow_path: generated.suggested_path,
        workflow_content: generated.yaml,
        previous_workflow_content: previous.yaml,
        package_digest: closure_digest.as_str().to_owned(),
    })
}

/// The closure must be exactly a package the governed publication allowlist accepts: a
/// Task definition under `.steward/tasks/`, or the legacy root Task definition with its
/// root `prompt.md`. Anything else is refused here instead of failing at publication.
fn publishable_closure_shape(closure: &PackageClosure) -> bool {
    let mut definition = None;
    let mut prompt = None;
    for entry in &closure.entries {
        let slot = match entry.kind {
            ClosureEntryKind::TaskDefinition => &mut definition,
            ClosureEntryKind::Prompt => &mut prompt,
            _ => return false,
        };
        if slot.replace(entry.path.as_str()).is_some() {
            return false;
        }
    }
    definition == Some(closure.entry_point.as_str())
        && steward_adapter_mcp_gw::valid_publication_package(closure.entry_point.as_str(), prompt)
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
        mismatch: None,
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
        ConnectionBrokerError::ProviderResponseInvalid => (
            StatusCode::SERVICE_UNAVAILABLE,
            Some("bridge_response_contract"),
        ),
        ConnectionBrokerError::BridgeResultTooLarge => (
            StatusCode::SERVICE_UNAVAILABLE,
            Some("bridge_result_too_large"),
        ),
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

fn published_workflow_mismatch(reason: &'static str) -> Response {
    (
        StatusCode::CONFLICT,
        [(header::CACHE_CONTROL, "no-store")],
        Json(GithubAutomationErrorResponse {
            api_version: GITHUB_AUTOMATION_API_VERSION,
            error: "published_workflow_mismatch",
            reason: Some(reason),
            manual_files: None,
        }),
    )
        .into_response()
}

fn run_evidence_unavailable() -> Response {
    (
        StatusCode::CONFLICT,
        [(header::CACHE_CONTROL, "no-store")],
        Json(GithubAutomationErrorResponse {
            api_version: GITHUB_AUTOMATION_API_VERSION,
            error: "run_evidence_unavailable",
            reason: None,
            manual_files: None,
        }),
    )
        .into_response()
}

fn unpublishable(reason: UnpublishableReason) -> Response {
    (
        StatusCode::CONFLICT,
        [(header::CACHE_CONTROL, "no-store")],
        Json(GithubAutomationErrorResponse {
            api_version: GITHUB_AUTOMATION_API_VERSION,
            error: "tested_package_unpublishable",
            reason: Some(reason.as_str()),
            manual_files: None,
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
    use steward_adapter_mcp_gw::{GithubBridgeOperation, GithubBridgeRequest};
    use steward_store::{
        AgentRunExecutionLog, AgentRunLogStream, AgentRunPage, AgentRunQuery, AgentRunRecord,
        AgentRunTimelineEvent, StoreError,
    };
    use steward_types::direct_package::{
        BrowserTaskEvidence, DirectTaskDefinition, PromptSourceKind, RelativePath, TaskOrigin,
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
    }

    #[derive(Clone, Default)]
    struct FakeBroker {
        calls: Arc<Mutex<Vec<BrokerCall>>>,
        /// Files on the default branch, by repository path. `None` models a repository whose
        /// every read matches the expected content.
        base_files: Arc<Mutex<Option<BTreeMap<String, String>>>>,
        no_repository_hits: bool,
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
                        request: request.clone(),
                        idempotency_identity: identity.idempotency_identity.clone(),
                    });
                if operation == ConnectionOperationKind::Workflow {
                    let path = request["path"].as_str().unwrap_or_default();
                    let existing = match self
                        .base_files
                        .lock()
                        .map_err(|_| ConnectionBrokerError::Unavailable)?
                        .as_ref()
                    {
                        Some(files) => files.get(path).cloned(),
                        None => request["expectedContent"].as_str().map(str::to_owned),
                    };
                    return Ok(json!({
                        "path": path,
                        "exists": existing.is_some(),
                        "compatible": existing.as_deref() == request["expectedContent"].as_str(),
                        "sha": existing.map(|_| "a".repeat(40)),
                    }));
                }
                match operation {
                    ConnectionOperationKind::Repositories if self.no_repository_hits => Ok(json!({
                        "login": "alice",
                        "repositories": [],
                        "page": 1,
                        "hasNextPage": false
                    })),
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
        let entry_point = ".steward/tasks/hello/task-definition.json";
        let content = json!({
            "schemaVersion": "steward.task-definition/v2",
            "name": "hello",
            "version": 1,
            "runtime": {"agentRef": "example-agent@1.0.0"},
            "promptText": "Say hello.",
            "outputs": [{"path": "out", "kind": "directory", "required": true}]
        })
        .to_string();
        let evidence = inline_evidence(
            entry_point,
            BTreeMap::from([(entry_point.to_owned(), content)]),
        )?;
        assert_eq!(evidence.prompt_source, PromptSourceKind::Inline);
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

    const LEGACY_DEFINITION_PATH: &str = "task-definition.json";
    const LEGACY_PROMPT_PATH: &str = "prompt.md";

    /// The JSON Steward v0.3.8 stored for a successful inline run. Its closure digests were
    /// computed outside this crate (SHA-256 over sorted, compact JSON and the exact prompt).
    const V038_LEGACY_EVIDENCE: &str = r#"{
  "source": "inline",
  "revision": "steward:sha256:3304250fc7826c93a763d925cc6e5aac771c61fe775c9a3ad0f30d7e98b0c397",
  "path": "task-definition.json",
  "closure": {
    "contractVersion": "steward.package-closure/v1",
    "entryPoint": "task-definition.json",
    "entries": [
      {
        "kind": "prompt",
        "path": "prompt.md",
        "digest": "steward:sha256:faead127f0d4393295e10acbf471bb784b833a422e8a9ba2a8fe13ce3455d111",
        "sizeBytes": 134
      },
      {
        "kind": "task_definition",
        "path": "task-definition.json",
        "digest": "steward:sha256:e6fde5453c4583c15f7b1320d65157df91366a9d7e5cb124fafee36ffbe48cd2",
        "sizeBytes": 507
      }
    ]
  },
  "closureDigest": "steward:sha256:3304250fc7826c93a763d925cc6e5aac771c61fe775c9a3ad0f30d7e98b0c397",
  "inlineFiles": {
    "prompt.md": "Create $STEWARD_OUTPUT_DIR/out/hello.txt containing exactly the line: hello world. Use no tools and no network. Create no other files.",
    "task-definition.json": "{\n  \"schemaVersion\": \"steward.task-definition/v2\",\n  \"name\": \"browser-task\",\n  \"version\": 1,\n  \"runtime\": {\n    \"agentRef\": \"example-agent@1.0.0\",\n    \"model\": {\n      \"provider\": \"provider-a\",\n      \"model\": \"model-a\"\n    }\n  },\n  \"prompt\": \"prompt.md\",\n  \"outputs\": [\n    {\n      \"path\": \"out\",\n      \"kind\": \"directory\",\n      \"required\": true\n    }\n  ],\n  \"requires\": {\n    \"authority\": {\n      \"llms\": [\n        {\n          \"provider\": \"provider-a\",\n          \"model\": \"model-a\"\n        }\n      ],\n      \"tools\": [],\n      \"budget\": {\n        \"monthlyLimit\": \"100.00\",\n        \"singleRunLimit\": null,\n        \"currency\": \"USD\"\n      },\n      \"ttl\": \"24h\",\n      \"runner\": {\n        \"platforms\": [],\n        \"memory\": null,\n        \"compute\": null,\n        \"storage\": null\n      }\n    }\n  }\n}"
  },
  "diagnostics": {}
}"#;

    /// Browser evidence exactly as Steward v0.3.8 persisted it for an inline run: a root Task
    /// definition with a path-backed `prompt.md`, no `promptSource` field, and closure digests
    /// computed independently of the resolver under test.
    fn v038_legacy_evidence() -> Result<BrowserTaskEvidence, String> {
        let evidence = serde_json::from_str::<BrowserTaskEvidence>(V038_LEGACY_EVIDENCE)
            .map_err(|error| format!("v0.3.8 evidence fixture is invalid: {error}"))?;
        evidence.validate()?;
        assert_eq!(evidence.prompt_source, PromptSourceKind::Path);
        Ok(evidence)
    }

    fn legacy_package_files() -> Result<BTreeMap<String, String>, String> {
        v038_legacy_evidence()?
            .inline_files
            .ok_or_else(|| "v0.3.8 evidence fixture omitted its inline files".to_owned())
    }

    fn inline_evidence(
        entry_point: &str,
        files: BTreeMap<String, String>,
    ) -> Result<BrowserTaskEvidence, String> {
        let entry_point = RelativePath::parse(entry_point)?;
        let source = files
            .get(entry_point.as_str())
            .ok_or("inline package omitted its entry point")?;
        let definition = serde_json::from_str::<DirectTaskDefinition>(source)
            .map_err(|error| error.to_string())?;
        let (_, closure, closure_digest) = crate::tasks::resolve_inline_package_closure(
            &entry_point,
            &definition,
            source.as_bytes(),
            &files,
        )
        .map_err(|error| format!("resolve inline package closure: {error:?}"))?;
        let evidence = BrowserTaskEvidence {
            source: "inline".to_owned(),
            revision: closure_digest.as_str().to_owned(),
            path: entry_point,
            closure: Some(closure),
            closure_digest,
            inline_files: Some(files),
            diagnostics: Default::default(),
            prompt_source: PromptSourceKind::for_definition(&definition),
        };
        evidence.validate()?;
        Ok(evidence)
    }

    fn legacy_browser_run(task_uid: Uuid, owner_user_id: &str) -> Result<AgentRunRecord, String> {
        let mut record = browser_run(task_uid, owner_user_id)?;
        record.browser_task_evidence = Some(v038_legacy_evidence()?);
        Ok(record)
    }

    async fn json_body(response: Response) -> Result<Value, String> {
        let body = to_bytes(response.into_body(), 256 * 1024)
            .await
            .map_err(|error| error.to_string())?;
        serde_json::from_slice(&body).map_err(|error| error.to_string())
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

    #[tokio::test]
    async fn bridge_result_bound_has_its_own_automation_reason() -> Result<(), String> {
        let response = automation_error(ConnectionBrokerError::BridgeResultTooLarge);
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), 1024)
            .await
            .map_err(|error| format!("read automation failure body: {error}"))?;
        assert_eq!(
            serde_json::from_slice::<Value>(&body)
                .map_err(|error| format!("parse automation failure body: {error}"))?["reason"],
            "bridge_result_too_large",
            "Steward's own result bound is not a provider response contract failure"
        );
        Ok(())
    }

    #[tokio::test]
    async fn repository_lookup_without_search_hits_is_not_found() -> Result<(), String> {
        let task_uid = Uuid::parse_str("11111111-1111-4111-8111-111111111111")
            .map_err(|error| error.to_string())?;
        let ledger = FakeLedger::default();
        ledger
            .records
            .lock()
            .map_err(|_| "lock records")?
            .push(browser_run(task_uid, OWNER_USER_ID)?);
        let broker = FakeBroker {
            no_repository_hits: true,
            ..FakeBroker::default()
        };
        let (auth, session_cookie, csrf) = signed_in_cookie_and_csrf().await?;
        let app = protected_router(ledger, broker.clone(), config()?, auth);
        let response = app
            .oneshot(mutation_request(
                format!("/app/api/v1/runs/{task_uid}/github/workflow"),
                &session_cookie,
                &csrf,
                json!({"owner": "example-org", "repository": "example-repo"}),
            )?)
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let lookup = broker
            .calls
            .lock()
            .map_err(|_| "lock broker calls")?
            .iter()
            .find(|call| call.operation == ConnectionOperationKind::Repositories)
            .map(|call| call.request.clone())
            .ok_or("repository lookup was not captured")?;
        assert_eq!(lookup["query"], "repo:example-org/example-repo");
        Ok(())
    }

    #[tokio::test]
    async fn bridge_response_contract_failure_has_a_bounded_automation_reason() -> Result<(), String>
    {
        let response = automation_error(ConnectionBrokerError::ProviderResponseInvalid);
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
                "reason": "bridge_response_contract"
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

    #[tokio::test]
    async fn legacy_path_backed_run_publishes_its_exact_closure_through_a_package_path_caller()
    -> Result<(), String> {
        let task_uid = Uuid::parse_str("11111111-1111-4111-8111-111111111111")
            .map_err(|error| error.to_string())?;
        let source = legacy_browser_run(task_uid, OWNER_USER_ID)?;
        let tested = source
            .browser_task_evidence
            .clone()
            .ok_or("missing legacy browser evidence")?;
        let ledger = FakeLedger::default();
        ledger
            .records
            .lock()
            .map_err(|_| "lock records")?
            .push(source);
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
        let bundle = json_body(bundle).await?;
        assert_eq!(bundle["packageDigest"], tested.closure_digest.as_str());
        let workflow_path = bundle["workflowPath"]
            .as_str()
            .ok_or("bundle omitted its workflow path")?
            .to_owned();
        let files = bundle["files"]
            .as_object()
            .ok_or("bundle omitted its files")?;
        let mut expected_paths = vec![
            LEGACY_DEFINITION_PATH.to_owned(),
            LEGACY_PROMPT_PATH.to_owned(),
            workflow_path.clone(),
        ];
        expected_paths.sort();
        assert_eq!(files.keys().cloned().collect::<Vec<_>>(), expected_paths);
        let inline_files = tested
            .inline_files
            .as_ref()
            .ok_or("legacy evidence omitted inline files")?;
        for (path, content) in inline_files {
            assert_eq!(
                files[path].as_str(),
                Some(content.as_str()),
                "{path} must be published byte-for-byte as tested"
            );
        }
        let workflow = files[&workflow_path]
            .as_str()
            .ok_or("bundle omitted the workflow")?;
        assert!(workflow.contains(&format!("      package-path: {LEGACY_DEFINITION_PATH}\n")));
        assert!(!workflow.contains("invocation-path"));

        let publish = app
            .clone()
            .oneshot(mutation_request(
                format!("/app/api/v1/runs/{task_uid}/github/publish"),
                &session_cookie,
                &csrf,
                json!({
                    "owner": "example-org",
                    "repository": "agentic-ops",
                    "idempotencyKey": "legacy"
                }),
            )?)
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(publish.status(), StatusCode::OK);
        assert_eq!(
            json_body(publish).await?["packageDigest"],
            tested.closure_digest.as_str()
        );
        let mut publication = broker
            .calls
            .lock()
            .map_err(|_| "lock broker calls")?
            .iter()
            .find(|call| call.operation == ConnectionOperationKind::Publish)
            .map(|call| call.request.clone())
            .ok_or("publication request was not captured")?;
        let published = publication["files"]
            .as_array()
            .ok_or("publication omitted files")?
            .iter()
            .map(|file| {
                Ok((
                    file["path"].as_str().ok_or("file omitted path")?.to_owned(),
                    file["content"]
                        .as_str()
                        .ok_or("file omitted content")?
                        .to_owned(),
                ))
            })
            .collect::<Result<BTreeMap<_, _>, String>>()?;
        assert_eq!(published.len(), 3);
        for (path, content) in inline_files {
            assert_eq!(published.get(path), Some(content));
        }
        publication
            .as_object_mut()
            .ok_or("publication request is not an object")?
            .insert("resumeOwnedBranch".to_owned(), Value::Bool(false));
        GithubBridgeRequest::parse(
            GithubBridgeOperation::Publish,
            &serde_json::to_vec(&publication)
                .map_err(|error| format!("encode publication request: {error}"))?,
        )
        .map_err(|error| {
            format!("legacy publication payload violates bridge contract: {error:?}")
        })?;

        for (path, body) in [
            (
                "workflow",
                json!({"owner": "example-org", "repository": "agentic-ops"}),
            ),
            (
                "dispatch",
                json!({
                    "owner": "example-org",
                    "repository": "agentic-ops",
                    "inputs": {"task-inputs": "{}"},
                    "idempotencyKey": "legacy-dispatch"
                }),
            ),
        ] {
            let response = app
                .clone()
                .oneshot(mutation_request(
                    format!("/app/api/v1/runs/{task_uid}/github/{path}"),
                    &session_cookie,
                    &csrf,
                    body,
                )?)
                .await
                .map_err(|error| error.to_string())?;
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "{path} must accept the legacy bundle"
            );
        }
        let calls = broker.calls.lock().map_err(|_| "lock broker calls")?;
        let detection = calls
            .iter()
            .find(|call| {
                call.operation == ConnectionOperationKind::Workflow
                    && call.request["path"] == workflow_path.as_str()
            })
            .ok_or("workflow detection was not captured")?;
        let dispatch = calls
            .iter()
            .find(|call| call.operation == ConnectionOperationKind::Dispatch)
            .ok_or("dispatch was not captured")?;
        for request in [&detection.request, &dispatch.request] {
            assert_eq!(request["expectedContent"].as_str(), Some(workflow));
        }
        let publish_index = calls
            .iter()
            .position(|call| call.operation == ConnectionOperationKind::Publish)
            .ok_or("publication was not captured")?;
        for (path, content) in inline_files {
            let read = calls[..publish_index]
                .iter()
                .find(|call| {
                    call.operation == ConnectionOperationKind::Workflow
                        && call.request["path"] == path.as_str()
                })
                .ok_or_else(|| format!("{path} was not checked on the base branch"))?;
            assert_eq!(read.request["ref"], "main");
            assert_eq!(
                read.request["expectedContent"].as_str(),
                Some(content.as_str())
            );
            let mut bridge_read = read.request.clone();
            bridge_read["path"] = Value::String(path.clone());
            GithubBridgeRequest::parse(
                GithubBridgeOperation::Workflow,
                &serde_json::to_vec(&bridge_read).map_err(|error| error.to_string())?,
            )
            .map_err(|error| format!("{path} read violates the bridge contract: {error:?}"))?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn legacy_publication_refuses_different_root_files_and_accepts_identical_ones()
    -> Result<(), String> {
        let task_uid = Uuid::parse_str("11111111-1111-4111-8111-111111111111")
            .map_err(|error| error.to_string())?;
        let legacy_files = legacy_package_files()?;
        for (existing, conflict) in [
            (
                BTreeMap::from([(
                    LEGACY_DEFINITION_PATH.to_owned(),
                    "{\"family\":\"web\"}".to_owned(),
                )]),
                true,
            ),
            (
                BTreeMap::from([(LEGACY_PROMPT_PATH.to_owned(), "Other notes.\n".to_owned())]),
                true,
            ),
            (legacy_files.clone(), false),
            (BTreeMap::new(), false),
        ] {
            let ledger = FakeLedger::default();
            ledger
                .records
                .lock()
                .map_err(|_| "lock records")?
                .push(legacy_browser_run(task_uid, OWNER_USER_ID)?);
            let broker = FakeBroker::default();
            *broker.base_files.lock().map_err(|_| "lock base files")? = Some(existing);
            let (auth, session_cookie, csrf) = signed_in_cookie_and_csrf().await?;
            let response = protected_router(ledger, broker.clone(), config()?, auth)
                .oneshot(mutation_request(
                    format!("/app/api/v1/runs/{task_uid}/github/publish"),
                    &session_cookie,
                    &csrf,
                    json!({
                        "owner": "example-org",
                        "repository": "agentic-ops",
                        "idempotencyKey": "legacy-root"
                    }),
                )?)
                .await
                .map_err(|error| error.to_string())?;
            let published = broker
                .calls
                .lock()
                .map_err(|_| "lock broker calls")?
                .iter()
                .any(|call| call.operation == ConnectionOperationKind::Publish);
            if conflict {
                assert_eq!(response.status(), StatusCode::CONFLICT);
                assert_eq!(
                    json_body(response).await?,
                    json!({
                        "apiVersion": GITHUB_AUTOMATION_API_VERSION,
                        "error": "tested_package_unpublishable",
                        "reason": "repository_root_conflict"
                    })
                );
                assert!(
                    !published,
                    "a root conflict must be refused before any write"
                );
            } else {
                assert_eq!(response.status(), StatusCode::OK);
                assert!(published);
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn generated_workflows_bind_the_tested_closure_digest() -> Result<(), String> {
        let task_uid = Uuid::parse_str("11111111-1111-4111-8111-111111111111")
            .map_err(|error| error.to_string())?;
        let mut other_legacy = legacy_browser_run(task_uid, OWNER_USER_ID)?;
        let mut other_files = legacy_package_files()?;
        other_files.insert(
            LEGACY_PROMPT_PATH.to_owned(),
            "Summarize the repository in out/summary.md.\n".to_owned(),
        );
        other_legacy.browser_task_evidence =
            Some(inline_evidence(LEGACY_DEFINITION_PATH, other_files)?);
        let mut bundles = Vec::new();
        for record in [
            browser_run(task_uid, OWNER_USER_ID)?,
            legacy_browser_run(task_uid, OWNER_USER_ID)?,
            other_legacy,
        ] {
            let evidence = record
                .browser_task_evidence
                .clone()
                .ok_or("run omitted browser evidence")?;
            let bundle = tested_package_bundle(Some(&evidence), run_envelope(&record), &config()?)
                .map_err(|error| format!("bundle: {error:?}"))?;
            assert!(bundle.workflow_content.contains(&format!(
                "\n# package-digest: {}\n",
                evidence.closure_digest.as_str()
            )));
            bundles.push(bundle);
        }
        assert_eq!(
            bundles[1].workflow_path, bundles[2].workflow_path,
            "legacy packages share root paths and one caller path"
        );
        assert_ne!(
            bundles[1].workflow_content, bundles[2].workflow_content,
            "exact-content detection must reject another legacy Task's caller"
        );
        Ok(())
    }

    #[tokio::test]
    async fn detection_and_dispatch_accept_only_the_tested_package_under_either_caller_form()
    -> Result<(), String> {
        let task_uid = Uuid::parse_str("11111111-1111-4111-8111-111111111111")
            .map_err(|error| error.to_string())?;
        let bundle_for = |record: &AgentRunRecord| -> Result<ExactRepositoryBundle, String> {
            let evidence = record
                .browser_task_evidence
                .as_ref()
                .ok_or("run omitted browser evidence")?;
            tested_package_bundle(Some(evidence), run_envelope(record), &config()?)
                .map_err(|error| format!("bundle: {error:?}"))
        };
        let legacy = legacy_browser_run(task_uid, OWNER_USER_ID)?;
        let single = browser_run(task_uid, OWNER_USER_ID)?;
        let mut other = legacy_browser_run(task_uid, OWNER_USER_ID)?;
        let mut other_files = legacy_package_files()?;
        other_files.insert(
            LEGACY_PROMPT_PATH.to_owned(),
            "Summarize the repository in out/summary.md.\n".to_owned(),
        );
        other.browser_task_evidence = Some(inline_evidence(LEGACY_DEFINITION_PATH, other_files)?);
        let tested = bundle_for(&legacy)?;
        let other = bundle_for(&other)?;
        let single_bundle = bundle_for(&single)?;
        assert!(!tested.previous_workflow_content.contains("package-digest"));
        assert_eq!(
            tested.previous_workflow_content, other.previous_workflow_content,
            "digest-less legacy callers are identical for every legacy Task"
        );
        let published = |bundle: &ExactRepositoryBundle, caller: &str| {
            let mut files = bundle.files.clone();
            files.insert(bundle.workflow_path.clone(), caller.to_owned());
            files
        };
        let mut overwritten = published(&other, &tested.previous_workflow_content);
        overwritten.insert(
            tested.workflow_path.clone(),
            tested.previous_workflow_content.clone(),
        );

        for (record, base, expected) in [
            (
                &legacy,
                published(&tested, &tested.previous_workflow_content),
                Ok(tested.previous_workflow_content.clone()),
            ),
            (
                &legacy,
                published(&tested, &tested.workflow_content),
                Ok(tested.workflow_content.clone()),
            ),
            (
                &single,
                published(&single_bundle, &single_bundle.previous_workflow_content),
                Ok(single_bundle.previous_workflow_content.clone()),
            ),
            (&legacy, overwritten, Err(Some("package_mismatch"))),
            (
                &legacy,
                published(&tested, &other.workflow_content),
                Err(Some("caller_mismatch")),
            ),
            (&legacy, BTreeMap::new(), Err(None)),
        ] {
            let ledger = FakeLedger::default();
            ledger
                .records
                .lock()
                .map_err(|_| "lock records")?
                .push(record.clone());
            let broker = FakeBroker::default();
            *broker.base_files.lock().map_err(|_| "lock base files")? = Some(base);
            let (auth, session_cookie, csrf) = signed_in_cookie_and_csrf().await?;
            let app = protected_router(ledger, broker.clone(), config()?, auth);
            let detection = app
                .clone()
                .oneshot(mutation_request(
                    format!("/app/api/v1/runs/{task_uid}/github/workflow"),
                    &session_cookie,
                    &csrf,
                    json!({"owner": "example-org", "repository": "agentic-ops"}),
                )?)
                .await
                .map_err(|error| error.to_string())?;
            assert_eq!(detection.status(), StatusCode::OK);
            let detection = json_body(detection).await?;
            let dispatch = app
                .oneshot(mutation_request(
                    format!("/app/api/v1/runs/{task_uid}/github/dispatch"),
                    &session_cookie,
                    &csrf,
                    json!({
                        "owner": "example-org",
                        "repository": "agentic-ops",
                        "inputs": {"task-inputs": "{}"},
                        "idempotencyKey": "verified-dispatch"
                    }),
                )?)
                .await
                .map_err(|error| error.to_string())?;
            let dispatched = broker
                .calls
                .lock()
                .map_err(|_| "lock broker calls")?
                .iter()
                .find(|call| call.operation == ConnectionOperationKind::Dispatch)
                .map(|call| call.request["expectedContent"].clone());
            match expected {
                Ok(caller) => {
                    assert_eq!(detection["compatible"], true, "{detection}");
                    assert!(detection.get("mismatch").is_none());
                    assert_eq!(dispatch.status(), StatusCode::OK);
                    assert_eq!(dispatched, Some(Value::String(caller)));
                }
                Err(mismatch) => {
                    assert_eq!(detection["compatible"], false, "{detection}");
                    assert_eq!(detection["exists"], mismatch.is_some());
                    assert_eq!(detection.get("mismatch").and_then(Value::as_str), mismatch);
                    assert_eq!(dispatch.status(), StatusCode::CONFLICT);
                    assert_eq!(
                        json_body(dispatch).await?,
                        json!({
                            "apiVersion": GITHUB_AUTOMATION_API_VERSION,
                            "error": "published_workflow_mismatch",
                            "reason": mismatch.unwrap_or("caller_missing")
                        })
                    );
                    assert_eq!(
                        dispatched, None,
                        "a mismatched package must never be dispatched"
                    );
                }
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn evidence_lookups_report_unavailable_run_evidence() -> Result<(), String> {
        let task_uid = Uuid::parse_str("11111111-1111-4111-8111-111111111111")
            .map_err(|error| error.to_string())?;
        let mut record = browser_run(task_uid, OWNER_USER_ID)?;
        record.browser_task_evidence = None;
        let ledger = FakeLedger::default();
        ledger
            .records
            .lock()
            .map_err(|_| "lock records")?
            .push(record);
        let (auth, session_cookie, _) = signed_in_cookie_and_csrf().await?;
        let app = protected_router(ledger, FakeBroker::default(), config()?, auth);
        for uri in [
            format!("/app/api/v1/runs/{task_uid}/github/onboarding"),
            format!(
                "/app/api/v1/runs/{task_uid}/github/evidence?owner=example-org&repository=agentic-ops"
            ),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(&uri)
                        .header(header::COOKIE, &session_cookie)
                        .body(Body::empty())
                        .map_err(|error| error.to_string())?,
                )
                .await
                .map_err(|error| error.to_string())?;
            assert_eq!(response.status(), StatusCode::CONFLICT, "{uri}");
            assert_eq!(
                json_body(response).await?,
                json!({
                    "apiVersion": GITHUB_AUTOMATION_API_VERSION,
                    "error": "run_evidence_unavailable"
                })
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn single_file_publication_satisfies_the_bridge_contract() -> Result<(), String> {
        let task_uid = Uuid::parse_str("11111111-1111-4111-8111-111111111111")
            .map_err(|error| error.to_string())?;
        let ledger = FakeLedger::default();
        ledger
            .records
            .lock()
            .map_err(|_| "lock records")?
            .push(browser_run(task_uid, OWNER_USER_ID)?);
        let broker = FakeBroker::default();
        let (auth, session_cookie, csrf) = signed_in_cookie_and_csrf().await?;
        let response = protected_router(ledger, broker.clone(), config()?, auth)
            .oneshot(mutation_request(
                format!("/app/api/v1/runs/{task_uid}/github/publish"),
                &session_cookie,
                &csrf,
                json!({
                    "owner": "example-org",
                    "repository": "agentic-ops",
                    "idempotencyKey": "single"
                }),
            )?)
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(response.status(), StatusCode::OK);
        let mut publication = broker
            .calls
            .lock()
            .map_err(|_| "lock broker calls")?
            .iter()
            .find(|call| call.operation == ConnectionOperationKind::Publish)
            .map(|call| call.request.clone())
            .ok_or("publication request was not captured")?;
        assert_eq!(publication["files"].as_array().map(Vec::len), Some(2));
        publication
            .as_object_mut()
            .ok_or("publication request is not an object")?
            .insert("resumeOwnedBranch".to_owned(), Value::Bool(false));
        GithubBridgeRequest::parse(
            GithubBridgeOperation::Publish,
            &serde_json::to_vec(&publication)
                .map_err(|error| format!("encode publication request: {error}"))?,
        )
        .map_err(|error| format!("publication payload violates bridge contract: {error:?}"))?;
        Ok(())
    }

    #[tokio::test]
    async fn unsupported_release_offers_every_legacy_package_file_for_manual_copy()
    -> Result<(), String> {
        let task_uid = Uuid::parse_str("11111111-1111-4111-8111-111111111111")
            .map_err(|error| error.to_string())?;
        let ledger = FakeLedger::default();
        ledger
            .records
            .lock()
            .map_err(|_| "lock records")?
            .push(legacy_browser_run(task_uid, OWNER_USER_ID)?);
        let mut unsupported = config()?;
        unsupported.steward_run_release.version = "0.7.9".to_owned();
        let (auth, session_cookie, _) = signed_in_cookie_and_csrf().await?;
        let response = protected_router(ledger, FakeBroker::default(), unsupported, auth)
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
        let body = json_body(response).await?;
        assert_eq!(body["error"], "steward_run_release_unsupported");
        assert_eq!(
            serde_json::from_value::<BTreeMap<String, String>>(body["manualFiles"].clone())
                .map_err(|error| error.to_string())?,
            legacy_package_files()?
        );
        Ok(())
    }

    #[tokio::test]
    async fn unbundlable_evidence_returns_a_bounded_problem_instead_of_a_bare_conflict()
    -> Result<(), String> {
        let task_uid = Uuid::parse_str("11111111-1111-4111-8111-111111111111")
            .map_err(|error| error.to_string())?;
        let mut missing = legacy_browser_run(task_uid, OWNER_USER_ID)?;
        missing.browser_task_evidence = None;

        let mut tampered = legacy_browser_run(task_uid, OWNER_USER_ID)?;
        if let Some(files) = tampered
            .browser_task_evidence
            .as_mut()
            .and_then(|evidence| evidence.inline_files.as_mut())
        {
            files.insert(
                LEGACY_PROMPT_PATH.to_owned(),
                "A different prompt.\n".to_owned(),
            );
        }

        let mut unreferenced = legacy_browser_run(task_uid, OWNER_USER_ID)?;
        if let Some(files) = unreferenced
            .browser_task_evidence
            .as_mut()
            .and_then(|evidence| evidence.inline_files.as_mut())
        {
            files.insert("notes.md".to_owned(), "untested\n".to_owned());
        }

        let mut repository = browser_run(task_uid, OWNER_USER_ID)?;
        if let Some(evidence) = repository.browser_task_evidence.as_mut() {
            evidence.source = "https://github.com/example-org/agentic-ops.git".to_owned();
            evidence.revision = format!("git:sha1:{}", "a".repeat(40));
            evidence.inline_files = None;
        }

        let mut skilled = browser_run(task_uid, OWNER_USER_ID)?;
        skilled.browser_task_evidence = Some(inline_evidence(
            ".steward/tasks/hello/task-definition.json",
            BTreeMap::from([
                (
                    ".steward/tasks/hello/task-definition.json".to_owned(),
                    json!({
                        "schemaVersion": "steward.task-definition/v2",
                        "name": "hello",
                        "version": 1,
                        "runtime": {"agentRef": "example-agent@1.0.0"},
                        "promptText": "Say hello.",
                        "skills": ["skills/review/skill.json"],
                        "outputs": [{"path": "out", "kind": "directory", "required": true}]
                    })
                    .to_string(),
                ),
                (
                    ".steward/tasks/hello/skills/review/skill.json".to_owned(),
                    json!({
                        "schemaVersion": "steward.instruction-skill/v1",
                        "name": "review",
                        "description": "Apply the review instructions.",
                        "instructions": "instructions.md"
                    })
                    .to_string(),
                ),
                (
                    ".steward/tasks/hello/skills/review/instructions.md".to_owned(),
                    "Review carefully.\n".to_owned(),
                ),
            ]),
        )?);

        let root_prompt_text = |path: &str| -> Result<AgentRunRecord, String> {
            let mut record = browser_run(task_uid, OWNER_USER_ID)?;
            record.browser_task_evidence = Some(inline_evidence(
                path,
                BTreeMap::from([(
                    path.to_owned(),
                    json!({
                        "schemaVersion": "steward.task-definition/v2",
                        "name": "hello",
                        "version": 1,
                        "runtime": {"agentRef": "example-agent@1.0.0"},
                        "promptText": "Say hello.",
                        "outputs": [{"path": "out", "kind": "directory", "required": true}]
                    })
                    .to_string(),
                )]),
            )?);
            Ok(record)
        };
        let root_single = root_prompt_text(LEGACY_DEFINITION_PATH)?;
        let outside_tasks = root_prompt_text("catalog/hello/task-definition.json")?;

        let mut no_envelope = legacy_browser_run(task_uid, OWNER_USER_ID)?;
        no_envelope.user_envelope_digest = None;

        for (record, reason) in [
            (missing, "evidence_unavailable"),
            (tampered, "closure_mismatch"),
            (unreferenced, "package_files_invalid"),
            (repository, "source_not_inline"),
            (skilled, "package_shape_unsupported"),
            (root_single, "package_shape_unsupported"),
            (outside_tasks, "package_shape_unsupported"),
            (no_envelope, "envelope_unavailable"),
        ] {
            let ledger = FakeLedger::default();
            ledger
                .records
                .lock()
                .map_err(|_| "lock records")?
                .push(record);
            let (auth, session_cookie, _) = signed_in_cookie_and_csrf().await?;
            let response = protected_router(ledger, FakeBroker::default(), config()?, auth)
                .oneshot(
                    Request::builder()
                        .uri(format!("/app/api/v1/runs/{task_uid}/github/bundle"))
                        .header(header::COOKIE, session_cookie)
                        .body(Body::empty())
                        .map_err(|error| error.to_string())?,
                )
                .await
                .map_err(|error| error.to_string())?;
            assert_eq!(response.status(), StatusCode::CONFLICT, "{reason}");
            assert_eq!(
                json_body(response).await?,
                json!({
                    "apiVersion": GITHUB_AUTOMATION_API_VERSION,
                    "error": "tested_package_unpublishable",
                    "reason": reason
                }),
            );
        }
        Ok(())
    }
}
