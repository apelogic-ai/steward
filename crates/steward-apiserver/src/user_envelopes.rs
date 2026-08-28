//! Browser-session-bound user envelope request API.

use std::hash::Hash;

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use steward_admission::{AdmissionDecision, Envelope, evaluate, validate_envelope};
use steward_store::{
    EnvelopeRequestRecord, EnvelopeRequestReservationRequest, PgStore, StoreError,
    WorkflowRevisionRecord,
};
use steward_types::{
    AgentRuntimeSpec, AgentType, Budget, CanonicalUserId, Duration, Email, ModelRef, Principal,
    RunnerRequirements, ToolGrant,
};
use uuid::Uuid;

use crate::browser_auth::{
    BrowserAuthService, BrowserMutationProof, BrowserSessionBinding, BrowserSessionContext,
    protect_browser_routes,
};
use crate::{
    BoxFuture, GithubActionsEnvelopeSelection, VersionedGithubActionsWorkflowContext,
    render_versioned_github_actions_workflow, reviewed_steward_run_release_v2,
};

pub const ENVELOPE_REQUESTS_API_VERSION: &str = "steward.envelope-requests/v1";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserEnvelopeSubject {
    pub canonical_user_id: CanonicalUserId,
    pub display_email: Email,
    pub member_roles: Vec<String>,
}

/// The session binding is opaque to the request domain. It is only used by a broker to bind
/// one-time actions to the same browser session that initiated them.
#[derive(Clone, Eq, PartialEq)]
pub struct UserEnvelopeSession<B> {
    pub subject: UserEnvelopeSubject,
    pub binding: B,
}

#[derive(Clone, Copy)]
pub(crate) struct UserEnvelopeMutationProof;

fn subject_from_browser_session(context: &BrowserSessionContext) -> UserEnvelopeSubject {
    UserEnvelopeSubject {
        canonical_user_id: context.principal.canonical_user_id.clone(),
        display_email: context.principal.display_email.clone(),
        member_roles: context.principal.member_roles.clone(),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionReadiness {
    Connected,
    ReauthRequired,
    Missing,
}

#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AvailableEnvelopeTemplate {
    pub id: String,
    pub display_name: String,
    pub revision: i64,
    #[schema(value_type = BrowserEnvelope)]
    pub ceiling: Envelope,
    #[schema(value_type = Option<BrowserEnvelope>)]
    pub auto_provision_threshold: Option<Envelope>,
    pub github_connection: ConnectionReadiness,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum EnvelopeRequestStatus {
    Pending,
    Approved,
    Rejected,
    Provisioned,
    Stale,
    Conflict,
}

#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UserEnvelopeRequest {
    #[schema(value_type = String, format = "uuid")]
    pub id: Uuid,
    pub template_id: String,
    pub template_revision: i64,
    #[schema(value_type = BrowserEnvelope)]
    pub requested_envelope: Envelope,
    #[schema(value_type = Option<BrowserEnvelope>)]
    pub approved_envelope: Option<Envelope>,
    pub status: EnvelopeRequestStatus,
    #[schema(value_type = Option<String>, format = "uuid")]
    pub approval_id: Option<Uuid>,
    pub envelope_instance_id: Option<String>,
    pub envelope_digest: Option<String>,
    pub reason: Option<String>,
    pub created_at: String,
    pub status_at: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowserEnvelope {
    pub revision: i64,
    pub spec: BrowserEnvelopeSpec,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowserEnvelopeSpec {
    pub llms: Vec<ModelRef>,
    pub tools: Vec<ToolGrant>,
    pub budget: Budget,
    pub ttl: Duration,
    #[serde(default)]
    pub runner: RunnerRequirements,
}

impl From<Envelope> for BrowserEnvelope {
    fn from(envelope: Envelope) -> Self {
        Self {
            revision: envelope.revision,
            spec: BrowserEnvelopeSpec {
                llms: envelope.spec.llms,
                tools: envelope.spec.tools,
                budget: envelope.spec.budget,
                ttl: envelope.spec.ttl,
                runner: envelope.spec.runner,
            },
        }
    }
}

impl From<BrowserEnvelope> for Envelope {
    fn from(envelope: BrowserEnvelope) -> Self {
        Self {
            revision: envelope.revision,
            spec: steward_admission::EnvelopeSpec {
                llms: envelope.spec.llms,
                tools: envelope.spec.tools,
                budget: envelope.spec.budget,
                ttl: envelope.spec.ttl,
                runner: envelope.spec.runner,
            },
        }
    }
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EnvelopeTemplatesResponse {
    api_version: &'static str,
    templates: Vec<AvailableEnvelopeTemplate>,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EnvelopeRequestsResponse {
    api_version: &'static str,
    requests: Vec<UserEnvelopeRequest>,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EnvelopeRequestResponse {
    api_version: &'static str,
    request: UserEnvelopeRequest,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GithubActionsWorkflowResponse {
    api_version: &'static str,
    workflow: crate::GeneratedGithubActionsWorkflow,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CreateEnvelopeRequestBody {
    template_id: String,
    template_revision: i64,
    requested_envelope: BrowserEnvelope,
    idempotency_key: String,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RenderGithubActionsWorkflowBody {
    workflow: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PublishedWorkflowOption {
    name: String,
    version: i64,
    display_name: String,
    agent: String,
}

impl From<WorkflowRevisionRecord> for PublishedWorkflowOption {
    fn from(record: WorkflowRevisionRecord) -> Self {
        Self {
            name: record.name,
            version: record.version,
            display_name: record.display_name,
            agent: record.agent,
        }
    }
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PublishedWorkflowsResponse {
    api_version: &'static str,
    workflows: Vec<PublishedWorkflowOption>,
}

/// Submission passed only after the HTTP boundary has derived its canonical owner, required an
/// exact template revision, verified the hard ceiling, and decided the auto-provision threshold.
pub struct ValidatedEnvelopeRequest<'a> {
    pub template: &'a AvailableEnvelopeTemplate,
    pub requested_envelope: &'a Envelope,
    pub idempotency_key: &'a str,
    pub auto_provision: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnvelopeRequestBrokerError {
    NotFound,
    Conflict,
    Unavailable,
}

/// Production request broker backed by the authoritative envelope and request ledgers.
///
/// A template becomes visible only through a local-RBAC member-role assignment. Its current
/// envelope is the hard ceiling. Automatic provisioning remains absent until an independently
/// persisted threshold and reconciler are supplied; this broker never infers either one.
#[derive(Clone)]
pub struct PgEnvelopeRequestBroker {
    store: PgStore,
}

fn has_tools(envelope: &Envelope) -> bool {
    !envelope.spec.tools.is_empty()
}

fn connection_readiness_for_ceiling(envelope: &Envelope) -> ConnectionReadiness {
    if has_tools(envelope) {
        ConnectionReadiness::Missing
    } else {
        ConnectionReadiness::Connected
    }
}

impl PgEnvelopeRequestBroker {
    pub fn new(store: PgStore) -> Self {
        Self { store }
    }
}

impl EnvelopeRequestBroker<BrowserSessionBinding> for PgEnvelopeRequestBroker {
    fn templates<'a>(
        &'a self,
        session: &'a UserEnvelopeSession<BrowserSessionBinding>,
    ) -> BoxFuture<'a, Result<Vec<AvailableEnvelopeTemplate>, EnvelopeRequestBrokerError>> {
        Box::pin(async move {
            let mut templates = Vec::new();
            for member_role in &session.subject.member_roles {
                if let Some(ceiling) = self
                    .store
                    .latest_envelope(member_role)
                    .await
                    .map_err(map_store_broker_error)?
                {
                    let github_connection = connection_readiness_for_ceiling(&ceiling);
                    templates.push(AvailableEnvelopeTemplate {
                        id: member_role.clone(),
                        display_name: member_role.clone(),
                        revision: ceiling.revision,
                        ceiling,
                        auto_provision_threshold: None,
                        github_connection,
                    });
                }
            }
            templates.sort_by(|left, right| left.id.cmp(&right.id));
            Ok(templates)
        })
    }

    fn list<'a>(
        &'a self,
        session: &'a UserEnvelopeSession<BrowserSessionBinding>,
    ) -> BoxFuture<'a, Result<Vec<UserEnvelopeRequest>, EnvelopeRequestBrokerError>> {
        Box::pin(async move {
            self.store
                .envelope_requests(&session.subject.canonical_user_id)
                .await
                .map(|records| records.into_iter().map(user_envelope_request).collect())
                .map_err(map_store_broker_error)
        })
    }

    fn get<'a>(
        &'a self,
        session: &'a UserEnvelopeSession<BrowserSessionBinding>,
        request_id: Uuid,
    ) -> BoxFuture<'a, Result<Option<UserEnvelopeRequest>, EnvelopeRequestBrokerError>> {
        Box::pin(async move {
            self.store
                .envelope_request(&session.subject.canonical_user_id, request_id)
                .await
                .map(|record| record.map(user_envelope_request))
                .map_err(map_store_broker_error)
        })
    }

    fn create<'a>(
        &'a self,
        session: &'a UserEnvelopeSession<BrowserSessionBinding>,
        request: ValidatedEnvelopeRequest<'a>,
    ) -> BoxFuture<'a, Result<UserEnvelopeRequest, EnvelopeRequestBrokerError>> {
        Box::pin(async move {
            let reservation = self
                .store
                .reserve_envelope_request(EnvelopeRequestReservationRequest {
                    owner_user_id: &session.subject.canonical_user_id,
                    template_id: &request.template.id,
                    template_revision: request.template.revision,
                    requested_envelope: request.requested_envelope,
                    idempotency_key: request.idempotency_key,
                })
                .await
                .map_err(map_store_broker_error)?;
            Ok(user_envelope_request(reservation.record))
        })
    }

    fn workflows<'a>(
        &'a self,
        _session: &'a UserEnvelopeSession<BrowserSessionBinding>,
    ) -> BoxFuture<'a, Result<Vec<WorkflowRevisionRecord>, EnvelopeRequestBrokerError>> {
        Box::pin(async move {
            self.store
                .list_latest_workflows()
                .await
                .map_err(map_store_broker_error)
        })
    }

    fn workflow<'a>(
        &'a self,
        _session: &'a UserEnvelopeSession<BrowserSessionBinding>,
        name: &'a str,
        version: i64,
    ) -> BoxFuture<'a, Result<Option<WorkflowRevisionRecord>, EnvelopeRequestBrokerError>> {
        Box::pin(async move {
            self.store
                .workflow_revision(name, version)
                .await
                .map_err(map_store_broker_error)
        })
    }
}

fn user_envelope_request(record: EnvelopeRequestRecord) -> UserEnvelopeRequest {
    UserEnvelopeRequest {
        id: record.id,
        template_id: record.template_id,
        template_revision: record.template_revision,
        requested_envelope: record.requested_envelope,
        approved_envelope: record.approved_envelope,
        status: match record.status {
            steward_store::EnvelopeRequestStatus::Pending => EnvelopeRequestStatus::Pending,
            steward_store::EnvelopeRequestStatus::Approved => EnvelopeRequestStatus::Approved,
            steward_store::EnvelopeRequestStatus::Rejected => EnvelopeRequestStatus::Rejected,
            steward_store::EnvelopeRequestStatus::Provisioned => EnvelopeRequestStatus::Provisioned,
            steward_store::EnvelopeRequestStatus::Stale => EnvelopeRequestStatus::Stale,
            steward_store::EnvelopeRequestStatus::Conflict => EnvelopeRequestStatus::Conflict,
        },
        approval_id: record.approval_id,
        envelope_instance_id: record.envelope_instance_id,
        envelope_digest: record.envelope_digest,
        reason: record.reason,
        created_at: record.created_at,
        status_at: record.status_at,
    }
}

fn map_store_broker_error(error: StoreError) -> EnvelopeRequestBrokerError {
    match error {
        StoreError::EnvelopeRequestNotFound => EnvelopeRequestBrokerError::NotFound,
        StoreError::EnvelopeRequestIdempotencyConflict
        | StoreError::InvalidEnvelopeRequest
        | StoreError::InvalidEnvelopeRequestTransition => EnvelopeRequestBrokerError::Conflict,
        _ => EnvelopeRequestBrokerError::Unavailable,
    }
}

/// Server-side request broker. Every method receives the canonical browser subject derived by
/// middleware; no method accepts a browser-supplied owner or provider credential.
pub trait EnvelopeRequestBroker<B>: Clone + Send + Sync + 'static
where
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    fn templates<'a>(
        &'a self,
        session: &'a UserEnvelopeSession<B>,
    ) -> BoxFuture<'a, Result<Vec<AvailableEnvelopeTemplate>, EnvelopeRequestBrokerError>>;

    fn list<'a>(
        &'a self,
        session: &'a UserEnvelopeSession<B>,
    ) -> BoxFuture<'a, Result<Vec<UserEnvelopeRequest>, EnvelopeRequestBrokerError>>;

    fn get<'a>(
        &'a self,
        session: &'a UserEnvelopeSession<B>,
        request_id: Uuid,
    ) -> BoxFuture<'a, Result<Option<UserEnvelopeRequest>, EnvelopeRequestBrokerError>>;

    fn create<'a>(
        &'a self,
        session: &'a UserEnvelopeSession<B>,
        request: ValidatedEnvelopeRequest<'a>,
    ) -> BoxFuture<'a, Result<UserEnvelopeRequest, EnvelopeRequestBrokerError>>;

    fn workflows<'a>(
        &'a self,
        _session: &'a UserEnvelopeSession<B>,
    ) -> BoxFuture<'a, Result<Vec<WorkflowRevisionRecord>, EnvelopeRequestBrokerError>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn workflow<'a>(
        &'a self,
        _session: &'a UserEnvelopeSession<B>,
        _name: &'a str,
        _version: i64,
    ) -> BoxFuture<'a, Result<Option<WorkflowRevisionRecord>, EnvelopeRequestBrokerError>> {
        Box::pin(async { Ok(None) })
    }
}

#[derive(Clone)]
pub(crate) struct UserEnvelopeState<P> {
    broker: P,
}

fn inner_router<P, B>(broker: P) -> Router
where
    P: EnvelopeRequestBroker<B>,
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    Router::new()
        .route(
            "/app/api/v1/envelope-templates",
            get(list_templates::<P, B>),
        )
        .route(
            "/app/api/v1/envelope-requests",
            get(list_requests::<P, B>).post(create_request::<P, B>),
        )
        .route(
            "/app/api/v1/envelope-requests/{request_id}",
            get(get_request::<P, B>),
        )
        .route(
            "/app/api/v1/envelope-requests/{request_id}/github-actions-workflow",
            post(render_github_actions_for_envelope::<P, B>),
        )
        .route("/app/api/v1/workflows", get(list_workflows::<P, B>))
        .with_state(UserEnvelopeState { broker })
}

#[utoipa::path(
    get,
    operation_id = "listPublishedWorkflows",
    path = "/app/api/v1/workflows",
    responses(
        (status = 200, body = PublishedWorkflowsResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 503, description = "Workflow store is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn list_workflows<P, B>(
    session: Option<Extension<UserEnvelopeSession<B>>>,
    State(state): State<UserEnvelopeState<P>>,
) -> Response
where
    P: EnvelopeRequestBroker<B>,
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    match state.broker.workflows(&session).await {
        Ok(workflows) => Json(PublishedWorkflowsResponse {
            api_version: ENVELOPE_REQUESTS_API_VERSION,
            workflows: workflows.into_iter().map(Into::into).collect(),
        })
        .into_response(),
        Err(error) => broker_error_response(error),
    }
}

/// Mount user envelope APIs behind the common browser-session, same-origin, CSRF and
/// fetch-metadata boundary. User pages only learn authoritative records returned by their broker.
pub fn protected_router<P>(broker: P, browser_auth: BrowserAuthService) -> Router
where
    P: EnvelopeRequestBroker<BrowserSessionBinding>,
{
    let routes = inner_router(broker).route_layer(middleware::from_fn(adapt_browser_context));
    protect_browser_routes(routes, browser_auth)
}

async fn adapt_browser_context(mut request: Request, next: Next) -> Response {
    if let Some(context) = request.extensions().get::<BrowserSessionContext>().cloned() {
        request.extensions_mut().insert(UserEnvelopeSession {
            subject: subject_from_browser_session(&context),
            binding: context.binding,
        });
        if request.extensions().get::<BrowserMutationProof>().is_some() {
            request.extensions_mut().insert(UserEnvelopeMutationProof);
        }
    }
    next.run(request).await
}

#[utoipa::path(
    get,
    path = "/app/api/v1/envelope-templates",
    responses(
        (status = 200, body = EnvelopeTemplatesResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 503, description = "Envelope templates are unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn list_templates<P, B>(
    session: Option<Extension<UserEnvelopeSession<B>>>,
    State(state): State<UserEnvelopeState<P>>,
) -> Response
where
    P: EnvelopeRequestBroker<B>,
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    match state.broker.templates(&session).await {
        Ok(templates) => Json(EnvelopeTemplatesResponse {
            api_version: ENVELOPE_REQUESTS_API_VERSION,
            templates,
        })
        .into_response(),
        Err(error) => broker_error_response(error),
    }
}

#[utoipa::path(
    get,
    path = "/app/api/v1/envelope-requests",
    responses(
        (status = 200, body = EnvelopeRequestsResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 503, description = "Envelope requests are unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn list_requests<P, B>(
    session: Option<Extension<UserEnvelopeSession<B>>>,
    State(state): State<UserEnvelopeState<P>>,
) -> Response
where
    P: EnvelopeRequestBroker<B>,
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    match state.broker.list(&session).await {
        Ok(requests) => Json(EnvelopeRequestsResponse {
            api_version: ENVELOPE_REQUESTS_API_VERSION,
            requests,
        })
        .into_response(),
        Err(error) => broker_error_response(error),
    }
}

#[utoipa::path(
    get,
    path = "/app/api/v1/envelope-requests/{request_id}",
    params(("request_id" = String, Path)),
    responses(
        (status = 200, body = EnvelopeRequestResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 404, description = "Envelope request was not found"),
        (status = 503, description = "Envelope request is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn get_request<P, B>(
    session: Option<Extension<UserEnvelopeSession<B>>>,
    State(state): State<UserEnvelopeState<P>>,
    Path(request_id): Path<Uuid>,
) -> Response
where
    P: EnvelopeRequestBroker<B>,
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    match state.broker.get(&session, request_id).await {
        Ok(Some(request)) => Json(EnvelopeRequestResponse {
            api_version: ENVELOPE_REQUESTS_API_VERSION,
            request,
        })
        .into_response(),
        Ok(None) | Err(EnvelopeRequestBrokerError::NotFound) => {
            StatusCode::NOT_FOUND.into_response()
        }
        Err(error) => broker_error_response(error),
    }
}

#[utoipa::path(
    post,
    path = "/app/api/v1/envelope-requests",
    request_body = CreateEnvelopeRequestBody,
    params(("X-Steward-CSRF" = String, Header)),
    responses(
        (status = 201, body = EnvelopeRequestResponse),
        (status = 400, description = "Request is malformed"),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Origin, fetch metadata, or CSRF proof is invalid"),
        (status = 409, description = "Template revision or connection state conflicts"),
        (status = 422, description = "Requested envelope exceeds its ceiling"),
        (status = 503, description = "Envelope requests are unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn create_request<P, B>(
    session: Option<Extension<UserEnvelopeSession<B>>>,
    proof: Option<Extension<UserEnvelopeMutationProof>>,
    State(state): State<UserEnvelopeState<P>>,
    Json(body): Json<CreateEnvelopeRequestBody>,
) -> Response
where
    P: EnvelopeRequestBroker<B>,
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if proof.is_none() {
        return StatusCode::FORBIDDEN.into_response();
    }
    if body.idempotency_key.is_empty() || body.idempotency_key.len() > 200 {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let requested_envelope: Envelope = body.requested_envelope.into();
    let templates = match state.broker.templates(&session).await {
        Ok(templates) => templates,
        Err(error) => return broker_error_response(error),
    };
    let Some(template) = templates.iter().find(|template| {
        template.id == body.template_id && template.revision == body.template_revision
    }) else {
        return StatusCode::CONFLICT.into_response();
    };
    if has_tools(&requested_envelope)
        && template.github_connection != ConnectionReadiness::Connected
    {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "apiVersion": ENVELOPE_REQUESTS_API_VERSION,
                "error": "required GitHub connection is not ready",
                "connectionsPath": "/admin/connections",
            })),
        )
            .into_response();
    }
    if requested_envelope.revision != template.revision
        || validate_envelope(&requested_envelope).is_err()
    {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    }
    let request_spec = envelope_as_user_runtime(&requested_envelope, &session.subject);
    let inside_ceiling = matches!(
        evaluate(&request_spec, &template.ceiling),
        Ok(AdmissionDecision::Admit)
    );
    if !inside_ceiling {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    }
    let auto_provision = template
        .auto_provision_threshold
        .as_ref()
        .is_some_and(|threshold| {
            matches!(
                evaluate(&request_spec, threshold),
                Ok(AdmissionDecision::Admit)
            )
        });
    match state
        .broker
        .create(
            &session,
            ValidatedEnvelopeRequest {
                template,
                requested_envelope: &requested_envelope,
                idempotency_key: &body.idempotency_key,
                auto_provision,
            },
        )
        .await
    {
        Ok(request) => (
            StatusCode::CREATED,
            Json(EnvelopeRequestResponse {
                api_version: ENVELOPE_REQUESTS_API_VERSION,
                request,
            }),
        )
            .into_response(),
        Err(error) => broker_error_response(error),
    }
}

#[utoipa::path(
    post,
    path = "/app/api/v1/envelope-requests/{request_id}/github-actions-workflow",
    params(
        ("request_id" = String, Path),
        ("X-Steward-CSRF" = String, Header)
    ),
    request_body = RenderGithubActionsWorkflowBody,
    responses(
        (status = 200, body = GithubActionsWorkflowResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Origin, fetch metadata, or CSRF proof is invalid"),
        (status = 404, description = "Envelope request was not found"),
        (status = 409, description = "Envelope request is not provisioned"),
        (status = 422, description = "Workflow inputs are invalid"),
        (status = 503, description = "Envelope request is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn render_github_actions_for_envelope<P, B>(
    session: Option<Extension<UserEnvelopeSession<B>>>,
    proof: Option<Extension<UserEnvelopeMutationProof>>,
    State(state): State<UserEnvelopeState<P>>,
    Path(request_id): Path<Uuid>,
    Json(body): Json<RenderGithubActionsWorkflowBody>,
) -> Response
where
    P: EnvelopeRequestBroker<B>,
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if proof.is_none() {
        return StatusCode::FORBIDDEN.into_response();
    }
    let request = match state.broker.get(&session, request_id).await {
        Ok(Some(request)) => request,
        Ok(None) | Err(EnvelopeRequestBrokerError::NotFound) => {
            return StatusCode::NOT_FOUND.into_response();
        }
        Err(error) => return broker_error_response(error),
    };
    let Some(envelope) = github_actions_envelope(&request) else {
        return StatusCode::CONFLICT.into_response();
    };
    let reference = match crate::WorkflowReference::parse(&body.workflow) {
        Ok(reference) => reference,
        Err(_) => return StatusCode::UNPROCESSABLE_ENTITY.into_response(),
    };
    let workflow = match state
        .broker
        .workflow(&session, &reference.name, reference.version)
        .await
    {
        Ok(Some(workflow)) => workflow,
        Ok(None) | Err(EnvelopeRequestBrokerError::NotFound) => {
            return StatusCode::NOT_FOUND.into_response();
        }
        Err(error) => return broker_error_response(error),
    };
    let context = VersionedGithubActionsWorkflowContext {
        envelope,
        workflow_name: workflow.name,
        workflow_version: workflow.version,
        workflow_digest: workflow.content_digest,
        reviewed_release: reviewed_steward_run_release_v2(),
    };
    match render_versioned_github_actions_workflow(&body.workflow, &context) {
        Ok(workflow) => Json(GithubActionsWorkflowResponse {
            api_version: ENVELOPE_REQUESTS_API_VERSION,
            workflow,
        })
        .into_response(),
        Err(_) => StatusCode::UNPROCESSABLE_ENTITY.into_response(),
    }
}

fn github_actions_envelope(
    request: &UserEnvelopeRequest,
) -> Option<GithubActionsEnvelopeSelection> {
    if request.status != EnvelopeRequestStatus::Provisioned {
        return None;
    }
    let approved_envelope = request.approved_envelope.as_ref()?;
    let envelope_id = request.envelope_instance_id.as_ref()?;
    let envelope_digest = request.envelope_digest.as_ref()?;
    let revision = u64::try_from(approved_envelope.revision).ok()?;
    Some(GithubActionsEnvelopeSelection {
        id: envelope_id.clone(),
        revision,
        digest: envelope_digest.clone(),
    })
}

fn envelope_as_user_runtime(
    envelope: &Envelope,
    subject: &UserEnvelopeSubject,
) -> AgentRuntimeSpec {
    AgentRuntimeSpec {
        principal: Principal::User {
            acting_user: subject.display_email.clone(),
        },
        owner: subject.display_email.clone(),
        canonical_authority: None,
        agent_type: AgentType {
            name: "user-envelope-request".to_owned(),
        },
        llms: envelope.spec.llms.clone(),
        tools: envelope.spec.tools.clone(),
        budget: envelope.spec.budget.clone(),
        ttl: envelope.spec.ttl.clone(),
        runner: envelope.spec.runner.clone(),
        bindings: None,
    }
}

fn broker_error_response(error: EnvelopeRequestBrokerError) -> Response {
    match error {
        EnvelopeRequestBrokerError::NotFound => StatusCode::NOT_FOUND.into_response(),
        EnvelopeRequestBrokerError::Conflict => StatusCode::CONFLICT.into_response(),
        EnvelopeRequestBrokerError::Unavailable => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use steward_types::{Budget, Duration, ModelRef, ToolGrant};
    use tower::ServiceExt;

    use super::{
        AvailableEnvelopeTemplate, ConnectionReadiness, EnvelopeRequestBroker,
        EnvelopeRequestBrokerError, EnvelopeRequestStatus, UserEnvelopeMutationProof,
        UserEnvelopeRequest, UserEnvelopeSession, UserEnvelopeSubject, ValidatedEnvelopeRequest,
        connection_readiness_for_ceiling, inner_router,
    };
    use crate::BoxFuture;
    use steward_admission::{Envelope, EnvelopeSpec};
    use steward_store::WorkflowRevisionRecord;
    use steward_types::{CanonicalUserId, Email};
    use uuid::Uuid;

    #[derive(Clone, Default)]
    struct TestBroker {
        create_owners: Arc<Mutex<Vec<CanonicalUserId>>>,
    }

    impl EnvelopeRequestBroker<()> for TestBroker {
        fn templates<'a>(
            &'a self,
            _session: &'a UserEnvelopeSession<()>,
        ) -> BoxFuture<'a, Result<Vec<AvailableEnvelopeTemplate>, EnvelopeRequestBrokerError>>
        {
            Box::pin(async { Ok(vec![template()]) })
        }

        fn list<'a>(
            &'a self,
            _session: &'a UserEnvelopeSession<()>,
        ) -> BoxFuture<'a, Result<Vec<UserEnvelopeRequest>, EnvelopeRequestBrokerError>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn get<'a>(
            &'a self,
            _session: &'a UserEnvelopeSession<()>,
            request_id: Uuid,
        ) -> BoxFuture<'a, Result<Option<UserEnvelopeRequest>, EnvelopeRequestBrokerError>>
        {
            let approved_envelope = template()
                .auto_provision_threshold
                .ok_or(EnvelopeRequestBrokerError::Unavailable);
            Box::pin(async move {
                let approved_envelope = approved_envelope?;
                Ok(Some(UserEnvelopeRequest {
                    id: request_id,
                    template_id: "engineer".to_owned(),
                    template_revision: 3,
                    requested_envelope: approved_envelope.clone(),
                    approved_envelope: Some(approved_envelope),
                    status: EnvelopeRequestStatus::Provisioned,
                    approval_id: None,
                    envelope_instance_id: Some("env_local_test".to_owned()),
                    envelope_digest: Some(
                        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                            .to_owned(),
                    ),
                    reason: None,
                    created_at: "2026-08-17T00:00:00Z".to_owned(),
                    status_at: "2026-08-17T00:00:00Z".to_owned(),
                }))
            })
        }

        fn create<'a>(
            &'a self,
            session: &'a UserEnvelopeSession<()>,
            request: ValidatedEnvelopeRequest<'a>,
        ) -> BoxFuture<'a, Result<UserEnvelopeRequest, EnvelopeRequestBrokerError>> {
            let owners = self.create_owners.clone();
            let owner = session.subject.canonical_user_id.clone();
            let template_id = request.template.id.clone();
            let template_revision = request.template.revision;
            let requested_envelope = request.requested_envelope.clone();
            let auto_provision = request.auto_provision;
            let approved_envelope = auto_provision.then(|| requested_envelope.clone());
            let status = if auto_provision {
                EnvelopeRequestStatus::Provisioned
            } else {
                EnvelopeRequestStatus::Pending
            };
            Box::pin(async move {
                owners
                    .lock()
                    .map_err(|_| EnvelopeRequestBrokerError::Unavailable)?
                    .push(owner);
                Ok(UserEnvelopeRequest {
                    id: Uuid::nil(),
                    template_id,
                    template_revision,
                    requested_envelope,
                    approved_envelope,
                    status,
                    approval_id: None,
                    envelope_instance_id: Some("env_local_test".to_owned()),
                    envelope_digest: Some("sha256:local-test".to_owned()),
                    reason: None,
                    created_at: "2026-08-17T00:00:00Z".to_owned(),
                    status_at: "2026-08-17T00:00:00Z".to_owned(),
                })
            })
        }

        fn workflows<'a>(
            &'a self,
            _session: &'a UserEnvelopeSession<()>,
        ) -> BoxFuture<'a, Result<Vec<WorkflowRevisionRecord>, EnvelopeRequestBrokerError>>
        {
            Box::pin(async { Ok(vec![workflow()]) })
        }

        fn workflow<'a>(
            &'a self,
            _session: &'a UserEnvelopeSession<()>,
            name: &'a str,
            version: i64,
        ) -> BoxFuture<'a, Result<Option<WorkflowRevisionRecord>, EnvelopeRequestBrokerError>>
        {
            Box::pin(
                async move { Ok((name == "repository-review" && version == 1).then(workflow)) },
            )
        }
    }

    fn workflow() -> WorkflowRevisionRecord {
        WorkflowRevisionRecord {
            name: "repository-review".to_owned(),
            version: 1,
            display_name: "Repository review".to_owned(),
            agent: "codex@0.117.0".to_owned(),
            prompt: "Review the repository state.".to_owned(),
            content_digest:
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
            published_by: "usr_abcdef0123456789abcdef0123456789".to_owned(),
            published_at: "2026-08-24T00:00:00.000000Z".to_owned(),
        }
    }

    fn template() -> AvailableEnvelopeTemplate {
        let ceiling = Envelope {
            revision: 3,
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
                    monthly_limit: "100.00".to_owned(),
                    single_run_limit: None,
                    currency: "USD".to_owned(),
                },
                ttl: Duration("72h".to_owned()),
                runner: steward_types::RunnerRequirements::default(),
            },
        };
        AvailableEnvelopeTemplate {
            id: "engineer".to_owned(),
            display_name: "Engineer".to_owned(),
            revision: 3,
            auto_provision_threshold: Some(Envelope {
                revision: 3,
                spec: EnvelopeSpec {
                    budget: Budget {
                        monthly_limit: "50.00".to_owned(),
                        single_run_limit: None,
                        currency: "USD".to_owned(),
                    },
                    ttl: Duration("24h".to_owned()),
                    runner: steward_types::RunnerRequirements::default(),
                    ..ceiling.spec.clone()
                },
            }),
            ceiling,
            github_connection: ConnectionReadiness::Connected,
        }
    }

    #[test]
    fn connection_is_not_required_for_a_toolless_envelope() {
        let mut ceiling = template().ceiling;
        ceiling.spec.tools.clear();
        assert_eq!(
            connection_readiness_for_ceiling(&ceiling),
            ConnectionReadiness::Connected,
        );
    }

    #[test]
    fn connection_remains_required_for_any_tool_envelope() {
        let mut ceiling = template().ceiling;
        ceiling.spec.tools[0].provider = "provider-a".to_owned();
        assert_eq!(
            connection_readiness_for_ceiling(&ceiling),
            ConnectionReadiness::Missing,
        );
    }

    fn session() -> Result<UserEnvelopeSession<()>, String> {
        Ok(UserEnvelopeSession {
            subject: UserEnvelopeSubject {
                canonical_user_id: CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?,
                display_email: Email::parse("alice@example.com")?,
                member_roles: Vec::new(),
            },
            binding: (),
        })
    }

    #[tokio::test]
    async fn create_uses_the_authenticated_canonical_owner_and_exact_template_revision()
    -> Result<(), String> {
        let broker = TestBroker::default();
        let app = inner_router(broker.clone());
        let request = Request::builder()
            .method("POST")
            .uri("/app/api/v1/envelope-requests")
            .header("content-type", "application/json")
            .extension(session()?)
            .extension(UserEnvelopeMutationProof)
            .body(Body::from(
                serde_json::json!({
                    "templateId": "engineer",
                    "templateRevision": 3,
                    "requestedEnvelope": template().auto_provision_threshold,
                    "idempotencyKey": "request-1",
                })
                .to_string(),
            ))
            .map_err(|error| format!("build request: {error}"))?;
        let response = app
            .oneshot(request)
            .await
            .map_err(|error| format!("submit request: {error}"))?;
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = to_bytes(response.into_body(), 16 * 1024)
            .await
            .map_err(|error| format!("read response: {error}"))?;
        let response: serde_json::Value =
            serde_json::from_slice(&body).map_err(|error| format!("parse response: {error}"))?;
        assert_eq!(response["request"]["templateRevision"], 3);
        assert_eq!(response["request"]["status"], "provisioned");
        let owners = broker
            .create_owners
            .lock()
            .map_err(|_| "read broker owners".to_owned())?;
        assert_eq!(owners.len(), 1);
        assert_eq!(owners[0].as_str(), "usr_0123456789abcdef0123456789abcdef");
        Ok(())
    }

    #[tokio::test]
    async fn provisioned_envelope_renders_only_the_server_bound_pinned_workflow()
    -> Result<(), String> {
        let app = inner_router(TestBroker::default());
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/app/api/v1/envelope-requests/00000000-0000-0000-0000-000000000001/github-actions-workflow")
                    .header("content-type", "application/json")
                    .extension(session()?)
                    .extension(UserEnvelopeMutationProof)
                    .body(Body::from(
                        serde_json::json!({
                            "workflow": "repository-review@1",
                        })
                        .to_string(),
                    ))
                    .map_err(|error| format!("build render request: {error}"))?,
            )
            .await
            .map_err(|error| format!("render request: {error}"))?;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "a provisioned envelope must yield only server-bound, pinned workflow YAML"
        );
        let body = to_bytes(response.into_body(), 64 * 1024)
            .await
            .map_err(|error| format!("read render response: {error}"))?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("parse render response: {error}"))?;
        let yaml = value["workflow"]["yaml"]
            .as_str()
            .ok_or_else(|| "render response omitted YAML".to_owned())?;
        assert!(yaml.contains("# envelope-id: env_local_test"));
        assert!(yaml.contains("      workflow: repository-review@1"));
        assert!(yaml.contains(
            "uses: apelogic-ai/steward-run/.github/workflows/steward-task-self-hosted.yml@328159f3b816b8c93a9e5a8c1790243d2965aff8"
        ));
        assert!(!yaml.contains("contents: write"));
        assert!(!yaml.contains("coding-agent-runtime"));
        assert!(!yaml.contains("TARGET_REVISION"));
        assert!(!yaml.contains("TARGET_PATH"));
        Ok(())
    }

    #[tokio::test]
    async fn unauthenticated_or_out_of_ceiling_requests_fail_closed() -> Result<(), String> {
        let app = inner_router(TestBroker::default());
        let anonymous = Request::builder()
            .uri("/app/api/v1/envelope-requests")
            .body(Body::empty())
            .map_err(|error| format!("build anonymous request: {error}"))?;
        assert_eq!(
            app.clone()
                .oneshot(anonymous)
                .await
                .map_err(|error| format!("send anonymous request: {error}"))?
                .status(),
            StatusCode::UNAUTHORIZED
        );

        let mut excessive = template().ceiling;
        excessive.spec.budget.monthly_limit = "1000.00".to_owned();
        let request = Request::builder()
            .method("POST")
            .uri("/app/api/v1/envelope-requests")
            .header("content-type", "application/json")
            .extension(session()?)
            .extension(UserEnvelopeMutationProof)
            .body(Body::from(
                serde_json::json!({
                    "templateId": "engineer",
                    "templateRevision": 3,
                    "requestedEnvelope": excessive,
                    "idempotencyKey": "request-over-ceiling",
                })
                .to_string(),
            ))
            .map_err(|error| format!("build excessive request: {error}"))?;
        assert_eq!(
            app.oneshot(request)
                .await
                .map_err(|error| format!("send excessive request: {error}"))?
                .status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        Ok(())
    }

    #[tokio::test]
    async fn missing_automatic_threshold_never_invents_provisioning_authority() -> Result<(), String>
    {
        #[derive(Clone)]
        struct ReviewOnlyBroker;

        impl EnvelopeRequestBroker<()> for ReviewOnlyBroker {
            fn templates<'a>(
                &'a self,
                _session: &'a UserEnvelopeSession<()>,
            ) -> BoxFuture<'a, Result<Vec<AvailableEnvelopeTemplate>, EnvelopeRequestBrokerError>>
            {
                let mut review_only = template();
                review_only.auto_provision_threshold = None;
                Box::pin(async move { Ok(vec![review_only]) })
            }

            fn list<'a>(
                &'a self,
                _session: &'a UserEnvelopeSession<()>,
            ) -> BoxFuture<'a, Result<Vec<UserEnvelopeRequest>, EnvelopeRequestBrokerError>>
            {
                Box::pin(async { Ok(Vec::new()) })
            }

            fn get<'a>(
                &'a self,
                _session: &'a UserEnvelopeSession<()>,
                _request_id: Uuid,
            ) -> BoxFuture<'a, Result<Option<UserEnvelopeRequest>, EnvelopeRequestBrokerError>>
            {
                Box::pin(async { Ok(None) })
            }

            fn create<'a>(
                &'a self,
                _session: &'a UserEnvelopeSession<()>,
                request: ValidatedEnvelopeRequest<'a>,
            ) -> BoxFuture<'a, Result<UserEnvelopeRequest, EnvelopeRequestBrokerError>>
            {
                let requested_envelope = request.requested_envelope.clone();
                Box::pin(async move {
                    Ok(UserEnvelopeRequest {
                        id: Uuid::nil(),
                        template_id: "engineer".to_owned(),
                        template_revision: 3,
                        requested_envelope,
                        approved_envelope: None,
                        status: EnvelopeRequestStatus::Pending,
                        approval_id: None,
                        envelope_instance_id: None,
                        envelope_digest: None,
                        reason: None,
                        created_at: "2026-08-17T00:00:00Z".to_owned(),
                        status_at: "2026-08-17T00:00:00Z".to_owned(),
                    })
                })
            }
        }

        let review_only = template();
        let app = inner_router(ReviewOnlyBroker);
        let request = Request::builder()
            .method("POST")
            .uri("/app/api/v1/envelope-requests")
            .header("content-type", "application/json")
            .extension(session()?)
            .extension(UserEnvelopeMutationProof)
            .body(Body::from(
                serde_json::json!({
                    "templateId": review_only.id,
                    "templateRevision": review_only.revision,
                    "requestedEnvelope": review_only.ceiling,
                    "idempotencyKey": "review-only-request",
                })
                .to_string(),
            ))
            .map_err(|error| format!("build review-only request: {error}"))?;
        let response = app
            .oneshot(request)
            .await
            .map_err(|error| format!("submit review-only request: {error}"))?;
        let body = to_bytes(response.into_body(), 16 * 1024)
            .await
            .map_err(|error| format!("read review-only response: {error}"))?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("parse review-only response: {error}"))?;
        assert_eq!(value["request"]["status"], "pending");
        assert!(value["request"]["approvedEnvelope"].is_null());
        Ok(())
    }
}
