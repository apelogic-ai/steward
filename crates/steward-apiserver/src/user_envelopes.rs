//! Browser-session-bound user envelope request API.

use std::collections::BTreeMap;
use std::hash::Hash;

use axum::extract::{Path, Query, Request, State};
use axum::http::{StatusCode, header};
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
    CanonicalUserRecord, EnvelopeRequestRecord, EnvelopeRequestReservationRequest,
    EnvelopeRequestStatusEventRecord, EnvelopeRequestStatusUpdate, EnvelopeTemplateRevisionRecord,
    EnvelopeUsageRecord, PgStore, StoreError, WorkflowRevisionRecord,
};
use steward_types::direct_package::{
    DIRECT_TASK_CONTRACT_VERSION, DiagnosticsRequest, DirectAdmissionDelta, ExecutionLogMode,
    InvocationManifest, PackageCommit, PackageReference, RelativePath, RepositoryUrl,
    WorkspaceAuthority,
};
use steward_types::{
    Budget, CanonicalUserId, Duration, Email, ModelRef, RunnerRequirements, ToolGrant,
};
use uuid::Uuid;

use crate::browser_admin::CapabilityCatalog;
use crate::browser_auth::{
    BrowserAuthService, BrowserMutationProof, BrowserSessionBinding, BrowserSessionContext,
    protect_browser_routes,
};
#[cfg(test)]
use crate::tasks::TaskApiConfig;
use crate::tasks::WorkspaceDeploymentCapabilities;
use crate::{
    AgentRunAvailability, AgentRunDataStatus, BoxFuture, DirectPackageGithubActionsWorkflowContext,
    ExecutionBindingAdvertisement, GithubActionsEnvelopeSelection, StewardRunRelease,
    StewardRunWorkflowInstallationMode, VersionedGithubActionsWorkflowContext,
    render_direct_package_github_actions_workflow, render_versioned_github_actions_workflow,
    steward_run_supports_package_path_invocation,
};

pub const ENVELOPE_REQUESTS_API_VERSION: &str = "steward.envelope-requests/v1";
pub const USER_CAPABILITIES_API_VERSION: &str = "steward.user-capabilities/v1";
pub const MAX_CUSTOM_ENVELOPE_SAFETY_CEILING_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserEnvelopeSubject {
    pub canonical_user_id: CanonicalUserId,
    pub display_email: Email,
    pub member_roles: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UserCapabilitiesResponse {
    pub api_version: String,
    pub workspace: WorkspaceDeploymentCapabilities,
    /// Deployment-owned coding agents and their configured model compatibility.
    pub agents: Vec<ExecutionBindingAdvertisement>,
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
    /// Historical compatibility metadata. New template revisions do not carry this field and
    /// request admission is governed only by `ceiling`.
    pub auto_provision_threshold: Option<Envelope>,
    pub allow_inline_browser_tasks: bool,
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
    pub template_id: Option<String>,
    pub template_display_name: Option<String>,
    pub template_revision: Option<i64>,
    #[schema(value_type = BrowserEnvelope)]
    pub requested_envelope: Envelope,
    #[schema(value_type = Option<BrowserEnvelope>)]
    pub template_envelope: Option<Envelope>,
    pub justification: Option<String>,
    #[schema(value_type = Vec<DirectAdmissionDelta>)]
    pub ceiling_exceedances: Vec<steward_admission::AdmissionDelta>,
    #[schema(value_type = Option<BrowserEnvelope>)]
    pub approved_envelope: Option<Envelope>,
    pub status: EnvelopeRequestStatus,
    #[schema(value_type = Option<String>, format = "uuid")]
    pub approval_id: Option<Uuid>,
    pub envelope_instance_id: Option<String>,
    pub envelope_digest: Option<String>,
    pub reason: Option<String>,
    pub status_actor: String,
    pub status_template_revision: Option<i64>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor_display_email: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceAuthority>,
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
                workspace: envelope.spec.workspace,
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
                workspace: envelope.spec.workspace,
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
    limit: Option<usize>,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EnvelopeRequestResponse {
    api_version: &'static str,
    request: UserEnvelopeRequest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum EnvelopeRequestField {
    RequestedEnvelope,
    Justification,
    Rationale,
    EvidenceUrl,
    ExpiresAt,
    BudgetMonthlyLimit,
    BudgetSingleRunLimit,
    RuntimeMinutesLimit,
    Ttl,
    Llms,
    Tools,
    RunnerPlatforms,
    RunnerMemory,
    RunnerCompute,
    RunnerStorage,
    Workspace,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EnvelopeRequestErrorResponse {
    api_version: &'static str,
    error: &'static str,
    failure_reason: String,
    fields: Vec<EnvelopeRequestField>,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GithubActionsWorkflowResponse {
    api_version: &'static str,
    workflow: crate::GeneratedGithubActionsWorkflow,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RepositoryBundleResponse {
    api_version: &'static str,
    files: BTreeMap<String, String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CreateEnvelopeRequestBody {
    #[serde(default)]
    template_id: Option<String>,
    #[serde(default)]
    template_revision: Option<i64>,
    requested_envelope: BrowserEnvelope,
    idempotency_key: String,
    #[serde(default)]
    justification: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RenderGithubActionsWorkflowBody {
    workflow: String,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RenderRepositoryBundleBody {
    repository: RepositoryUrl,
    package_path: RelativePath,
    #[serde(default)]
    invocation_path: Option<RelativePath>,
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
        let sample = crate::workflows::is_onboarding_sample_workflow(&record);
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
    /// Exact logical agent references from the deployment-owned execution catalog.
    agents: Vec<ExecutionBindingAdvertisement>,
    workflows: Vec<PublishedWorkflowOption>,
}

/// Submission passed only after the HTTP boundary has derived its canonical owner, required an
/// exact template revision and decided whether it is within the template's limits.
pub struct ValidatedEnvelopeRequest<'a> {
    pub template: Option<&'a AvailableEnvelopeTemplate>,
    pub requested_envelope: &'a Envelope,
    pub idempotency_key: &'a str,
    pub justification: Option<&'a str>,
    pub ceiling_exceedances: &'a [steward_admission::AdmissionDelta],
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
/// limits are the automatic-provisioning boundary. Requests outside those limits remain pending
/// for an explicit administrator decision.
#[derive(Clone)]
pub struct PgEnvelopeRequestBroker {
    store: PgStore,
    capabilities: CapabilityCatalog,
    steward_run_release: StewardRunRelease,
    workflow_installation_mode: StewardRunWorkflowInstallationMode,
    task_identity_discovery_enabled: bool,
}

trait UserEnvelopeProjectionStore {
    fn canonical_user<'a>(
        &'a self,
        user_id: &'a CanonicalUserId,
    ) -> BoxFuture<'a, Result<Option<CanonicalUserRecord>, StoreError>>;

    fn envelope_template_revision<'a>(
        &'a self,
        template_id: &'a str,
        revision: i64,
    ) -> BoxFuture<'a, Result<Option<EnvelopeTemplateRevisionRecord>, StoreError>>;
}

impl UserEnvelopeProjectionStore for PgStore {
    fn canonical_user<'a>(
        &'a self,
        user_id: &'a CanonicalUserId,
    ) -> BoxFuture<'a, Result<Option<CanonicalUserRecord>, StoreError>> {
        Box::pin(async move { PgStore::canonical_user(self, user_id).await })
    }

    fn envelope_template_revision<'a>(
        &'a self,
        template_id: &'a str,
        revision: i64,
    ) -> BoxFuture<'a, Result<Option<EnvelopeTemplateRevisionRecord>, StoreError>> {
        Box::pin(
            async move { PgStore::envelope_template_revision(self, template_id, revision).await },
        )
    }
}

async fn attach_request_projection<S: UserEnvelopeProjectionStore>(
    store: &S,
    subject: &UserEnvelopeSubject,
    mut request: UserEnvelopeRequest,
) -> Result<UserEnvelopeRequest, EnvelopeRequestBrokerError> {
    if let (Some(template_id), Some(template_revision)) =
        (request.template_id.as_deref(), request.template_revision)
    {
        request.template_display_name = store
            .envelope_template_revision(template_id, template_revision)
            .await
            .map_err(map_store_broker_error)?
            .map(|template| template.display_name);
    }
    if request.history.is_empty() {
        return Ok(request);
    }

    let Some(owner) = store
        .canonical_user(&subject.canonical_user_id)
        .await
        .map_err(map_store_broker_error)?
    else {
        return Ok(request);
    };
    let mut emails = BTreeMap::from([(
        subject.canonical_user_id.as_str().to_owned(),
        Some(subject.display_email.as_str().to_owned()),
    )]);
    for event in &mut request.history {
        if !emails.contains_key(&event.actor) {
            let display_email = match CanonicalUserId::parse(event.actor.clone()) {
                Ok(actor_id) => store
                    .canonical_user(&actor_id)
                    .await
                    .map_err(map_store_broker_error)?
                    .filter(|actor| actor.organization_id == owner.organization_id)
                    .map(|actor| actor.display_email.as_str().to_owned()),
                Err(_) => None,
            };
            emails.insert(event.actor.clone(), display_email);
        }
        event.actor_display_email = emails.get(&event.actor).cloned().flatten();
    }
    Ok(request)
}

impl PgEnvelopeRequestBroker {
    pub fn new(
        store: PgStore,
        capabilities: CapabilityCatalog,
        steward_run_release: StewardRunRelease,
        workflow_installation_mode: StewardRunWorkflowInstallationMode,
        task_identity_discovery_enabled: bool,
    ) -> Self {
        Self {
            store,
            capabilities,
            steward_run_release,
            workflow_installation_mode,
            task_identity_discovery_enabled,
        }
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

    async fn attach_projection(
        &self,
        session: &UserEnvelopeSession<BrowserSessionBinding>,
        request: UserEnvelopeRequest,
    ) -> Result<UserEnvelopeRequest, EnvelopeRequestBrokerError> {
        attach_request_projection(&self.store, &session.subject, request).await
    }
}

impl EnvelopeRequestBroker<BrowserSessionBinding> for PgEnvelopeRequestBroker {
    fn steward_run_release(&self) -> Option<StewardRunRelease> {
        Some(self.steward_run_release.clone())
    }

    fn workflow_installation_mode(&self) -> StewardRunWorkflowInstallationMode {
        self.workflow_installation_mode
    }

    fn task_identity_discovery_enabled(&self) -> bool {
        self.task_identity_discovery_enabled
    }

    fn envelope_capability_violation_fields(
        &self,
        envelope: &Envelope,
    ) -> Vec<EnvelopeRequestField> {
        capability_catalog_violation_fields(&self.capabilities, envelope)
    }

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
                            allow_inline_browser_tasks: template.allow_inline_browser_tasks,
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
                allow_inline_browser_tasks: template.allow_inline_browser_tasks,
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
                let request = self.attach_usage(user_envelope_request(record)).await?;
                requests.push(self.attach_projection(session, request).await?);
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
            let request = self.attach_usage(request).await?;
            Ok(Some(self.attach_projection(session, request).await?))
        })
    }

    fn get_by_idempotency_key<'a>(
        &'a self,
        session: &'a UserEnvelopeSession<BrowserSessionBinding>,
        idempotency_key: &'a str,
    ) -> BoxFuture<'a, Result<Option<UserEnvelopeRequest>, EnvelopeRequestBrokerError>> {
        Box::pin(async move {
            let record = self
                .store
                .envelope_request_by_idempotency_key(
                    &session.subject.canonical_user_id,
                    idempotency_key,
                )
                .await
                .map_err(map_store_broker_error)?;
            match record {
                Some(record) => {
                    let request = self.attach_usage(user_envelope_request(record)).await?;
                    Ok(Some(self.attach_projection(session, request).await?))
                }
                None => Ok(None),
            }
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
                    template_id: request.template.map(|template| template.id.as_str()),
                    template_revision: request.template.map(|template| template.revision),
                    requested_envelope: request.requested_envelope,
                    idempotency_key: request.idempotency_key,
                    justification: request.justification,
                    ceiling_exceedances: request.ceiling_exceedances,
                    actor: session.subject.canonical_user_id.as_str(),
                })
                .await
                .map_err(map_store_broker_error)?;
            if !request.auto_provision {
                return self
                    .attach_projection(session, user_envelope_request(reservation.record))
                    .await;
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
            let request = self
                .attach_usage(user_envelope_request(provisioned))
                .await?;
            self.attach_projection(session, request).await
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

pub(crate) fn user_envelope_request(record: EnvelopeRequestRecord) -> UserEnvelopeRequest {
    UserEnvelopeRequest {
        id: record.id,
        template_id: record.template_id,
        template_display_name: None,
        template_revision: record.template_revision,
        requested_envelope: record.requested_envelope,
        template_envelope: record.template_envelope,
        justification: record.justification,
        ceiling_exceedances: record.ceiling_exceedances,
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
        actor_display_email: None,
        reason: event.reason,
        rationale: event.rationale,
        evidence_url: event.evidence_url,
        expires_at: event.expires_at,
    }
}

fn map_store_broker_error(error: StoreError) -> EnvelopeRequestBrokerError {
    match error {
        StoreError::EnvelopeRequestNotFound => EnvelopeRequestBrokerError::NotFound,
        StoreError::EnvelopeRequestTemplateStale | StoreError::EnvelopeRequestDigestConflict => {
            EnvelopeRequestBrokerError::Conflict
        }
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

pub(crate) fn capability_catalog_violation_fields(
    capabilities: &CapabilityCatalog,
    envelope: &Envelope,
) -> Vec<EnvelopeRequestField> {
    let mut fields = Vec::new();
    if capabilities.models.is_empty()
        || envelope
            .spec
            .llms
            .iter()
            .any(|model| !capabilities.models.contains(model))
    {
        fields.push(EnvelopeRequestField::Llms);
    }
    if envelope.spec.tools.iter().any(|tool| {
        !capabilities
            .tools
            .iter()
            .any(|available| available.grants(tool))
    }) {
        fields.push(EnvelopeRequestField::Tools);
    }
    fields
}

pub(crate) fn envelope_request_error_response(
    status: StatusCode,
    error: &'static str,
    failure_reason: impl Into<String>,
    fields: Vec<EnvelopeRequestField>,
) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(EnvelopeRequestErrorResponse {
            api_version: ENVELOPE_REQUESTS_API_VERSION,
            error,
            failure_reason: failure_reason.into(),
            fields,
        }),
    )
        .into_response()
}

fn envelope_request_field_label(field: EnvelopeRequestField) -> &'static str {
    match field {
        EnvelopeRequestField::RequestedEnvelope => "requested envelope",
        EnvelopeRequestField::Justification => "justification",
        EnvelopeRequestField::Rationale => "rationale",
        EnvelopeRequestField::EvidenceUrl => "evidence URL",
        EnvelopeRequestField::ExpiresAt => "expiry",
        EnvelopeRequestField::BudgetMonthlyLimit => "monthly limit",
        EnvelopeRequestField::BudgetSingleRunLimit => "limit per run",
        EnvelopeRequestField::RuntimeMinutesLimit => "runtime minutes",
        EnvelopeRequestField::Ttl => "time to live",
        EnvelopeRequestField::Llms => "models",
        EnvelopeRequestField::Tools => "tools",
        EnvelopeRequestField::RunnerPlatforms => "runner platforms",
        EnvelopeRequestField::RunnerMemory => "runner memory",
        EnvelopeRequestField::RunnerCompute => "runner compute",
        EnvelopeRequestField::RunnerStorage => "runner storage",
        EnvelopeRequestField::Workspace => "workspace",
    }
}

pub(crate) fn envelope_request_field_list(fields: &[EnvelopeRequestField]) -> String {
    fields
        .iter()
        .map(|field| envelope_request_field_label(*field))
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn workspace_capability_violation_fields(
    capabilities: &WorkspaceDeploymentCapabilities,
    envelope: &Envelope,
) -> Vec<EnvelopeRequestField> {
    let unavailable = envelope.spec.workspace.as_ref().is_some_and(|workspace| {
        workspace.allowed_types.iter().any(|entry_type| {
            let label = match entry_type {
                steward_types::direct_package::WorkspaceEntryType::Git => "git",
                steward_types::direct_package::WorkspaceEntryType::Scratch => "scratch",
            };
            !capabilities
                .enabled_types
                .iter()
                .any(|enabled| enabled == label)
        })
    });
    unavailable
        .then_some(EnvelopeRequestField::Workspace)
        .into_iter()
        .collect()
}

fn validate_capabilities<P, B>(
    broker: &P,
    workspace_capabilities: &WorkspaceDeploymentCapabilities,
    envelope: &Envelope,
) -> Result<(), Box<Response>>
where
    P: EnvelopeRequestBroker<B>,
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    let mut capability_fields = broker.envelope_capability_violation_fields(envelope);
    capability_fields.extend(workspace_capability_violation_fields(
        workspace_capabilities,
        envelope,
    ));
    if !capability_fields.is_empty() {
        return Err(Box::new(envelope_request_error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "capability_not_available",
            format!(
                "This deployment does not provide the requested {}.",
                envelope_request_field_list(&capability_fields)
            ),
            capability_fields,
        )));
    }
    Ok(())
}

/// Server-side request broker. Every method receives the canonical browser subject derived by
/// middleware; no method accepts a browser-supplied owner or provider credential.
pub trait EnvelopeRequestBroker<B>: Clone + Send + Sync + 'static
where
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    fn steward_run_release(&self) -> Option<StewardRunRelease> {
        None
    }

    fn workflow_installation_mode(&self) -> StewardRunWorkflowInstallationMode {
        StewardRunWorkflowInstallationMode::Remote
    }

    fn task_identity_discovery_enabled(&self) -> bool {
        false
    }

    fn envelope_capability_violation_fields(
        &self,
        _envelope: &Envelope,
    ) -> Vec<EnvelopeRequestField> {
        Vec::new()
    }

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

    fn get_by_idempotency_key<'a>(
        &'a self,
        _session: &'a UserEnvelopeSession<B>,
        _idempotency_key: &'a str,
    ) -> BoxFuture<'a, Result<Option<UserEnvelopeRequest>, EnvelopeRequestBrokerError>> {
        Box::pin(async { Ok(None) })
    }

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
    agents: Vec<ExecutionBindingAdvertisement>,
    workspace_capabilities: WorkspaceDeploymentCapabilities,
}

#[cfg(test)]
pub(crate) fn inner_router<P, B>(broker: P) -> Router
where
    P: EnvelopeRequestBroker<B>,
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    inner_router_with_agents(broker, Vec::new())
}

#[cfg(test)]
fn inner_router_with_agents<P, B>(broker: P, agents: Vec<ExecutionBindingAdvertisement>) -> Router
where
    P: EnvelopeRequestBroker<B>,
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    inner_router_with_capabilities(
        broker,
        agents,
        TaskApiConfig::default().workspace_deployment_capabilities(),
    )
}

fn inner_router_with_capabilities<P, B>(
    broker: P,
    agents: Vec<ExecutionBindingAdvertisement>,
    workspace_capabilities: WorkspaceDeploymentCapabilities,
) -> Router
where
    P: EnvelopeRequestBroker<B>,
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    Router::new()
        .route("/app/api/v1/capabilities", get(user_capabilities::<P, B>))
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
        .route(
            "/app/api/v1/envelope-requests/{request_id}/repository-bundle",
            post(render_repository_bundle_for_envelope::<P, B>),
        )
        .route("/app/api/v1/workflows", get(list_workflows::<P, B>))
        .with_state(UserEnvelopeState {
            broker,
            agents,
            workspace_capabilities,
        })
}

#[utoipa::path(
    get,
    operation_id = "getUserCapabilities",
    path = "/app/api/v1/capabilities",
    responses(
        (status = 200, body = UserCapabilitiesResponse),
        (status = 401, description = "Browser session is absent or invalid")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn user_capabilities<P, B>(
    session: Option<Extension<UserEnvelopeSession<B>>>,
    State(state): State<UserEnvelopeState<P>>,
) -> Response
where
    P: EnvelopeRequestBroker<B>,
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    if session.is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(UserCapabilitiesResponse {
        api_version: USER_CAPABILITIES_API_VERSION.to_owned(),
        workspace: state.workspace_capabilities,
        agents: state.agents,
    })
    .into_response()
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
            agents: state.agents,
            workflows: workflows.into_iter().map(Into::into).collect(),
        })
        .into_response(),
        Err(error) => broker_error_response(error),
    }
}

/// Mount user envelope APIs behind the common browser-session, same-origin, CSRF and
/// fetch-metadata boundary. User pages only learn authoritative records returned by their broker.
pub fn protected_router<P>(
    broker: P,
    agents: Vec<ExecutionBindingAdvertisement>,
    workspace_capabilities: WorkspaceDeploymentCapabilities,
    browser_auth: BrowserAuthService,
) -> Router
where
    P: EnvelopeRequestBroker<BrowserSessionBinding>,
{
    let routes = inner_router_with_capabilities(broker, agents, workspace_capabilities)
        .route_layer(middleware::from_fn(adapt_browser_context));
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
        (status = 422, description = "Cursor or limit is invalid"),
        (status = 503, description = "Envelope requests are unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn list_requests<P, B>(
    session: Option<Extension<UserEnvelopeSession<B>>>,
    State(state): State<UserEnvelopeState<P>>,
    query: Result<Query<EnvelopeRequestsQuery>, axum::extract::rejection::QueryRejection>,
) -> Response
where
    P: EnvelopeRequestBroker<B>,
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Ok(Query(query)) = query else {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
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
            let limit = query.limit.unwrap_or(50);
            if !(1..=100).contains(&limit) {
                return StatusCode::UNPROCESSABLE_ENTITY.into_response();
            }
            let next_cursor = (requests.len() > limit).then(|| requests[limit - 1].id.to_string());
            requests.truncate(limit);
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
        (status = 422, body = EnvelopeRequestErrorResponse, description = "Requested envelope, justification, or reviewed capabilities are invalid"),
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
    let justification = match body.justification.as_deref() {
        Some(value) if !value.trim().is_empty() && value.trim().chars().count() <= 2_000 => {
            Some(value.trim())
        }
        Some(_) => {
            return envelope_request_error_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_justification",
                "Justification must contain between 1 and 2,000 characters when supplied.",
                vec![EnvelopeRequestField::Justification],
            );
        }
        None => None,
    };
    let requested_envelope: Envelope = body.requested_envelope.into();
    if validate_envelope(&requested_envelope).is_err() {
        return envelope_request_error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_requested_envelope",
            "The requested envelope is invalid.",
            vec![EnvelopeRequestField::RequestedEnvelope],
        );
    }
    let replayed_request = match state
        .broker
        .get_by_idempotency_key(&session, &body.idempotency_key)
        .await
    {
        Ok(Some(existing))
            if existing.template_id.as_deref() == body.template_id.as_deref()
                && existing.template_revision == body.template_revision
                && existing.requested_envelope == requested_envelope
                && existing.justification.as_deref() == justification =>
        {
            if existing.status != EnvelopeRequestStatus::Pending || existing.template_id.is_none() {
                return (
                    StatusCode::CREATED,
                    Json(EnvelopeRequestResponse {
                        api_version: ENVELOPE_REQUESTS_API_VERSION,
                        request: existing,
                    }),
                )
                    .into_response();
            }
            Some(existing)
        }
        Ok(Some(_)) => return StatusCode::CONFLICT.into_response(),
        Ok(None) => None,
        Err(error) => return broker_error_response(error),
    };
    let template = match (body.template_id.as_deref(), body.template_revision) {
        (None, None) => None,
        (Some(template_id), Some(template_revision)) => {
            match state
                .broker
                .template(&session, template_id, template_revision)
                .await
            {
                Ok(Some(template)) => Some(template),
                Ok(None) | Err(EnvelopeRequestBrokerError::NotFound) => {
                    if let Some(existing) = replayed_request {
                        return (
                            StatusCode::CREATED,
                            Json(EnvelopeRequestResponse {
                                api_version: ENVELOPE_REQUESTS_API_VERSION,
                                request: existing,
                            }),
                        )
                            .into_response();
                    }
                    return StatusCode::CONFLICT.into_response();
                }
                Err(error) => return broker_error_response(error),
            }
        }
        _ => return StatusCode::BAD_REQUEST.into_response(),
    };
    let mut ceiling_exceedances = Vec::new();
    let auto_provision = if let Some(template) = template.as_ref() {
        if requested_envelope.revision != template.revision {
            return envelope_request_error_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                "template_revision_mismatch",
                "The requested envelope revision does not match the selected template revision.",
                vec![EnvelopeRequestField::RequestedEnvelope],
            );
        }
        match envelope_is_within(&requested_envelope, &template.ceiling) {
            Ok(AdmissionDecision::Admit) => true,
            Ok(AdmissionDecision::Reject { deltas }) => {
                ceiling_exceedances = deltas;
                false
            }
            Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        }
    } else {
        false
    };
    if !auto_provision && let Some(existing) = replayed_request {
        return (
            StatusCode::CREATED,
            Json(EnvelopeRequestResponse {
                api_version: ENVELOPE_REQUESTS_API_VERSION,
                request: existing,
            }),
        )
            .into_response();
    }
    if !auto_provision && justification.is_none() {
        return envelope_request_error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "justification_required",
            "Justification is required for requests that need administrator approval.",
            vec![EnvelopeRequestField::Justification],
        );
    }
    if !auto_provision
        && let Err(response) = validate_capabilities(
            &state.broker,
            &state.workspace_capabilities,
            &requested_envelope,
        )
    {
        return *response;
    }
    match state
        .broker
        .create(
            &session,
            ValidatedEnvelopeRequest {
                template: template.as_ref(),
                requested_envelope: &requested_envelope,
                idempotency_key: &body.idempotency_key,
                justification,
                ceiling_exceedances: &ceiling_exceedances,
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
    let Some(reviewed_release) = state.broker.steward_run_release() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let context = VersionedGithubActionsWorkflowContext {
        envelope,
        workflow_name: workflow.name,
        workflow_version: workflow.version,
        workflow_digest: workflow.content_digest,
        reviewed_release,
        workflow_installation_mode: state.broker.workflow_installation_mode(),
        task_identity_discovery_enabled: state.broker.task_identity_discovery_enabled(),
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

#[utoipa::path(
    post,
    path = "/app/api/v1/envelope-requests/{request_id}/repository-bundle",
    params(
        ("request_id" = String, Path),
        ("X-Steward-CSRF" = String, Header)
    ),
    request_body = RenderRepositoryBundleBody,
    responses(
        (status = 200, body = RepositoryBundleResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Origin, fetch metadata, or CSRF proof is invalid"),
        (status = 404, description = "Envelope request was not found"),
        (status = 409, description = "Envelope request is not provisioned"),
        (status = 422, description = "Repository bundle inputs are invalid"),
        (status = 503, description = "Envelope request or release configuration is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn render_repository_bundle_for_envelope<P, B>(
    session: Option<Extension<UserEnvelopeSession<B>>>,
    proof: Option<Extension<UserEnvelopeMutationProof>>,
    State(state): State<UserEnvelopeState<P>>,
    Path(request_id): Path<Uuid>,
    Json(body): Json<RenderRepositoryBundleBody>,
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
    if !body.repository.as_str().starts_with("https://github.com/")
        || body
            .invocation_path
            .as_ref()
            .is_some_and(|path| path == &body.package_path)
    {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
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
    let Some(reviewed_release) = state.broker.steward_run_release() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let invocation_path = match body.invocation_path {
        Some(path) => Some(path),
        None if steward_run_supports_package_path_invocation(&reviewed_release) => None,
        None => match RelativePath::parse(".steward/invocations/browser-task.json") {
            Ok(path) => Some(path),
            Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        },
    };
    let invocation = invocation_path.as_ref().map(|_| InvocationManifest {
        contract_version: DIRECT_TASK_CONTRACT_VERSION.to_owned(),
        package: PackageReference {
            repository: body.repository.clone(),
            commit: PackageCommit::Trigger,
            path: body.package_path.clone(),
        },
        envelope: None,
        diagnostics: Some(DiagnosticsRequest {
            execution_log: ExecutionLogMode::Full,
        }),
    });
    if invocation.as_ref().is_some_and(|invocation| {
        invocation
            .validate_for_invoking_repository(&body.repository)
            .is_err()
    }) {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    }
    let workflow = match render_direct_package_github_actions_workflow(
        &DirectPackageGithubActionsWorkflowContext {
            envelope,
            invocation_path: invocation_path
                .as_ref()
                .map(|path| path.as_str().to_owned()),
            package_path: invocation_path
                .is_none()
                .then(|| body.package_path.as_str().to_owned()),
            execution_log: if invocation_path.is_some() {
                ExecutionLogMode::Off
            } else {
                ExecutionLogMode::Full
            },
            reviewed_release,
            workflow_installation_mode: state.broker.workflow_installation_mode(),
            task_identity_discovery_enabled: state.broker.task_identity_discovery_enabled(),
        },
    ) {
        Ok(workflow) => workflow,
        Err(_) => return StatusCode::UNPROCESSABLE_ENTITY.into_response(),
    };
    let mut files = BTreeMap::new();
    if let (Some(invocation_path), Some(invocation)) = (&invocation_path, invocation) {
        let invocation_json = match serde_json::to_string_pretty(&invocation) {
            Ok(value) => value + "\n",
            Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        };
        files.insert(invocation_path.as_str().to_owned(), invocation_json);
    }
    if files
        .insert(workflow.suggested_path, workflow.yaml)
        .is_some()
    {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    }
    Json(RepositoryBundleResponse {
        api_version: ENVELOPE_REQUESTS_API_VERSION,
        files,
    })
    .into_response()
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
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use steward_types::direct_package::{ResourceQuantity, WorkspaceAuthority, WorkspaceEntryType};
    use steward_types::{
        Budget, Duration, ExecutionModelCompatibility, KubernetesQuantity, ModelRef,
        RunnerPlatform, ToolGrant,
    };
    use tower::ServiceExt;

    use super::{
        AvailableEnvelopeTemplate, EnvelopeRequestBroker, EnvelopeRequestBrokerError,
        EnvelopeRequestField, EnvelopeRequestHistoryEvent, EnvelopeRequestStatus,
        PublishedWorkflowOption, UserEnvelopeMutationProof, UserEnvelopeProjectionStore,
        UserEnvelopeRequest, UserEnvelopeSession, UserEnvelopeSubject, ValidatedEnvelopeRequest,
        attach_request_projection, envelope_usage_view, inner_router, inner_router_with_agents,
        inner_router_with_capabilities,
    };
    use crate::connections::{
        ConnectionBrokerError, ConnectionPhase, ConnectionSession, ConnectionStartOperation,
        ConnectionSubject, ProviderConnectionBroker, ProviderConnectionStatus,
        ReservedConnectionStart,
    };
    use crate::{
        BoxFuture, ExecutionBindingAdvertisement, StewardRunRelease,
        StewardRunWorkflowInstallationMode,
    };
    use steward_admission::{AdmissionDelta, Envelope, EnvelopeSpec};
    use steward_store::{
        CanonicalUserRecord, EnvelopeTemplateRevisionRecord, EnvelopeUsageRecord, StoreError,
        WorkflowRevisionRecord,
    };
    use steward_types::{CanonicalUserId, Email, OrganizationId};
    use uuid::Uuid;

    #[derive(Clone)]
    struct TestBroker {
        create_owners: Arc<Mutex<Vec<CanonicalUserId>>>,
        replayed_request: Arc<Mutex<Option<UserEnvelopeRequest>>>,
        capability_violation_fields: Vec<EnvelopeRequestField>,
        template_override: Option<AvailableEnvelopeTemplate>,
        steward_run_version: String,
        workflow_installation_mode: StewardRunWorkflowInstallationMode,
        task_identity_discovery_enabled: bool,
    }

    impl Default for TestBroker {
        fn default() -> Self {
            Self {
                create_owners: Arc::new(Mutex::new(Vec::new())),
                replayed_request: Arc::new(Mutex::new(None)),
                capability_violation_fields: Vec::new(),
                template_override: None,
                steward_run_version: "0.7.6".to_owned(),
                workflow_installation_mode: StewardRunWorkflowInstallationMode::Remote,
                task_identity_discovery_enabled: false,
            }
        }
    }

    impl EnvelopeRequestBroker<()> for TestBroker {
        fn steward_run_release(&self) -> Option<StewardRunRelease> {
            Some(StewardRunRelease {
                manifest_schema_version: 3,
                version: self.steward_run_version.clone(),
                workflow_repository: "example-org/steward-run".to_owned(),
                workflow_commit: "3333333333333333333333333333333333333333".to_owned(),
                action_commit: "4444444444444444444444444444444444444444".to_owned(),
                governed_job_container_image: Some(format!(
                    "registry.example.test/steward-run@sha256:{}",
                    "a".repeat(64)
                )),
            })
        }

        fn workflow_installation_mode(&self) -> StewardRunWorkflowInstallationMode {
            self.workflow_installation_mode
        }

        fn task_identity_discovery_enabled(&self) -> bool {
            self.task_identity_discovery_enabled
        }

        fn envelope_capability_violation_fields(
            &self,
            _envelope: &Envelope,
        ) -> Vec<EnvelopeRequestField> {
            self.capability_violation_fields.clone()
        }

        fn templates<'a>(
            &'a self,
            _session: &'a UserEnvelopeSession<()>,
        ) -> BoxFuture<'a, Result<Vec<AvailableEnvelopeTemplate>, EnvelopeRequestBrokerError>>
        {
            let template = self.template_override.clone().unwrap_or_else(template);
            Box::pin(async move { Ok(vec![template]) })
        }

        fn template<'a>(
            &'a self,
            _session: &'a UserEnvelopeSession<()>,
            template_id: &'a str,
            revision: i64,
        ) -> BoxFuture<'a, Result<Option<AvailableEnvelopeTemplate>, EnvelopeRequestBrokerError>>
        {
            let template = self.template_override.clone().unwrap_or_else(template);
            Box::pin(
                async move { Ok((template_id == "engineer" && revision == 3).then_some(template)) },
            )
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
                    template_id: Some("engineer".to_owned()),
                    template_display_name: Some("Engineer".to_owned()),
                    template_revision: Some(3),
                    requested_envelope: approved_envelope.clone(),
                    template_envelope: Some(template().ceiling),
                    justification: None,
                    ceiling_exceedances: Vec::new(),
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
                    status_template_revision: Some(3),
                    created_at: "2026-08-17T00:00:00Z".to_owned(),
                    status_at: "2026-08-17T00:00:00Z".to_owned(),
                    history: Vec::new(),
                    usage: None,
                }))
            })
        }

        fn get_by_idempotency_key<'a>(
            &'a self,
            _session: &'a UserEnvelopeSession<()>,
            _idempotency_key: &'a str,
        ) -> BoxFuture<'a, Result<Option<UserEnvelopeRequest>, EnvelopeRequestBrokerError>>
        {
            let replayed_request = self.replayed_request.clone();
            Box::pin(async move {
                replayed_request
                    .lock()
                    .map_err(|_| EnvelopeRequestBrokerError::Unavailable)
                    .map(|request| request.clone())
            })
        }

        fn create<'a>(
            &'a self,
            session: &'a UserEnvelopeSession<()>,
            request: ValidatedEnvelopeRequest<'a>,
        ) -> BoxFuture<'a, Result<UserEnvelopeRequest, EnvelopeRequestBrokerError>> {
            let owners = self.create_owners.clone();
            let replayed_request = self.replayed_request.clone();
            let owner = session.subject.canonical_user_id.clone();
            let status_actor = owner.as_str().to_owned();
            let template_id = request.template.map(|template| template.id.clone());
            let template_display_name = request
                .template
                .map(|template| template.display_name.clone());
            let template_revision = request.template.map(|template| template.revision);
            let requested_envelope = request.requested_envelope.clone();
            let template_envelope = request.template.map(|template| template.ceiling.clone());
            let justification = request.justification.map(str::to_owned);
            let ceiling_exceedances = request.ceiling_exceedances.to_vec();
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
                let id = replayed_request
                    .lock()
                    .map_err(|_| EnvelopeRequestBrokerError::Unavailable)?
                    .as_ref()
                    .map(|request| request.id)
                    .unwrap_or_else(Uuid::nil);
                Ok(UserEnvelopeRequest {
                    id,
                    template_id,
                    template_display_name,
                    template_revision,
                    requested_envelope,
                    template_envelope,
                    justification,
                    ceiling_exceedances,
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
    fn onboarding_sample_metadata_is_explicit_and_version_pinned() -> Result<(), String> {
        let sample_agent = "example-agent@1.0.0";
        let sample = PublishedWorkflowOption::from(WorkflowRevisionRecord {
            name: crate::workflows::SAMPLE_WORKFLOW_NAME.to_owned(),
            version: crate::workflows::SAMPLE_WORKFLOW_VERSION,
            display_name: crate::workflows::SAMPLE_WORKFLOW_DISPLAY_NAME.to_owned(),
            agent: sample_agent.to_owned(),
            prompt: crate::workflows::SAMPLE_WORKFLOW_PROMPT.to_owned(),
            content_digest: crate::workflows::workflow_content_digest(
                sample_agent,
                crate::workflows::SAMPLE_WORKFLOW_PROMPT,
            ),
            published_by: crate::workflows::SAMPLE_WORKFLOW_ACTOR.to_owned(),
            published_at: "2026-09-24T00:00:00.000000Z".to_owned(),
        });
        let impostor = PublishedWorkflowOption::from(WorkflowRevisionRecord {
            name: crate::workflows::SAMPLE_WORKFLOW_NAME.to_owned(),
            version: crate::workflows::SAMPLE_WORKFLOW_VERSION,
            display_name: crate::workflows::SAMPLE_WORKFLOW_DISPLAY_NAME.to_owned(),
            agent: sample_agent.to_owned(),
            prompt: crate::workflows::SAMPLE_WORKFLOW_PROMPT.to_owned(),
            content_digest:
                "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd".to_owned(),
            published_by: "usr_abcdef0123456789abcdef0123456789".to_owned(),
            published_at: "2026-09-24T00:00:00.000000Z".to_owned(),
        });

        let sample = serde_json::to_value(sample)
            .map_err(|error| format!("serialize published Workflow option: {error}"))?;
        let impostor = serde_json::to_value(impostor)
            .map_err(|error| format!("serialize impostor Workflow option: {error}"))?;
        assert_eq!(sample["sample"], true);
        assert_eq!(impostor["sample"], false);
        Ok(())
    }

    #[tokio::test]
    async fn published_workflow_catalog_advertises_deployment_execution_bindings()
    -> Result<(), String> {
        let response = inner_router_with_agents(
            TestBroker::default(),
            vec![ExecutionBindingAdvertisement {
                agent_ref: "codex@0.140.0".to_owned(),
                display_name: Some("Codex 0.140.0".to_owned()),
                models: ExecutionModelCompatibility {
                    providers: vec!["example".to_owned()],
                },
            }],
        )
        .layer(axum::Extension(session()?))
        .oneshot(
            Request::builder()
                .uri("/app/api/v1/workflows")
                .body(Body::empty())
                .map_err(|error| format!("build workflow catalog request: {error}"))?,
        )
        .await
        .map_err(|error| format!("request workflow catalog: {error}"))?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 16 * 1024)
            .await
            .map_err(|error| format!("read workflow catalog response: {error}"))?;
        let response: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("parse workflow catalog response: {error}"))?;
        assert_eq!(response["agents"][0]["agentRef"], "codex@0.140.0");
        assert_eq!(response["agents"][0]["displayName"], "Codex 0.140.0");
        Ok(())
    }

    #[tokio::test]
    async fn deployment_workspace_capabilities_require_a_signed_in_user() -> Result<(), String> {
        let response = inner_router_with_capabilities(
            TestBroker::default(),
            Vec::new(),
            crate::tasks::TaskApiConfig::default().workspace_deployment_capabilities(),
        )
        .oneshot(
            Request::builder()
                .uri("/app/api/v1/capabilities")
                .body(Body::empty())
                .map_err(|error| format!("build unauthenticated capabilities request: {error}"))?,
        )
        .await
        .map_err(|error| format!("request unauthenticated capabilities: {error}"))?;

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        Ok(())
    }

    #[tokio::test]
    async fn signed_in_users_can_read_deployment_capabilities() -> Result<(), String> {
        let workspace = crate::tasks::TaskApiConfig::default()
            .with_workspace_policy_json(Some(
                &serde_json::json!({
                    "enabledTypes": ["git"],
                    "maxTotalSize": "3Gi",
                    "scratchVolumeSize": "2Gi",
                    "maxFiles": 321,
                    "maxHistoryDepth": 42,
                    "maxSubmoduleDepth": 3,
                    "maxSubmodulesPerEntry": 7,
                    "maxTotalSubmodules": 9,
                    "maxTreeReads": 11
                })
                .to_string(),
            ))?
            .workspace_deployment_capabilities();
        let response = inner_router_with_capabilities(
            TestBroker::default(),
            vec![ExecutionBindingAdvertisement {
                agent_ref: "example-agent@1.0.0".to_owned(),
                display_name: Some("Example Agent".to_owned()),
                models: ExecutionModelCompatibility {
                    providers: vec!["example".to_owned()],
                },
            }],
            workspace,
        )
        .layer(axum::Extension(session()?))
        .oneshot(
            Request::builder()
                .uri("/app/api/v1/capabilities")
                .body(Body::empty())
                .map_err(|error| format!("build user capabilities request: {error}"))?,
        )
        .await
        .map_err(|error| format!("request user capabilities: {error}"))?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 16 * 1024)
            .await
            .map_err(|error| format!("read user capabilities response: {error}"))?;
        let body: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("parse user capabilities response: {error}"))?;
        assert_eq!(body["apiVersion"], "steward.user-capabilities/v1");
        assert_eq!(
            body["workspace"]["enabledTypes"],
            serde_json::json!(["git"])
        );
        assert_eq!(
            body["workspace"]["maxTotalBytes"],
            3 * 1024 * 1024 * 1024_u64
        );
        assert_eq!(body["workspace"]["maxHistoryDepth"], 42);
        assert_eq!(body["agents"][0]["agentRef"], "example-agent@1.0.0");
        assert_eq!(
            body["agents"][0]["models"]["providers"],
            serde_json::json!(["example"])
        );
        Ok(())
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
                    single_run_limit: Some("0.50".to_owned()),
                    currency: "USD".to_owned(),
                },
                runtime_minutes_limit: Some("120".to_owned()),
                workspace: None,
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
                        single_run_limit: Some("0.25".to_owned()),
                        currency: "USD".to_owned(),
                    },
                    ttl: Duration("24h".to_owned()),
                    runner: steward_types::RunnerRequirements::default(),
                    ..ceiling.spec.clone()
                },
            }),
            allow_inline_browser_tasks: true,
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

    #[derive(Clone, Default)]
    struct TestProjectionStore {
        users: BTreeMap<String, CanonicalUserRecord>,
        templates: BTreeMap<(String, i64), EnvelopeTemplateRevisionRecord>,
    }

    impl UserEnvelopeProjectionStore for TestProjectionStore {
        fn canonical_user<'a>(
            &'a self,
            user_id: &'a CanonicalUserId,
        ) -> BoxFuture<'a, Result<Option<CanonicalUserRecord>, StoreError>> {
            let user = self.users.get(user_id.as_str()).cloned();
            Box::pin(async move { Ok(user) })
        }

        fn envelope_template_revision<'a>(
            &'a self,
            template_id: &'a str,
            revision: i64,
        ) -> BoxFuture<'a, Result<Option<EnvelopeTemplateRevisionRecord>, StoreError>> {
            let template = self
                .templates
                .get(&(template_id.to_owned(), revision))
                .cloned();
            Box::pin(async move { Ok(template) })
        }
    }

    fn canonical_user(
        user_id: &str,
        organization_id: &str,
        display_email: &str,
    ) -> Result<CanonicalUserRecord, String> {
        Ok(CanonicalUserRecord {
            user_id: CanonicalUserId::parse(user_id)?,
            organization_id: OrganizationId::parse(organization_id)?,
            display_email: Email::parse(display_email)?,
            display_name: None,
            state: "active".to_owned(),
            created_at: "2026-08-17T00:00:00Z".to_owned(),
            last_sign_in_at: None,
            invited_by: None,
        })
    }

    #[tokio::test]
    async fn request_projection_uses_the_pinned_template_and_same_organization_actor_emails()
    -> Result<(), String> {
        let session = session()?;
        let same_organization_actor = "usr_abcdef0123456789abcdef0123456789";
        let other_organization_actor = "usr_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let mut request = TestBroker::default()
            .get(&session, Uuid::from_u128(1))
            .await
            .map_err(|error| format!("load request fixture: {error:?}"))?
            .ok_or_else(|| "missing request fixture".to_owned())?;
        request.template_display_name = None;
        request.history = [
            session.subject.canonical_user_id.as_str(),
            same_organization_actor,
            other_organization_actor,
        ]
        .into_iter()
        .map(|actor| EnvelopeRequestHistoryEvent {
            status: EnvelopeRequestStatus::Pending,
            at: "2026-08-17T00:00:00Z".to_owned(),
            actor: actor.to_owned(),
            actor_display_email: None,
            reason: None,
            rationale: None,
            evidence_url: None,
            expires_at: None,
        })
        .collect();

        let envelope_template = template();
        let store = TestProjectionStore {
            users: [
                canonical_user(
                    session.subject.canonical_user_id.as_str(),
                    "org_0123456789abcdef0123456789abcdef",
                    "alice@example.com",
                )?,
                canonical_user(
                    same_organization_actor,
                    "org_0123456789abcdef0123456789abcdef",
                    "bob@example.org",
                )?,
                canonical_user(
                    other_organization_actor,
                    "org_abcdef0123456789abcdef0123456789",
                    "carol@example.com",
                )?,
            ]
            .into_iter()
            .map(|user| (user.user_id.as_str().to_owned(), user))
            .collect(),
            templates: [(
                (envelope_template.id.clone(), envelope_template.revision),
                EnvelopeTemplateRevisionRecord {
                    template_id: envelope_template.id,
                    display_name: "Pinned Engineer".to_owned(),
                    member_roles: Vec::new(),
                    ceiling: envelope_template.ceiling,
                    auto_provision_threshold: envelope_template.auto_provision_threshold,
                    allow_inline_browser_tasks: envelope_template.allow_inline_browser_tasks,
                },
            )]
            .into_iter()
            .collect(),
        };

        let projected = attach_request_projection(&store, &session.subject, request)
            .await
            .map_err(|error| format!("project request: {error:?}"))?;
        assert_eq!(
            projected.template_display_name.as_deref(),
            Some("Pinned Engineer")
        );
        assert_eq!(
            projected.history[0].actor_display_email.as_deref(),
            Some("alice@example.com")
        );
        assert_eq!(
            projected.history[1].actor_display_email.as_deref(),
            Some("bob@example.org")
        );
        assert_eq!(projected.history[2].actor_display_email, None);
        Ok(())
    }

    #[tokio::test]
    async fn envelope_request_limits_are_bounded_and_malformed_values_are_unprocessable()
    -> Result<(), String> {
        let app = inner_router(TestBroker::default());
        for query in ["limit=abc", "limit=0", "limit=101"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/app/api/v1/envelope-requests?{query}"))
                        .extension(session()?)
                        .body(Body::empty())
                        .map_err(|error| format!("build list request: {error}"))?,
                )
                .await
                .map_err(|error| format!("list envelope requests: {error}"))?;
            assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        }

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/app/api/v1/envelope-requests?limit=100")
                    .extension(session()?)
                    .body(Body::empty())
                    .map_err(|error| format!("build valid list request: {error}"))?,
            )
            .await
            .map_err(|error| format!("list envelope requests: {error}"))?;
        assert_eq!(response.status(), StatusCode::OK);
        Ok(())
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
    async fn within_template_request_on_an_empty_capability_catalog_provisions()
    -> Result<(), String> {
        let broker = TestBroker {
            capability_violation_fields: vec![
                EnvelopeRequestField::Llms,
                EnvelopeRequestField::Tools,
            ],
            ..TestBroker::default()
        };
        let mut workspace_capabilities =
            crate::tasks::TaskApiConfig::default().workspace_deployment_capabilities();
        workspace_capabilities.enabled_types.clear();
        let response = inner_router_with_capabilities(broker, Vec::new(), workspace_capabilities)
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
                            "requestedEnvelope": template().auto_provision_threshold,
                            "idempotencyKey": "within-template-stale-catalog",
                        })
                        .to_string(),
                    ))
                    .map_err(|error| format!("build within-template request: {error}"))?,
            )
            .await
            .map_err(|error| format!("submit within-template request: {error}"))?;
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = to_bytes(response.into_body(), 16 * 1024)
            .await
            .map_err(|error| format!("read within-template response: {error}"))?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("parse within-template response: {error}"))?;
        assert_eq!(value["request"]["status"], "provisioned");
        Ok(())
    }

    #[tokio::test]
    async fn within_template_request_with_a_stale_catalog_capability_provisions()
    -> Result<(), String> {
        let response = inner_router(TestBroker {
            capability_violation_fields: vec![EnvelopeRequestField::Llms],
            ..TestBroker::default()
        })
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
                        "requestedEnvelope": template().auto_provision_threshold,
                        "idempotencyKey": "within-template-stale-model",
                    })
                    .to_string(),
                ))
                .map_err(|error| format!("build stale-catalog request: {error}"))?,
        )
        .await
        .map_err(|error| format!("submit stale-catalog request: {error}"))?;
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = to_bytes(response.into_body(), 16 * 1024)
            .await
            .map_err(|error| format!("read stale-catalog response: {error}"))?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("parse stale-catalog response: {error}"))?;
        assert_eq!(value["request"]["status"], "provisioned");
        Ok(())
    }

    #[tokio::test]
    async fn custom_envelope_request_is_template_free_and_always_pending() -> Result<(), String> {
        let app = inner_router(TestBroker::default());
        let requested = template().auto_provision_threshold;
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/app/api/v1/envelope-requests")
                    .header("content-type", "application/json")
                    .extension(session()?)
                    .extension(UserEnvelopeMutationProof)
                    .body(Body::from(
                        serde_json::json!({
                            "requestedEnvelope": requested,
                            "idempotencyKey": "custom-request-1",
                            "justification": "Needed for a governed task",
                        })
                        .to_string(),
                    ))
                    .map_err(|error| format!("build custom request: {error}"))?,
            )
            .await
            .map_err(|error| format!("submit custom request: {error}"))?;
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = to_bytes(response.into_body(), 16 * 1024)
            .await
            .map_err(|error| format!("read custom response: {error}"))?;
        let value: serde_json::Value =
            serde_json::from_slice(&body).map_err(|error| format!("parse response: {error}"))?;
        assert_eq!(value["request"]["status"], "pending");
        assert!(value["request"]["templateId"].is_null());
        assert!(value["request"]["templateRevision"].is_null());

        let partial = app
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
                            "requestedEnvelope": template().ceiling,
                            "idempotencyKey": "invalid-partial-template",
                        })
                        .to_string(),
                    ))
                    .map_err(|error| format!("build partial request: {error}"))?,
            )
            .await
            .map_err(|error| format!("submit partial request: {error}"))?;
        assert_eq!(partial.status(), StatusCode::BAD_REQUEST);

        let unavailable_capability_broker = TestBroker {
            capability_violation_fields: vec![EnvelopeRequestField::Llms],
            ..TestBroker::default()
        };
        let unavailable_capability = inner_router(unavailable_capability_broker.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/app/api/v1/envelope-requests")
                    .header("content-type", "application/json")
                    .extension(session()?)
                    .extension(UserEnvelopeMutationProof)
                    .body(Body::from(
                        serde_json::json!({
                            "requestedEnvelope": template().ceiling,
                            "idempotencyKey": "custom-unavailable-capability",
                            "justification": "Needed for a governed task",
                        })
                        .to_string(),
                    ))
                    .map_err(|error| format!("build unavailable capability request: {error}"))?,
            )
            .await
            .map_err(|error| format!("submit unavailable capability request: {error}"))?;
        assert_eq!(
            unavailable_capability.status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let body = to_bytes(unavailable_capability.into_body(), 16 * 1024)
            .await
            .map_err(|error| format!("read unavailable capability response: {error}"))?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("parse unavailable capability response: {error}"))?;
        assert_eq!(value["error"], "capability_not_available");
        assert_eq!(value["fields"], serde_json::json!(["llms"]));
        assert!(
            value["failureReason"]
                .as_str()
                .is_some_and(|reason| reason.contains("models"))
        );
        assert!(
            unavailable_capability_broker
                .create_owners
                .lock()
                .map_err(|_| "lock unavailable-capability create calls")?
                .is_empty(),
            "a capability-catalog rejection must not persist a request"
        );

        Ok(())
    }

    #[tokio::test]
    async fn requests_that_need_review_require_a_justification() -> Result<(), String> {
        let mut above_template = template().ceiling;
        above_template.spec.budget.monthly_limit = "101.00".to_owned();
        for (body, idempotency_key) in [
            (
                serde_json::json!({
                    "requestedEnvelope": template().ceiling,
                    "idempotencyKey": "custom-without-justification",
                }),
                "custom-without-justification",
            ),
            (
                serde_json::json!({
                    "templateId": "engineer",
                    "templateRevision": 3,
                    "requestedEnvelope": above_template,
                    "idempotencyKey": "exception-without-justification",
                }),
                "exception-without-justification",
            ),
        ] {
            let response = inner_router(TestBroker::default())
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/app/api/v1/envelope-requests")
                        .header("content-type", "application/json")
                        .extension(session()?)
                        .extension(UserEnvelopeMutationProof)
                        .body(Body::from(body.to_string()))
                        .map_err(|error| format!("build {idempotency_key} request: {error}"))?,
                )
                .await
                .map_err(|error| format!("submit {idempotency_key} request: {error}"))?;
            assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
            let body = to_bytes(response.into_body(), 16 * 1024)
                .await
                .map_err(|error| format!("read {idempotency_key} response: {error}"))?;
            let value: serde_json::Value = serde_json::from_slice(&body)
                .map_err(|error| format!("parse {idempotency_key} response: {error}"))?;
            assert_eq!(value["error"], "justification_required");
            assert_eq!(value["fields"], serde_json::json!(["justification"]));
        }
        Ok(())
    }

    #[tokio::test]
    async fn request_rejects_unavailable_model_tool_and_workspace_with_plain_fields()
    -> Result<(), String> {
        let broker = TestBroker {
            capability_violation_fields: vec![
                EnvelopeRequestField::Llms,
                EnvelopeRequestField::Tools,
            ],
            ..TestBroker::default()
        };
        let mut workspace_capabilities =
            crate::tasks::TaskApiConfig::default().workspace_deployment_capabilities();
        workspace_capabilities.enabled_types = vec!["scratch".to_owned()];
        let mut requested = template().ceiling;
        requested.spec.workspace = Some(WorkspaceAuthority {
            allowed_types: vec![WorkspaceEntryType::Git],
            max_total_size: Some(ResourceQuantity::parse("1Gi")?),
            max_files: Some(1_000),
            max_history_depth: Some(100),
            max_submodule_depth: Some(1),
        });
        let response = inner_router_with_capabilities(broker, Vec::new(), workspace_capabilities)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/app/api/v1/envelope-requests")
                    .header("content-type", "application/json")
                    .extension(session()?)
                    .extension(UserEnvelopeMutationProof)
                    .body(Body::from(
                        serde_json::json!({
                            "requestedEnvelope": requested,
                            "idempotencyKey": "unavailable-capabilities",
                            "justification": "Needed for a governed task",
                        })
                        .to_string(),
                    ))
                    .map_err(|error| format!("build unavailable-capability request: {error}"))?,
            )
            .await
            .map_err(|error| format!("submit unavailable-capability request: {error}"))?;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = to_bytes(response.into_body(), 16 * 1024)
            .await
            .map_err(|error| format!("read unavailable-capability response: {error}"))?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("parse unavailable-capability response: {error}"))?;
        assert_eq!(value["error"], "capability_not_available");
        assert_eq!(
            value["fields"],
            serde_json::json!(["llms", "tools", "workspace"])
        );
        assert_eq!(
            value["failureReason"],
            "This deployment does not provide the requested models, tools, workspace."
        );
        Ok(())
    }

    #[tokio::test]
    async fn template_without_runtime_limit_accepts_another_dimension_for_review()
    -> Result<(), String> {
        let mut unbounded_runtime_template = template();
        unbounded_runtime_template
            .ceiling
            .spec
            .runtime_minutes_limit = None;
        if let Some(threshold) = unbounded_runtime_template.auto_provision_threshold.as_mut() {
            threshold.spec.runtime_minutes_limit = None;
        }
        let mut requested = unbounded_runtime_template.ceiling.clone();
        requested.spec.budget.monthly_limit = "101.00".to_owned();
        let app = inner_router(TestBroker {
            template_override: Some(unbounded_runtime_template),
            ..TestBroker::default()
        });
        let response = app
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
                            "idempotencyKey": "unbounded-runtime-review",
                            "justification": "Needed for a governed task",
                        })
                        .to_string(),
                    ))
                    .map_err(|error| format!("build unbounded-runtime request: {error}"))?,
            )
            .await
            .map_err(|error| format!("submit unbounded-runtime request: {error}"))?;
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = to_bytes(response.into_body(), 16 * 1024)
            .await
            .map_err(|error| format!("read unbounded-runtime response: {error}"))?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("parse unbounded-runtime response: {error}"))?;
        assert_eq!(value["request"]["status"], "pending");
        assert_eq!(
            value["request"]["ceilingExceedances"],
            serde_json::to_value([AdmissionDelta::Budget {
                requested: "101.00".to_owned(),
                ceiling: "100.00".to_owned(),
                currency: "USD".to_owned(),
            }])
            .map_err(|error| format!("serialize expected budget delta: {error}"))?
        );
        Ok(())
    }

    #[tokio::test]
    async fn exact_request_retry_returns_immutable_result_after_review_boundary_drift()
    -> Result<(), String> {
        let requested_envelope = template().ceiling;
        let replayed = UserEnvelopeRequest {
            id: Uuid::from_u128(7),
            template_id: None,
            template_display_name: None,
            template_revision: None,
            requested_envelope: requested_envelope.clone(),
            template_envelope: None,
            justification: Some("bounded exception".to_owned()),
            ceiling_exceedances: Vec::new(),
            approved_envelope: None,
            status: EnvelopeRequestStatus::Pending,
            approval_id: None,
            envelope_instance_id: None,
            envelope_digest: None,
            reason: None,
            status_actor: "usr_0123456789abcdef0123456789abcdef".to_owned(),
            status_template_revision: None,
            created_at: "2026-08-17T00:00:00Z".to_owned(),
            status_at: "2026-08-17T00:00:00Z".to_owned(),
            history: Vec::new(),
            usage: None,
        };
        let broker = TestBroker {
            capability_violation_fields: vec![EnvelopeRequestField::Llms],
            ..TestBroker::default()
        };
        *broker
            .replayed_request
            .lock()
            .map_err(|_| "lock replay fixture")? = Some(replayed.clone());
        let app = inner_router(broker.clone());
        let retry_body = serde_json::json!({
            "requestedEnvelope": requested_envelope,
            "idempotencyKey": "custom-request-retry",
            "justification": "bounded exception",
        });
        let retry = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/app/api/v1/envelope-requests")
                    .header("content-type", "application/json")
                    .extension(session()?)
                    .extension(UserEnvelopeMutationProof)
                    .body(Body::from(retry_body.to_string()))
                    .map_err(|error| format!("build exact request retry: {error}"))?,
            )
            .await
            .map_err(|error| format!("submit exact request retry: {error}"))?;
        assert_eq!(retry.status(), StatusCode::CREATED);
        let body = to_bytes(retry.into_body(), 16 * 1024)
            .await
            .map_err(|error| format!("read exact request retry: {error}"))?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("decode exact request retry: {error}"))?;
        assert_eq!(value["request"]["id"], replayed.id.to_string());
        assert!(
            broker
                .create_owners
                .lock()
                .map_err(|_| "lock replay create calls")?
                .is_empty(),
            "an exact retry must not reserve or revalidate a second request"
        );

        let changed = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/app/api/v1/envelope-requests")
                    .header("content-type", "application/json")
                    .extension(session()?)
                    .extension(UserEnvelopeMutationProof)
                    .body(Body::from(
                        serde_json::json!({
                            "requestedEnvelope": replayed.requested_envelope,
                            "idempotencyKey": "custom-request-retry",
                            "justification": "changed exception",
                        })
                        .to_string(),
                    ))
                    .map_err(|error| format!("build conflicting request retry: {error}"))?,
            )
            .await
            .map_err(|error| format!("submit conflicting request retry: {error}"))?;
        assert_eq!(changed.status(), StatusCode::CONFLICT);
        Ok(())
    }

    #[tokio::test]
    async fn exact_retry_completes_a_reserved_auto_provision_request() -> Result<(), String> {
        let requested_envelope = template()
            .auto_provision_threshold
            .ok_or_else(|| "missing automatic-provisioning threshold".to_owned())?;
        let replayed = UserEnvelopeRequest {
            id: Uuid::from_u128(8),
            template_id: Some("engineer".to_owned()),
            template_display_name: Some("Engineer".to_owned()),
            template_revision: Some(3),
            requested_envelope: requested_envelope.clone(),
            template_envelope: Some(template().ceiling),
            justification: None,
            ceiling_exceedances: Vec::new(),
            approved_envelope: None,
            status: EnvelopeRequestStatus::Pending,
            approval_id: None,
            envelope_instance_id: None,
            envelope_digest: None,
            reason: None,
            status_actor: "usr_0123456789abcdef0123456789abcdef".to_owned(),
            status_template_revision: Some(3),
            created_at: "2026-08-17T00:00:00Z".to_owned(),
            status_at: "2026-08-17T00:00:00Z".to_owned(),
            history: Vec::new(),
            usage: None,
        };
        let broker = TestBroker::default();
        *broker
            .replayed_request
            .lock()
            .map_err(|_| "lock replay fixture")? = Some(replayed);

        let response = inner_router(broker.clone())
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
                            "requestedEnvelope": requested_envelope,
                            "idempotencyKey": "reserved-auto-request-retry",
                        })
                        .to_string(),
                    ))
                    .map_err(|error| format!("build automatic retry: {error}"))?,
            )
            .await
            .map_err(|error| format!("submit automatic retry: {error}"))?;
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = to_bytes(response.into_body(), 16 * 1024)
            .await
            .map_err(|error| format!("read automatic retry: {error}"))?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("decode automatic retry: {error}"))?;
        assert_eq!(value["request"]["id"], Uuid::from_u128(8).to_string());
        assert_eq!(value["request"]["status"], "provisioned");
        assert_eq!(
            broker
                .create_owners
                .lock()
                .map_err(|_| "lock automatic retry create calls")?
                .len(),
            1,
            "the retry must resume the reserved request's provisioning transition"
        );
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
        assert!(yaml.contains("# steward-run-workflow-installation-mode: remote"));
        assert!(yaml.contains("      workflow: repository-review@1"));
        assert!(yaml.contains(
            "uses: example-org/steward-run/.github/workflows/steward-task-self-hosted.yml@3333333333333333333333333333333333333333"
        ));
        assert!(yaml.contains(&format!(
            "      envelope-digest: steward:sha256:{}",
            "a".repeat(64)
        )));
        assert!(yaml.contains("identity-exchange-url: ${{ vars.IDENTITY_EXCHANGE_URL }}"));
        assert!(
            yaml.contains("identity-exchange-audience: ${{ vars.IDENTITY_EXCHANGE_AUDIENCE }}")
        );
        assert!(
            yaml.contains("steward-ca-certificate-file: ${{ vars.STEWARD_CA_CERTIFICATE_FILE }}")
        );
        assert!(!yaml.contains("contents: write"));
        assert!(!yaml.contains("coding-agent-runtime"));
        assert!(!yaml.contains("TARGET_REVISION"));
        assert!(!yaml.contains("TARGET_PATH"));
        Ok(())
    }

    #[tokio::test]
    async fn provisioned_envelope_renders_configured_vendored_workflow_without_legacy_identity_inputs()
    -> Result<(), String> {
        let app = inner_router(TestBroker {
            workflow_installation_mode: StewardRunWorkflowInstallationMode::Vendored,
            task_identity_discovery_enabled: true,
            ..TestBroker::default()
        });
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
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024)
            .await
            .map_err(|error| format!("read render response: {error}"))?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("parse render response: {error}"))?;
        let yaml = value["workflow"]["yaml"]
            .as_str()
            .ok_or_else(|| "render response omitted YAML".to_owned())?;

        assert!(yaml.contains("# steward-run-workflow-installation-mode: vendored"));
        assert!(yaml.contains("    uses: ./.github/workflows/steward-task-vendored.yml"));
        assert!(!yaml.contains("identity-exchange-url:"));
        assert!(!yaml.contains("identity-exchange-audience:"));
        assert!(!yaml.contains("steward-ca-certificate-file:"));
        Ok(())
    }

    #[tokio::test]
    async fn provisioned_envelope_falls_back_for_a_release_without_package_path_support()
    -> Result<(), String> {
        let app = inner_router(TestBroker {
            task_identity_discovery_enabled: true,
            ..TestBroker::default()
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/app/api/v1/envelope-requests/00000000-0000-0000-0000-000000000001/repository-bundle")
                    .header("content-type", "application/json")
                    .extension(session()?)
                    .extension(UserEnvelopeMutationProof)
                    .body(Body::from(
                        serde_json::json!({
                            "repository": "https://github.com/example-org/agentic-ops.git",
                            "packagePath": ".steward/tasks/browser-task/task-definition.json"
                        })
                        .to_string(),
                    ))
                    .map_err(|error| format!("build bundle request: {error}"))?,
            )
            .await
            .map_err(|error| format!("render bundle: {error}"))?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 128 * 1024)
            .await
            .map_err(|error| format!("read bundle response: {error}"))?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("parse bundle response: {error}"))?;
        assert_eq!(
            value["files"].as_object().map(serde_json::Map::len),
            Some(2)
        );
        assert!(value["files"][".steward/invocations/browser-task.json"].is_string());
        let workflow = value["files"][".github/workflows/steward-browser-task.yml"]
            .as_str()
            .ok_or_else(|| "bundle omitted caller workflow".to_owned())?;
        assert!(workflow.contains("invocation-path: .steward/invocations/browser-task.json"));
        assert!(!workflow.contains("package-path:"));
        assert!(!workflow.contains("execution-log:"));
        assert!(workflow.contains(
            "uses: example-org/steward-run/.github/workflows/steward-task-self-hosted.yml@3333333333333333333333333333333333333333"
        ));
        assert!(!workflow.contains("identity-exchange-url:"));
        Ok(())
    }

    #[tokio::test]
    async fn provisioned_envelope_uses_two_file_bundle_for_a_capable_release() -> Result<(), String>
    {
        let app = inner_router(TestBroker {
            steward_run_version: "0.8.0".to_owned(),
            task_identity_discovery_enabled: true,
            ..TestBroker::default()
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/app/api/v1/envelope-requests/00000000-0000-0000-0000-000000000001/repository-bundle")
                    .header("content-type", "application/json")
                    .extension(session()?)
                    .extension(UserEnvelopeMutationProof)
                    .body(Body::from(
                        serde_json::json!({
                            "repository": "https://github.com/example-org/agentic-ops.git",
                            "packagePath": ".steward/tasks/browser-task/task-definition.json"
                        })
                        .to_string(),
                    ))
                    .map_err(|error| format!("build bundle request: {error}"))?,
            )
            .await
            .map_err(|error| format!("render bundle: {error}"))?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 128 * 1024)
            .await
            .map_err(|error| format!("read bundle response: {error}"))?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("parse bundle response: {error}"))?;
        assert_eq!(
            value["files"].as_object().map(serde_json::Map::len),
            Some(1)
        );
        let workflow = value["files"][".github/workflows/hypershell-browser-task.yml"]
            .as_str()
            .ok_or_else(|| "bundle omitted caller workflow".to_owned())?;
        assert!(
            workflow.contains("package-path: .steward/tasks/browser-task/task-definition.json")
        );
        assert!(workflow.contains("execution-log: full"));
        assert!(!workflow.contains("invocation-path:"));
        Ok(())
    }

    #[tokio::test]
    async fn provisioned_envelope_retains_the_explicit_legacy_invocation_bundle()
    -> Result<(), String> {
        let app = inner_router(TestBroker {
            task_identity_discovery_enabled: true,
            ..TestBroker::default()
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/app/api/v1/envelope-requests/00000000-0000-0000-0000-000000000001/repository-bundle")
                    .header("content-type", "application/json")
                    .extension(session()?)
                    .extension(UserEnvelopeMutationProof)
                    .body(Body::from(
                        serde_json::json!({
                            "repository": "https://github.com/example-org/agentic-ops.git",
                            "packagePath": ".steward/tasks/browser-task/task-definition.json",
                            "invocationPath": ".steward/invocations/browser-task.json"
                        })
                        .to_string(),
                    ))
                    .map_err(|error| format!("build bundle request: {error}"))?,
            )
            .await
            .map_err(|error| format!("render bundle: {error}"))?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 128 * 1024)
            .await
            .map_err(|error| format!("read bundle response: {error}"))?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("parse bundle response: {error}"))?;
        assert_eq!(
            value["files"].as_object().map(serde_json::Map::len),
            Some(2)
        );
        assert!(value["files"][".steward/invocations/browser-task.json"].is_string());
        let workflow = value["files"][".github/workflows/steward-browser-task.yml"]
            .as_str()
            .ok_or_else(|| "bundle omitted caller workflow".to_owned())?;
        assert!(workflow.contains("invocation-path: .steward/invocations/browser-task.json"));
        assert!(!workflow.contains("package-path:"));
        assert!(!workflow.contains("execution-log:"));
        assert!(!workflow.contains(
            "invocation-path: .steward/invocations/browser-task.json\n\n      envelope-digest:"
        ));
        Ok(())
    }

    #[tokio::test]
    async fn unauthenticated_requests_fail_closed_and_above_template_budget_remains_pending()
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
                    "justification": "Needed for a governed task",
                })
                .to_string(),
            ))
            .map_err(|error| format!("build excessive request: {error}"))?;
        let response = app
            .oneshot(request)
            .await
            .map_err(|error| format!("send excessive request: {error}"))?;
        assert_eq!(response.status(), StatusCode::CREATED);
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
    async fn historical_auto_threshold_does_not_add_a_second_review_boundary() -> Result<(), String>
    {
        let mut requested = template().ceiling;
        requested.spec.budget.monthly_limit = "75.00".to_owned();
        let response = inner_router(TestBroker::default())
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
                            "idempotencyKey": "request-within-limits",
                        })
                        .to_string(),
                    ))
                    .map_err(|error| format!("build request: {error}"))?,
            )
            .await
            .map_err(|error| format!("submit request: {error}"))?;
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = to_bytes(response.into_body(), 16 * 1024)
            .await
            .map_err(|error| format!("read response: {error}"))?;
        let value: serde_json::Value =
            serde_json::from_slice(&body).map_err(|error| format!("parse response: {error}"))?;
        assert_eq!(value["request"]["status"], "provisioned");
        assert_eq!(
            value["request"]["approvedEnvelope"],
            value["request"]["requestedEnvelope"]
        );
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
                let template_envelope = request.template.map(|template| template.ceiling.clone());
                let justification = request.justification.map(str::to_owned);
                let ceiling_exceedances = request.ceiling_exceedances.to_vec();
                let auto_provision = request.auto_provision;
                Box::pin(async move {
                    let approved_envelope = auto_provision.then(|| requested_envelope.clone());
                    Ok(UserEnvelopeRequest {
                        id: Uuid::nil(),
                        template_id: Some("engineer".to_owned()),
                        template_display_name: Some("Engineer".to_owned()),
                        template_revision: Some(3),
                        requested_envelope,
                        template_envelope,
                        justification,
                        ceiling_exceedances,
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
                        status_template_revision: Some(3),
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
    async fn runtime_minutes_above_the_template_limits_remain_pending() -> Result<(), String> {
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
                            "justification": "Needed for a governed task",
                        })
                        .to_string(),
                    ))
                    .map_err(|error| format!("build request: {error}"))?,
            )
            .await
            .map_err(|error| format!("submit request: {error}"))?;
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = to_bytes(response.into_body(), 16 * 1024)
            .await
            .map_err(|error| format!("read runtime-minutes response: {error}"))?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("parse runtime-minutes response: {error}"))?;
        assert_eq!(value["request"]["status"], "pending");
        assert!(value["request"]["approvedEnvelope"].is_null());
        Ok(())
    }

    #[tokio::test]
    async fn every_template_ceiling_dimension_can_be_submitted_for_review() -> Result<(), String> {
        let base = template().ceiling;
        let mut cases = Vec::new();

        let mut requested = base.clone();
        requested.spec.budget.monthly_limit = "101.00".to_owned();
        cases.push((
            "budget",
            requested,
            AdmissionDelta::Budget {
                requested: "101.00".to_owned(),
                ceiling: "100.00".to_owned(),
                currency: "USD".to_owned(),
            },
        ));

        let mut requested = base.clone();
        requested.spec.budget.single_run_limit = Some("1.00".to_owned());
        cases.push((
            "singleRunBudget",
            requested,
            AdmissionDelta::SingleRunBudget {
                requested: Some("1.00".to_owned()),
                ceiling: "0.50".to_owned(),
                currency: "USD".to_owned(),
            },
        ));

        let mut requested = base.clone();
        requested.spec.runtime_minutes_limit = Some("121".to_owned());
        cases.push((
            "runtimeMinutes",
            requested,
            AdmissionDelta::RuntimeMinutes {
                requested: Some("121".to_owned()),
                ceiling: "120".to_owned(),
            },
        ));

        let mut requested = base.clone();
        requested.spec.ttl = Duration("73h".to_owned());
        cases.push((
            "ttl",
            requested,
            AdmissionDelta::Ttl {
                requested: "73h".to_owned(),
                ceiling: "72h".to_owned(),
            },
        ));

        let mut requested = base.clone();
        let outside_model = ModelRef {
            provider: "provider-a".to_owned(),
            model: "model-a".to_owned(),
        };
        requested.spec.llms.push(outside_model.clone());
        cases.push((
            "models",
            requested,
            AdmissionDelta::Models {
                requested: vec![outside_model],
                ceiling: base.spec.llms.clone(),
            },
        ));

        let mut requested = base.clone();
        let outside_tool = ToolGrant {
            provider: "github".to_owned(),
            resource: "issues".to_owned(),
            action: "read".to_owned(),
        };
        requested.spec.tools.push(outside_tool.clone());
        cases.push((
            "tools",
            requested,
            AdmissionDelta::Tools {
                requested: vec![outside_tool],
                ceiling: base.spec.tools.clone(),
            },
        ));

        let mut requested = base.clone();
        requested.spec.runner.platforms = vec![RunnerPlatform::Linux];
        cases.push((
            "runnerPlatforms",
            requested,
            AdmissionDelta::RunnerPlatforms {
                requested: vec![RunnerPlatform::Linux],
                ceiling: Vec::new(),
            },
        ));

        let mut requested = base.clone();
        requested.spec.runner.memory = Some(KubernetesQuantity("1Gi".to_owned()));
        cases.push((
            "runnerMemory",
            requested,
            AdmissionDelta::RunnerMemory {
                requested: KubernetesQuantity("1Gi".to_owned()),
                ceiling: None,
            },
        ));

        let mut requested = base.clone();
        requested.spec.runner.compute = Some(KubernetesQuantity("500m".to_owned()));
        cases.push((
            "runnerCompute",
            requested,
            AdmissionDelta::RunnerCompute {
                requested: KubernetesQuantity("500m".to_owned()),
                ceiling: None,
            },
        ));

        let mut requested = base.clone();
        requested.spec.runner.storage = Some(KubernetesQuantity("2Gi".to_owned()));
        cases.push((
            "runnerStorage",
            requested,
            AdmissionDelta::RunnerStorage {
                requested: KubernetesQuantity("2Gi".to_owned()),
                ceiling: None,
            },
        ));

        let mut requested = base;
        let outside_workspace = WorkspaceAuthority {
            allowed_types: vec![WorkspaceEntryType::Git],
            max_total_size: Some(ResourceQuantity::parse("1Gi")?),
            max_files: Some(1_000),
            max_history_depth: Some(100),
            max_submodule_depth: Some(1),
        };
        requested.spec.workspace = Some(outside_workspace.clone());
        cases.push((
            "workspace",
            requested,
            AdmissionDelta::Workspace {
                requested: outside_workspace,
                ceiling: None,
            },
        ));

        for (dimension, requested_envelope, expected_delta) in cases {
            let response = inner_router(TestBroker::default())
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
                                "requestedEnvelope": requested_envelope,
                                "idempotencyKey": format!("review-{dimension}"),
                                "justification": "Needed for the requested task",
                            })
                            .to_string(),
                        ))
                        .map_err(|error| format!("build {dimension} request: {error}"))?,
                )
                .await
                .map_err(|error| format!("submit {dimension} request: {error}"))?;
            assert_eq!(
                response.status(),
                StatusCode::CREATED,
                "{dimension} should be reviewable"
            );
            let body = to_bytes(response.into_body(), 16 * 1024)
                .await
                .map_err(|error| format!("read {dimension} response: {error}"))?;
            let value: serde_json::Value = serde_json::from_slice(&body)
                .map_err(|error| format!("parse {dimension} response: {error}"))?;
            assert_eq!(value["request"]["status"], "pending");
            assert_eq!(
                value["request"]["justification"],
                "Needed for the requested task"
            );
            assert_eq!(
                value["request"]["ceilingExceedances"],
                serde_json::to_value([expected_delta])
                    .map_err(|error| format!("serialize expected {dimension} delta: {error}"))?
            );
            assert_eq!(
                value["request"]["requestedEnvelope"],
                serde_json::to_value(&requested_envelope).map_err(|error| format!(
                    "serialize requested {dimension} envelope: {error}"
                ))?
            );
            assert_eq!(
                value["request"]["templateEnvelope"],
                serde_json::to_value(template().ceiling)
                    .map_err(|error| format!("serialize template {dimension} envelope: {error}"))?
            );
        }
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
                .map(|result| result.clone())
                .unwrap_or(Err(ConnectionBrokerError::Unavailable));
            Box::pin(async move {
                let phase = result?;
                Ok(ProviderConnectionStatus {
                    phase,
                    account_email: None,
                    account_id: None,
                    account_login: None,
                    github_actions_identity_linked: None,
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
        ) -> BoxFuture<'a, Result<ReservedConnectionStart, ConnectionBrokerError>> {
            Box::pin(async { Err(ConnectionBrokerError::Unavailable) })
        }

        fn start_operation<'a>(
            &'a self,
            _session: &'a ConnectionSession<()>,
            _operation_id: Uuid,
        ) -> BoxFuture<'a, Result<Option<ConnectionStartOperation>, ConnectionBrokerError>>
        {
            Box::pin(async { Err(ConnectionBrokerError::Unavailable) })
        }

        fn disconnect<'a>(
            &'a self,
            _session: &'a ConnectionSession<()>,
        ) -> BoxFuture<'a, Result<ReservedConnectionStart, ConnectionBrokerError>> {
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
