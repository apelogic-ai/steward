//! Browser-session-bound user envelope request API.

use std::hash::Hash;

use axum::extract::{Path, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use steward_admission::{
    AdmissionDecision, Envelope, add_budget_amount, envelope_is_within, validate_envelope,
};
use steward_store::{
    EnvelopeRequestRecord, EnvelopeRequestReservationRequest, EnvelopeRequestStatusEventRecord,
    EnvelopeRequestStatusUpdate, EnvelopeUsageRecord, PgStore, StoreError, WorkflowRevisionRecord,
};
use steward_types::{
    Budget, CanonicalUserId, Duration, Email, ModelRef, RunnerRequirements, ToolGrant,
};
use uuid::Uuid;

use crate::browser_auth::{
    BrowserAuthService, BrowserMutationProof, BrowserSessionBinding, BrowserSessionContext,
    protect_browser_routes,
};
use crate::workflows::SAMPLE_WORKFLOW_NAME;
use crate::{
    AgentRunAvailability, AgentRunDataStatus, BoxFuture, GithubActionsEnvelopeSelection,
    VersionedGithubActionsWorkflowContext, render_versioned_github_actions_workflow,
    reviewed_steward_run_release_v2,
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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize, utoipa::ToSchema)]
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
    pub status_actor: String,
    pub status_template_revision: i64,
    pub created_at: String,
    pub status_at: String,
    pub history: Vec<EnvelopeRequestHistoryEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<EnvelopeUsageView>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct EnvelopeUsagePeriod {
    pub start: String,
    pub end: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct EnvelopeSpendUsage {
    pub observed: String,
    pub limit: String,
    pub currency: String,
    pub observed_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct EnvelopeUsageView {
    pub period: EnvelopeUsagePeriod,
    pub spend: Option<EnvelopeSpendUsage>,
    pub availability: AgentRunDataStatus,
}

#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct EnvelopeRequestHistoryEvent {
    pub status: EnvelopeRequestStatus,
    pub at: String,
    pub actor: String,
    pub reason: Option<String>,
    pub rationale: Option<String>,
    pub evidence_url: Option<String>,
    pub expires_at: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_minutes_limit: Option<String>,
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
                runtime_minutes_limit: envelope.spec.runtime_minutes_limit,
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
                runtime_minutes_limit: envelope.spec.runtime_minutes_limit,
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
    next_cursor: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct EnvelopeRequestsQuery {
    status: Option<EnvelopeRequestStatus>,
    cursor: Option<String>,
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
    sample: bool,
}

impl From<WorkflowRevisionRecord> for PublishedWorkflowOption {
    fn from(record: WorkflowRevisionRecord) -> Self {
        let sample = record.name == SAMPLE_WORKFLOW_NAME;
        Self {
            name: record.name,
            version: record.version,
            display_name: record.display_name,
            agent: record.agent,
            sample,
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
/// exact template revision and decided whether it is within the automatic-provisioning boundary.
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
/// envelope is the hard ceiling and the automatic-provisioning boundary. Requests outside that
/// exact revision remain pending for an explicit administrator decision.
#[derive(Clone)]
pub struct PgEnvelopeRequestBroker {
    store: PgStore,
}

impl PgEnvelopeRequestBroker {
    pub fn new(store: PgStore) -> Self {
        Self { store }
    }

    async fn attach_usage(
        &self,
        mut request: UserEnvelopeRequest,
    ) -> Result<UserEnvelopeRequest, EnvelopeRequestBrokerError> {
        if request.status != EnvelopeRequestStatus::Provisioned {
            return Ok(request);
        }
        let Some(instance_id) = request.envelope_instance_id.as_deref() else {
            return Err(EnvelopeRequestBrokerError::Unavailable);
        };
        let usage = self
            .store
            .envelope_usage(instance_id)
            .await
            .map_err(map_store_broker_error)?;
        let authority = request
            .approved_envelope
            .as_ref()
            .unwrap_or(&request.requested_envelope);
        request.usage = Some(envelope_usage_view(usage, authority));
        Ok(request)
    }
}

impl EnvelopeRequestBroker<BrowserSessionBinding> for PgEnvelopeRequestBroker {
    fn templates<'a>(
        &'a self,
        session: &'a UserEnvelopeSession<BrowserSessionBinding>,
    ) -> BoxFuture<'a, Result<Vec<AvailableEnvelopeTemplate>, EnvelopeRequestBrokerError>> {
        Box::pin(async move {
            self.store
                .available_envelope_templates(&session.subject.member_roles)
                .await
                .map(|templates| {
                    templates
                        .into_iter()
                        .map(|template| AvailableEnvelopeTemplate {
                            id: template.template_id,
                            display_name: template.display_name,
                            revision: template.ceiling.revision,
                            ceiling: template.ceiling,
                            auto_provision_threshold: template.auto_provision_threshold,
                        })
                        .collect()
                })
                .map_err(map_store_broker_error)
        })
    }

    fn template<'a>(
        &'a self,
        session: &'a UserEnvelopeSession<BrowserSessionBinding>,
        template_id: &'a str,
        revision: i64,
    ) -> BoxFuture<'a, Result<Option<AvailableEnvelopeTemplate>, EnvelopeRequestBrokerError>> {
        Box::pin(async move {
            let Some(template) = self
                .store
                .latest_envelope_template(template_id)
                .await
                .map_err(map_store_broker_error)?
            else {
                return Ok(None);
            };
            if template.ceiling.revision != revision
                || !template
                    .member_roles
                    .iter()
                    .any(|role| session.subject.member_roles.contains(role))
            {
                return Ok(None);
            }
            Ok(Some(AvailableEnvelopeTemplate {
                id: template.template_id,
                display_name: template.display_name,
                revision,
                ceiling: template.ceiling,
                auto_provision_threshold: template.auto_provision_threshold,
            }))
        })
    }

    fn list<'a>(
        &'a self,
        session: &'a UserEnvelopeSession<BrowserSessionBinding>,
    ) -> BoxFuture<'a, Result<Vec<UserEnvelopeRequest>, EnvelopeRequestBrokerError>> {
        Box::pin(async move {
            let records = self
                .store
                .envelope_requests(&session.subject.canonical_user_id)
                .await
                .map_err(map_store_broker_error)?;
            let mut requests = Vec::with_capacity(records.len());
            for record in records {
                requests.push(self.attach_usage(user_envelope_request(record)).await?);
            }
            Ok(requests)
        })
    }

    fn get<'a>(
        &'a self,
        session: &'a UserEnvelopeSession<BrowserSessionBinding>,
        request_id: Uuid,
    ) -> BoxFuture<'a, Result<Option<UserEnvelopeRequest>, EnvelopeRequestBrokerError>> {
        Box::pin(async move {
            let record = self
                .store
                .envelope_request(&session.subject.canonical_user_id, request_id)
                .await
                .map_err(map_store_broker_error)?;
            let Some(record) = record else {
                return Ok(None);
            };
            let history = self
                .store
                .envelope_request_history(request_id)
                .await
                .map_err(map_store_broker_error)?;
            let mut request = user_envelope_request(record);
            request.history = history
                .into_iter()
                .map(envelope_request_history_event)
                .collect();
            Ok(Some(self.attach_usage(request).await?))
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
                    actor: session.subject.canonical_user_id.as_str(),
                })
                .await
                .map_err(map_store_broker_error)?;
            if !request.auto_provision {
                return Ok(user_envelope_request(reservation.record));
            }
            let instance_id = envelope_instance_id(reservation.record.id);
            let digest = envelope_content_digest(request.requested_envelope)
                .map_err(|_| EnvelopeRequestBrokerError::Unavailable)?;
            let provisioned = self
                .store
                .append_envelope_request_status(
                    reservation.record.id,
                    EnvelopeRequestStatusUpdate {
                        from: steward_store::EnvelopeRequestStatus::Pending,
                        to: steward_store::EnvelopeRequestStatus::Provisioned,
                        approval_id: None,
                        envelope_instance_id: Some(&instance_id),
                        envelope_digest: Some(&digest),
                        reason: None,
                        rationale: None,
                        evidence_url: None,
                        expires_at: None,
                        approved_envelope: Some(request.requested_envelope),
                        actor: "system:auto",
                    },
                )
                .await
                .map_err(map_store_broker_error)?;
            self.attach_usage(user_envelope_request(provisioned)).await
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
        status_actor: record.status_actor,
        status_template_revision: record.status_template_revision,
        created_at: record.created_at,
        status_at: record.status_at,
        history: Vec::new(),
        usage: None,
    }
}

fn envelope_usage_view(usage: EnvelopeUsageRecord, authority: &Envelope) -> EnvelopeUsageView {
    let expected_currency = authority.spec.budget.currency.clone();
    let effective_limit = usage.active_top_up_amount.as_deref().map_or_else(
        || Some(authority.spec.budget.monthly_limit.clone()),
        |amount| add_budget_amount(&authority.spec.budget.monthly_limit, amount).ok(),
    );
    let currency_valid = usage.currency_count <= 1
        && usage
            .currency
            .as_deref()
            .is_none_or(|currency| currency == expected_currency);
    let availability = if effective_limit.is_none() {
        AgentRunDataStatus {
            availability: AgentRunAvailability::Unavailable,
            source: "envelope_instance_grants".to_owned(),
            observed_at: usage.observed_at.clone(),
            reason: Some("active spend grant is invalid".to_owned()),
        }
    } else if !currency_valid {
        AgentRunDataStatus {
            availability: AgentRunAvailability::Unavailable,
            source: "spend_observations".to_owned(),
            observed_at: usage.observed_at.clone(),
            reason: Some("inconsistent spend currencies".to_owned()),
        }
    } else if usage.observed_runtime_count == 0 {
        AgentRunDataStatus {
            availability: AgentRunAvailability::Unavailable,
            source: "spend_observations".to_owned(),
            observed_at: None,
            reason: Some("no current-period spend observation is available".to_owned()),
        }
    } else if usage.observed_runtime_count < usage.runtime_count {
        AgentRunDataStatus {
            availability: AgentRunAvailability::Partial,
            source: "spend_observations".to_owned(),
            observed_at: usage.observed_at.clone(),
            reason: Some(
                "one or more bound runtimes have no current-period observation".to_owned(),
            ),
        }
    } else {
        AgentRunDataStatus {
            availability: AgentRunAvailability::Available,
            source: "spend_observations".to_owned(),
            observed_at: usage.observed_at.clone(),
            reason: None,
        }
    };
    let spend = currency_valid
        .then(|| {
            Some(EnvelopeSpendUsage {
                observed: usage.observed_amount?,
                limit: effective_limit?,
                currency: expected_currency,
                observed_at: usage.observed_at?,
            })
        })
        .flatten();
    EnvelopeUsageView {
        period: EnvelopeUsagePeriod {
            start: usage.period_start,
            end: usage.period_end,
        },
        spend,
        availability,
    }
}

fn envelope_request_history_event(
    event: EnvelopeRequestStatusEventRecord,
) -> EnvelopeRequestHistoryEvent {
    EnvelopeRequestHistoryEvent {
        status: match event.status {
            steward_store::EnvelopeRequestStatus::Pending => EnvelopeRequestStatus::Pending,
            steward_store::EnvelopeRequestStatus::Approved => EnvelopeRequestStatus::Approved,
            steward_store::EnvelopeRequestStatus::Rejected => EnvelopeRequestStatus::Rejected,
            steward_store::EnvelopeRequestStatus::Provisioned => EnvelopeRequestStatus::Provisioned,
            steward_store::EnvelopeRequestStatus::Stale => EnvelopeRequestStatus::Stale,
            steward_store::EnvelopeRequestStatus::Conflict => EnvelopeRequestStatus::Conflict,
        },
        at: event.at,
        actor: event.actor,
        reason: event.reason,
        rationale: event.rationale,
        evidence_url: event.evidence_url,
        expires_at: event.expires_at,
    }
}

fn map_store_broker_error(error: StoreError) -> EnvelopeRequestBrokerError {
    match error {
        StoreError::EnvelopeRequestNotFound => EnvelopeRequestBrokerError::NotFound,
        StoreError::EnvelopeRequestTemplateStale => EnvelopeRequestBrokerError::Conflict,
        StoreError::EnvelopeRequestIdempotencyConflict
        | StoreError::InvalidEnvelopeRequest
        | StoreError::InvalidEnvelopeRequestTransition => EnvelopeRequestBrokerError::Conflict,
        _ => EnvelopeRequestBrokerError::Unavailable,
    }
}

pub(crate) fn envelope_instance_id(request_id: Uuid) -> String {
    format!("env_{}", request_id.simple())
}

pub(crate) fn envelope_content_digest(envelope: &Envelope) -> Result<String, serde_json::Error> {
    let bytes = serde_json::to_vec(envelope)?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
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

    fn template<'a>(
        &'a self,
        session: &'a UserEnvelopeSession<B>,
        template_id: &'a str,
        revision: i64,
    ) -> BoxFuture<'a, Result<Option<AvailableEnvelopeTemplate>, EnvelopeRequestBrokerError>> {
        Box::pin(async move {
            self.templates(session).await.map(|templates| {
                templates
                    .into_iter()
                    .find(|template| template.id == template_id && template.revision == revision)
            })
        })
    }

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
    params(EnvelopeRequestsQuery),
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
    Query(query): Query<EnvelopeRequestsQuery>,
) -> Response
where
    P: EnvelopeRequestBroker<B>,
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    match state.broker.list(&session).await {
        Ok(mut requests) => {
            if let Some(status) = query.status {
                requests.retain(|request| request.status == status);
            }
            if let Some(cursor) = query.cursor {
                let Ok(cursor) = Uuid::parse_str(&cursor) else {
                    return StatusCode::UNPROCESSABLE_ENTITY.into_response();
                };
                let Some(position) = requests.iter().position(|request| request.id == cursor)
                else {
                    return StatusCode::UNPROCESSABLE_ENTITY.into_response();
                };
                requests = requests.into_iter().skip(position + 1).collect();
            }
            let next_cursor = (requests.len() > 50).then(|| requests[49].id.to_string());
            requests.truncate(50);
            Json(EnvelopeRequestsResponse {
                api_version: ENVELOPE_REQUESTS_API_VERSION,
                requests,
                next_cursor,
            })
            .into_response()
        }
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
        (status = 409, description = "Template revision conflicts"),
        (status = 422, description = "Requested envelope is invalid"),
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
    let template = match state
        .broker
        .template(&session, &body.template_id, body.template_revision)
        .await
    {
        Ok(Some(template)) => template,
        Ok(None) | Err(EnvelopeRequestBrokerError::NotFound) => {
            return StatusCode::CONFLICT.into_response();
        }
        Err(error) => return broker_error_response(error),
    };
    if requested_envelope.revision != template.revision
        || validate_envelope(&requested_envelope).is_err()
    {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    }
    let auto_provision = matches!(
        envelope_is_within(&requested_envelope, &template.ceiling),
        Ok(AdmissionDecision::Admit)
    );
    match state
        .broker
        .create(
            &session,
            ValidatedEnvelopeRequest {
                template: &template,
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
        AvailableEnvelopeTemplate, EnvelopeRequestBroker, EnvelopeRequestBrokerError,
        EnvelopeRequestStatus, PublishedWorkflowOption, UserEnvelopeMutationProof,
        UserEnvelopeRequest, UserEnvelopeSession, UserEnvelopeSubject, ValidatedEnvelopeRequest,
        envelope_usage_view, inner_router,
    };
    use crate::BoxFuture;
    use crate::connections::{
        ConnectionBrokerError, ConnectionPhase, ConnectionSession, ConnectionSubject,
        ProviderConnectionBroker, ProviderConnectionStatus,
    };
    use steward_admission::{Envelope, EnvelopeSpec};
    use steward_store::{EnvelopeUsageRecord, WorkflowRevisionRecord};
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
            Box::pin(async move { Ok(vec![template()]) })
        }

        fn template<'a>(
            &'a self,
            _session: &'a UserEnvelopeSession<()>,
            template_id: &'a str,
            revision: i64,
        ) -> BoxFuture<'a, Result<Option<AvailableEnvelopeTemplate>, EnvelopeRequestBrokerError>>
        {
            Box::pin(async move { Ok((template_id == "engineer" && revision == 3).then(template)) })
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
                    status_actor: "usr_0123456789abcdef0123456789abcdef".to_owned(),
                    status_template_revision: 3,
                    created_at: "2026-08-17T00:00:00Z".to_owned(),
                    status_at: "2026-08-17T00:00:00Z".to_owned(),
                    history: Vec::new(),
                    usage: None,
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
            let status_actor = owner.as_str().to_owned();
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
                    status_actor,
                    status_template_revision: template_revision,
                    created_at: "2026-08-17T00:00:00Z".to_owned(),
                    status_at: "2026-08-17T00:00:00Z".to_owned(),
                    history: Vec::new(),
                    usage: None,
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
            agent: "example-agent@1.0.0".to_owned(),
            prompt: "Review the repository state.".to_owned(),
            content_digest:
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
            published_by: "usr_abcdef0123456789abcdef0123456789".to_owned(),
            published_at: "2026-08-24T00:00:00.000000Z".to_owned(),
        }
    }

    #[test]
    fn reserved_repository_summary_is_advertised_as_the_sample_workflow() {
        let option = PublishedWorkflowOption::from(WorkflowRevisionRecord {
            name: "repo-summary".to_owned(),
            version: 1,
            display_name: "Repository summary".to_owned(),
            agent: "example-agent@1.0.0".to_owned(),
            prompt: "Summarize the repository.".to_owned(),
            content_digest:
                "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc".to_owned(),
            published_by: "system:sample-workflow".to_owned(),
            published_at: "2026-09-24T00:00:00.000000Z".to_owned(),
        });

        assert!(option.sample);
        assert_eq!(option.name, "repo-summary");
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
                runtime_minutes_limit: Some("120".to_owned()),
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
        }
    }

    #[test]
    fn envelope_template_contract_contains_authority_without_connection_state() -> Result<(), String>
    {
        let value = serde_json::to_value(template())
            .map_err(|error| format!("serialize template: {error}"))?;
        assert!(value.get("ceiling").is_some());
        assert!(value.get("githubConnection").is_none());
        Ok(())
    }

    #[test]
    fn envelope_usage_limit_includes_active_instance_top_ups() -> Result<(), String> {
        let authority = template()
            .auto_provision_threshold
            .ok_or_else(|| "missing approved envelope".to_owned())?;
        let usage = envelope_usage_view(
            EnvelopeUsageRecord {
                period_start: "2026-09-01T00:00:00.000000Z".to_owned(),
                period_end: "2026-10-01T00:00:00.000000Z".to_owned(),
                runtime_count: 1,
                observed_runtime_count: 1,
                observed_amount: Some("51.00".to_owned()),
                currency: Some("USD".to_owned()),
                currency_count: 1,
                observed_at: Some("2026-09-24T00:00:00.000000Z".to_owned()),
                active_top_up_amount: Some("25.00".to_owned()),
            },
            &authority,
        );
        assert_eq!(
            usage.spend.map(|spend| spend.limit),
            Some("75.00".to_owned())
        );
        assert_eq!(
            usage.availability.availability,
            crate::AgentRunAvailability::Available
        );
        Ok(())
    }

    #[test]
    fn tool_provider_does_not_select_a_credential_mode_at_the_envelope_boundary()
    -> Result<(), String> {
        let mut template = template();
        template.ceiling.spec.tools[0].provider = "provider-a".to_owned();
        let value = serde_json::to_value(template)
            .map_err(|error| format!("serialize template: {error}"))?;
        assert!(
            value["ceiling"]["spec"]["tools"][0]
                .get("provider")
                .is_some()
        );
        assert!(value.get("githubConnection").is_none());
        Ok(())
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
    async fn unauthenticated_requests_fail_closed_and_out_of_template_requests_remain_pending()
    -> Result<(), String> {
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
        let response = app
            .oneshot(request)
            .await
            .map_err(|error| format!("send excessive request: {error}"))?;
        assert_eq!(
            response.status(),
            StatusCode::CREATED,
            "a valid over-template request must enter review rather than disappearing"
        );
        let body = to_bytes(response.into_body(), 16 * 1024)
            .await
            .map_err(|error| format!("read excessive response: {error}"))?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("parse excessive response: {error}"))?;
        assert_eq!(value["request"]["status"], "pending");
        assert!(value["request"]["approvedEnvelope"].is_null());
        Ok(())
    }

    #[tokio::test]
    async fn the_exact_approved_template_is_the_automatic_provisioning_limit() -> Result<(), String>
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
                let auto_provision = request.auto_provision;
                Box::pin(async move {
                    let approved_envelope = auto_provision.then(|| requested_envelope.clone());
                    Ok(UserEnvelopeRequest {
                        id: Uuid::nil(),
                        template_id: "engineer".to_owned(),
                        template_revision: 3,
                        requested_envelope,
                        approved_envelope,
                        status: if auto_provision {
                            EnvelopeRequestStatus::Provisioned
                        } else {
                            EnvelopeRequestStatus::Pending
                        },
                        approval_id: None,
                        envelope_instance_id: None,
                        envelope_digest: None,
                        reason: None,
                        status_actor: "usr_0123456789abcdef0123456789abcdef".to_owned(),
                        status_template_revision: 3,
                        created_at: "2026-08-17T00:00:00Z".to_owned(),
                        status_at: "2026-08-17T00:00:00Z".to_owned(),
                        history: Vec::new(),
                        usage: None,
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
        assert_eq!(
            value["request"]["status"], "provisioned",
            "an independently approved template revision must not require a second hidden threshold"
        );
        assert_eq!(
            value["request"]["approvedEnvelope"],
            serde_json::to_value(review_only.ceiling)
                .map_err(|error| format!("serialize expected approved envelope: {error}"))?
        );
        Ok(())
    }

    #[tokio::test]
    async fn runtime_minutes_above_the_template_ceiling_enter_review() -> Result<(), String> {
        let broker = TestBroker::default();
        let mut requested = template().ceiling;
        requested.spec.runtime_minutes_limit = Some("180".to_owned());
        let response = inner_router(broker)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/app/api/v1/envelope-requests")
                    .header("content-type", "application/json")
                    .extension(session()?)
                    .extension(UserEnvelopeMutationProof)
                    .body(Body::from(
                        serde_json::json!({
                            "templateId": "engineer",
                            "templateRevision": 3,
                            "requestedEnvelope": requested,
                            "idempotencyKey": "runtime-minutes-review",
                        })
                        .to_string(),
                    ))
                    .map_err(|error| format!("build request: {error}"))?,
            )
            .await
            .map_err(|error| format!("submit request: {error}"))?;
        let body = to_bytes(response.into_body(), 16 * 1024)
            .await
            .map_err(|error| format!("read response: {error}"))?;
        let value: serde_json::Value =
            serde_json::from_slice(&body).map_err(|error| format!("parse response: {error}"))?;
        assert_eq!(value["request"]["status"], "pending");
        Ok(())
    }

    #[derive(Clone)]
    struct PhaseBroker {
        result: Arc<Mutex<Result<ConnectionPhase, ConnectionBrokerError>>>,
    }

    impl PhaseBroker {
        fn with_phase(phase: ConnectionPhase) -> Self {
            Self {
                result: Arc::new(Mutex::new(Ok(phase))),
            }
        }

        fn set(
            &self,
            result: Result<ConnectionPhase, ConnectionBrokerError>,
        ) -> Result<(), String> {
            *self
                .result
                .lock()
                .map_err(|_| "lock phase broker".to_owned())? = result;
            Ok(())
        }
    }

    impl ProviderConnectionBroker<()> for PhaseBroker {
        fn status<'a>(
            &'a self,
            _session: &'a ConnectionSession<()>,
        ) -> BoxFuture<'a, Result<ProviderConnectionStatus, ConnectionBrokerError>> {
            let result = self
                .result
                .lock()
                .map(|result| *result)
                .unwrap_or(Err(ConnectionBrokerError::Unavailable));
            Box::pin(async move {
                let phase = result?;
                Ok(ProviderConnectionStatus {
                    phase,
                    account_email: None,
                    scopes_required: vec!["repo".to_owned()],
                    scopes_granted: Vec::new(),
                    scopes_missing: vec!["repo".to_owned()],
                    expires_at: None,
                    active_credential_expires_at: None,
                    renewal_credential_expires_at: None,
                })
            })
        }

        fn start<'a>(
            &'a self,
            _session: &'a ConnectionSession<()>,
        ) -> BoxFuture<'a, Result<crate::connections::StartedConnection, ConnectionBrokerError>>
        {
            Box::pin(async { Err(ConnectionBrokerError::Unavailable) })
        }

        fn disconnect<'a>(
            &'a self,
            _session: &'a ConnectionSession<()>,
        ) -> BoxFuture<'a, Result<(), ConnectionBrokerError>> {
            Box::pin(async { Err(ConnectionBrokerError::Unavailable) })
        }
    }

    fn create_request_for(
        requested_envelope: Envelope,
        idempotency_key: &str,
    ) -> Result<Request<Body>, String> {
        Request::builder()
            .method("POST")
            .uri("/app/api/v1/envelope-requests")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "templateId": "engineer",
                    "templateRevision": 3,
                    "requestedEnvelope": requested_envelope,
                    "idempotencyKey": idempotency_key,
                })
                .to_string(),
            ))
            .map_err(|error| format!("build envelope request: {error}"))
    }

    #[tokio::test]
    async fn envelope_issuance_ignores_live_connection_changes() -> Result<(), String> {
        let connections = PhaseBroker::with_phase(ConnectionPhase::Connected);
        let broker = TestBroker::default();
        let user_session = session()?;
        let connection_session = ConnectionSession {
            subject: ConnectionSubject {
                canonical_user_id: user_session.subject.canonical_user_id.clone(),
                display_email: user_session.subject.display_email.as_str().to_owned(),
            },
            binding: (),
        };
        let app = inner_router(broker.clone())
            .merge(crate::connections::test_router(connections.clone()))
            .layer(axum::Extension(user_session))
            .layer(axum::Extension(connection_session))
            .layer(axum::Extension(UserEnvelopeMutationProof));

        let loaded = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/app/api/v1/envelope-templates")
                    .body(Body::empty())
                    .map_err(|error| format!("build template load: {error}"))?,
            )
            .await
            .map_err(|error| format!("load connected templates: {error}"))?;
        assert_eq!(loaded.status(), StatusCode::OK);

        for (phase, key) in [
            (ConnectionPhase::Disconnected, "after-disconnect"),
            (ConnectionPhase::ReauthRequired, "after-reauth"),
        ] {
            connections.set(Ok(phase))?;
            let response = app
                .clone()
                .oneshot(create_request_for(
                    template()
                        .auto_provision_threshold
                        .ok_or_else(|| "missing requested envelope".to_owned())?,
                    key,
                )?)
                .await
                .map_err(|error| format!("submit after readiness change: {error}"))?;
            assert_eq!(
                response.status(),
                StatusCode::CREATED,
                "Envelope issuance evaluates authority without live connection readiness"
            );
        }
        assert_eq!(
            broker
                .create_owners
                .lock()
                .map_err(|_| "read created owners".to_owned())?
                .len(),
            2
        );
        Ok(())
    }

    #[tokio::test]
    async fn tool_free_selection_is_admitted_by_the_authority_ceiling() -> Result<(), String> {
        let broker = TestBroker::default();
        let mut requested = template()
            .auto_provision_threshold
            .ok_or_else(|| "missing requested envelope".to_owned())?;
        requested.spec.tools.clear();
        let response = inner_router(broker)
            .layer(axum::Extension(session()?))
            .layer(axum::Extension(UserEnvelopeMutationProof))
            .oneshot(create_request_for(requested, "tool-free")?)
            .await
            .map_err(|error| format!("submit tool-free request: {error}"))?;
        assert_eq!(response.status(), StatusCode::CREATED);
        Ok(())
    }

    #[tokio::test]
    async fn connection_outage_does_not_block_envelope_discovery_or_issuance() -> Result<(), String>
    {
        let connections = PhaseBroker::with_phase(ConnectionPhase::Connected);
        connections.set(Err(ConnectionBrokerError::Unavailable))?;
        let user_session = session()?;
        let connection_session = ConnectionSession {
            subject: ConnectionSubject {
                canonical_user_id: user_session.subject.canonical_user_id.clone(),
                display_email: user_session.subject.display_email.as_str().to_owned(),
            },
            binding: (),
        };
        let app = inner_router(TestBroker::default())
            .merge(crate::connections::test_router(connections))
            .layer(axum::Extension(user_session))
            .layer(axum::Extension(connection_session))
            .layer(axum::Extension(UserEnvelopeMutationProof));

        let templates = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/app/api/v1/envelope-templates")
                    .body(Body::empty())
                    .map_err(|error| format!("build unavailable template request: {error}"))?,
            )
            .await
            .map_err(|error| format!("request unavailable templates: {error}"))?;
        assert_eq!(
            templates.status(),
            StatusCode::OK,
            "unused Envelope capacity must not query MCP-GW"
        );

        let mut tool_free = template()
            .auto_provision_threshold
            .ok_or_else(|| "missing requested envelope".to_owned())?;
        tool_free.spec.tools.clear();
        let tool_free_create = app
            .clone()
            .oneshot(create_request_for(tool_free, "tool-free-during-outage")?)
            .await
            .map_err(|error| {
                format!("submit tool-free request during connection outage: {error}")
            })?;
        assert_eq!(
            tool_free_create.status(),
            StatusCode::CREATED,
            "tool-free authority must not query the unavailable GitHub integration"
        );

        let create = app
            .oneshot(create_request_for(
                template()
                    .auto_provision_threshold
                    .ok_or_else(|| "missing requested envelope".to_owned())?,
                "unavailable",
            )?)
            .await
            .map_err(|error| format!("submit during connection outage: {error}"))?;
        assert_eq!(
            create.status(),
            StatusCode::CREATED,
            "tool-bearing Envelope issuance still evaluates authority only"
        );
        Ok(())
    }

    #[tokio::test]
    async fn connection_and_template_endpoints_are_lifecycle_independent() -> Result<(), String> {
        let phase = PhaseBroker::with_phase(ConnectionPhase::Disconnected);
        let user_session = session()?;
        let connection_session = ConnectionSession {
            subject: ConnectionSubject {
                canonical_user_id: user_session.subject.canonical_user_id.clone(),
                display_email: user_session.subject.display_email.as_str().to_owned(),
            },
            binding: (),
        };
        let app = inner_router(TestBroker::default())
            .merge(crate::connections::test_router(phase))
            .layer(axum::Extension(user_session))
            .layer(axum::Extension(connection_session));

        let connection = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/api/v1/connections/github")
                    .body(Body::empty())
                    .map_err(|error| format!("build connection status request: {error}"))?,
            )
            .await
            .map_err(|error| format!("request connection status: {error}"))?;
        let connection_body = to_bytes(connection.into_body(), 16 * 1024)
            .await
            .map_err(|error| format!("read connection status: {error}"))?;
        let connection: serde_json::Value = serde_json::from_slice(&connection_body)
            .map_err(|error| format!("parse connection status: {error}"))?;
        assert_eq!(connection["status"]["phase"], "disconnected");

        let templates = app
            .oneshot(
                Request::builder()
                    .uri("/app/api/v1/envelope-templates")
                    .body(Body::empty())
                    .map_err(|error| format!("build templates request: {error}"))?,
            )
            .await
            .map_err(|error| format!("request templates: {error}"))?;
        let templates_body = to_bytes(templates.into_body(), 16 * 1024)
            .await
            .map_err(|error| format!("read templates response: {error}"))?;
        let templates: serde_json::Value = serde_json::from_slice(&templates_body)
            .map_err(|error| format!("parse templates response: {error}"))?;
        assert_eq!(templates["templates"].as_array().map(Vec::len), Some(1));
        assert!(
            templates["templates"][0].get("githubConnection").is_none(),
            "Envelope discovery must not project live connection state"
        );
        Ok(())
    }
}
