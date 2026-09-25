//! Browser-session administrator APIs backed by Steward's existing authority paths.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use steward_admission::{AdmissionDecision, Envelope, validate_envelope};
use steward_store::{
    EnvelopeRequestRecord, EnvelopeRequestStatus, EnvelopeRequestStatusUpdate,
    FederatedSubjectAssociation, FederatedSubjectAuditRecord, FederatedSubjectDisable,
    FederatedSubjectRecord, PendingApproval, PendingEnvelopeRequest, PgStore, StoreError,
};
use steward_types::{AgentRuntimeSpec, CanonicalUserId, ModelRef, ToolGrant};
use uuid::Uuid;

use crate::browser_auth::{
    BrowserAdminAuthority, BrowserAuthService, BrowserMutationProof, BrowserMutationRequest,
    protect_browser_admin_routes,
};
use crate::user_envelopes::{BrowserEnvelope, envelope_content_digest, envelope_instance_id};
use crate::{
    AdminContext, AdmissionLedger, ApiError, ApprovalRequest, DecisionChannel, RuntimeRepository,
    approve_parked_request, file_decision_reference,
};

const BROWSER_ADMIN_API_VERSION: &str = "steward.browser-admin/v1";
pub const MAX_CAPABILITY_CATALOG_BYTES: usize = 256 * 1024;
const MAX_CAPABILITY_MODELS: usize = 256;
const MAX_CAPABILITY_TOOLS: usize = 1024;

#[derive(Clone)]
pub(crate) struct FederatedSubjectAdminState {
    store: PgStore,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserFederatedSubjectView {
    #[schema(value_type = String, format = "uuid")]
    subject_id: Uuid,
    issuer: String,
    subject: String,
    state: String,
    canonical_user_id: Option<CanonicalUserId>,
    actor_login: Option<String>,
    display_name: Option<String>,
    revision: i64,
    first_seen_at: String,
    last_seen_at: String,
    updated_at: String,
}

impl From<FederatedSubjectRecord> for BrowserFederatedSubjectView {
    fn from(record: FederatedSubjectRecord) -> Self {
        Self {
            subject_id: record.subject_id,
            issuer: record.issuer,
            subject: record.subject,
            state: record.state.as_str().to_owned(),
            canonical_user_id: record.canonical_user_id,
            actor_login: record.actor_login,
            display_name: record.display_name,
            revision: record.revision,
            first_seen_at: record.first_seen_at,
            last_seen_at: record.last_seen_at,
            updated_at: record.updated_at,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserFederatedSubjectAuditView {
    #[schema(value_type = String, format = "uuid")]
    event_id: Uuid,
    #[schema(value_type = String, format = "uuid")]
    subject_id: Uuid,
    action: String,
    actor: String,
    previous_canonical_user_id: Option<CanonicalUserId>,
    canonical_user_id: Option<CanonicalUserId>,
    previous_revision: i64,
    revision: i64,
    reason: Option<String>,
    created_at: String,
}

impl From<FederatedSubjectAuditRecord> for BrowserFederatedSubjectAuditView {
    fn from(record: FederatedSubjectAuditRecord) -> Self {
        Self {
            event_id: record.event_id,
            subject_id: record.subject_id,
            action: record.action.as_str().to_owned(),
            actor: record.actor,
            previous_canonical_user_id: record.previous_canonical_user_id,
            canonical_user_id: record.canonical_user_id,
            previous_revision: record.previous_revision,
            revision: record.revision,
            reason: record.reason,
            created_at: record.created_at,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserFederatedSubjectResponse {
    api_version: &'static str,
    federated_subject: BrowserFederatedSubjectView,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserFederatedSubjectListResponse {
    api_version: &'static str,
    federated_subjects: Vec<BrowserFederatedSubjectView>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserFederatedSubjectAuditResponse {
    api_version: &'static str,
    events: Vec<BrowserFederatedSubjectAuditView>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AssociateFederatedSubjectBody {
    expected_revision: i64,
    canonical_user_id: CanonicalUserId,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DisableFederatedSubjectBody {
    expected_revision: i64,
    reason: Option<String>,
}

#[derive(Clone)]
pub(crate) struct BrowserAdminState<R, L, D> {
    runtimes: R,
    ledger: L,
    decisions: D,
    capabilities: CapabilityCatalog,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CapabilityCatalog {
    pub schema_version: String,
    pub models: Vec<ModelRef>,
    pub tools: Vec<ToolGrant>,
}

impl CapabilityCatalog {
    pub fn from_json(value: &str) -> Result<Self, String> {
        if value.len() > MAX_CAPABILITY_CATALOG_BYTES {
            return Err("capability catalog exceeds 262144 bytes".to_owned());
        }
        let catalog: Self = serde_json::from_str(value)
            .map_err(|error| format!("invalid capability catalog JSON: {error}"))?;
        catalog.validate()?;
        Ok(catalog)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != "steward.capability-catalog/v1" {
            return Err(
                "capability catalog schemaVersion must be steward.capability-catalog/v1".to_owned(),
            );
        }
        if self.models.len() > MAX_CAPABILITY_MODELS || self.tools.len() > MAX_CAPABILITY_TOOLS {
            return Err("capability catalog exceeds its bounded model or tool count".to_owned());
        }
        let valid = |value: &str| {
            !value.is_empty()
                && value.len() <= 255
                && value.trim() == value
                && !value.chars().any(char::is_control)
        };
        if self
            .models
            .iter()
            .any(|model| !valid(&model.provider) || !valid(&model.model))
            || self.tools.iter().any(|tool| {
                !valid(&tool.provider) || !valid(&tool.resource) || !valid(&tool.action)
            })
        {
            return Err(
                "capability catalog entries must contain bounded exact identifiers".to_owned(),
            );
        }
        let mut model_keys = std::collections::BTreeSet::new();
        let mut tool_keys = std::collections::BTreeSet::new();
        if self
            .models
            .iter()
            .any(|model| !model_keys.insert((&model.provider, &model.model)))
            || self
                .tools
                .iter()
                .any(|tool| !tool_keys.insert((&tool.provider, &tool.resource, &tool.action)))
        {
            return Err("capability catalog entries must be unique".to_owned());
        }
        Ok(())
    }
}

#[cfg(test)]
mod capability_catalog_tests {
    use super::{
        CapabilityCatalog, MAX_CAPABILITY_CATALOG_BYTES, MAX_CAPABILITY_MODELS,
        MAX_CAPABILITY_TOOLS,
    };
    use steward_types::{ModelRef, ToolGrant};

    fn catalog() -> CapabilityCatalog {
        CapabilityCatalog {
            schema_version: "steward.capability-catalog/v1".to_owned(),
            models: vec![ModelRef {
                provider: "provider-a".to_owned(),
                model: "model-a".to_owned(),
            }],
            tools: vec![ToolGrant {
                provider: "github".to_owned(),
                resource: "actions_get".to_owned(),
                action: "read".to_owned(),
            }],
        }
    }

    #[test]
    fn accepts_exact_descriptive_capabilities() -> Result<(), String> {
        let catalog = catalog();
        catalog.validate()?;
        assert_eq!(
            CapabilityCatalog::from_json(
                &serde_json::to_string(&catalog).map_err(|error| error.to_string())?
            )?,
            catalog
        );
        Ok(())
    }

    #[test]
    fn rejects_unknown_fields_duplicates_and_malformed_identifiers() {
        let unknown = r#"{"schemaVersion":"steward.capability-catalog/v1","models":[],"tools":[],"budget":{}}"#;
        assert!(CapabilityCatalog::from_json(unknown).is_err());

        let mut duplicate = catalog();
        duplicate.models.push(duplicate.models[0].clone());
        assert!(duplicate.validate().is_err());

        let mut malformed = catalog();
        malformed.tools[0].resource = " actions_get".to_owned();
        assert!(malformed.validate().is_err());
    }

    #[test]
    fn rejects_oversized_documents_and_entry_sets() {
        let oversized = " ".repeat(MAX_CAPABILITY_CATALOG_BYTES + 1);
        assert!(CapabilityCatalog::from_json(&oversized).is_err());

        let mut too_many_models = catalog();
        too_many_models.models = (0..=MAX_CAPABILITY_MODELS)
            .map(|index| ModelRef {
                provider: "provider-a".to_owned(),
                model: format!("model-{index}"),
            })
            .collect();
        assert!(too_many_models.validate().is_err());

        let mut too_many_tools = catalog();
        too_many_tools.tools = (0..=MAX_CAPABILITY_TOOLS)
            .map(|index| ToolGrant {
                provider: "github".to_owned(),
                resource: format!("resource-{index}"),
                action: "read".to_owned(),
            })
            .collect();
        assert!(too_many_tools.validate().is_err());
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserEnvelopeTemplateResponse {
    api_version: &'static str,
    member_role: String,
    envelope: BrowserEnvelope,
}

#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserEnvelopeTemplateListItem {
    member_role: String,
    envelope: BrowserEnvelope,
}

#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserEnvelopeTemplateListResponse {
    api_version: &'static str,
    templates: Vec<BrowserEnvelopeTemplateListItem>,
}

#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserApprovalView {
    #[schema(value_type = String, format = "uuid")]
    approval_id: Uuid,
    runtime_uid: String,
    member_role: String,
    actor: String,
    envelope_revision: i64,
    counterexample: String,
    proposed_spec: AgentRuntimeSpec,
    decision_key: Option<String>,
    evidence_url: Option<String>,
}

impl From<PendingApproval> for BrowserApprovalView {
    fn from(approval: PendingApproval) -> Self {
        let counterexample = AdmissionDecision::Reject {
            deltas: approval.deltas,
        }
        .counterexample()
        .unwrap_or_else(|| "envelope exceeded".to_owned());
        Self {
            approval_id: approval.approval_id,
            runtime_uid: approval.runtime_uid,
            member_role: approval.member_role,
            actor: approval.actor,
            envelope_revision: approval.envelope_revision,
            counterexample,
            proposed_spec: approval.proposed_spec,
            decision_key: approval.decision_key,
            evidence_url: approval.evidence_url,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserEnvelopeRequestView {
    #[schema(value_type = String, format = "uuid")]
    request_id: Uuid,
    owner_display_email: String,
    template_id: String,
    template_revision: i64,
    requested_envelope: BrowserEnvelope,
    template_envelope: BrowserEnvelope,
    created_at: String,
}

impl From<PendingEnvelopeRequest> for BrowserEnvelopeRequestView {
    fn from(request: PendingEnvelopeRequest) -> Self {
        Self {
            request_id: request.request_id,
            owner_display_email: request.owner_display_email,
            template_id: request.template_id,
            template_revision: request.template_revision,
            requested_envelope: request.requested_envelope.into(),
            template_envelope: request.template_envelope.into(),
            created_at: request.created_at,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserEnvelopeRequestDecisionView {
    #[schema(value_type = String, format = "uuid")]
    request_id: Uuid,
    template_id: String,
    template_revision: i64,
    requested_envelope: BrowserEnvelope,
    approved_envelope: Option<BrowserEnvelope>,
    status: String,
    #[schema(value_type = Option<String>, format = "uuid")]
    approval_id: Option<Uuid>,
    envelope_instance_id: Option<String>,
    envelope_digest: Option<String>,
    reason: Option<String>,
    acted_by: String,
    status_at: String,
}

impl From<EnvelopeRequestRecord> for BrowserEnvelopeRequestDecisionView {
    fn from(request: EnvelopeRequestRecord) -> Self {
        Self {
            request_id: request.id,
            template_id: request.template_id,
            template_revision: request.template_revision,
            requested_envelope: request.requested_envelope.into(),
            approved_envelope: request.approved_envelope.map(Into::into),
            status: request.status.as_str().to_owned(),
            approval_id: request.approval_id,
            envelope_instance_id: request.envelope_instance_id,
            envelope_digest: request.envelope_digest,
            reason: request.reason,
            acted_by: request.status_actor,
            status_at: request.status_at,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserEnvelopeRequestDecisionResponse {
    api_version: &'static str,
    request: BrowserEnvelopeRequestDecisionView,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RejectEnvelopeRequestBody {
    reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserApprovalsResponse {
    api_version: &'static str,
    approvals: Vec<BrowserApprovalView>,
    envelope_requests: Vec<BrowserEnvelopeRequestView>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserDecisionReferenceResponse {
    api_version: &'static str,
    #[schema(value_type = String, format = "uuid")]
    approval_id: Uuid,
    decision_key: String,
    evidence_url: String,
}

fn inner_router<R, L, D>(
    runtimes: R,
    ledger: L,
    decisions: D,
    capabilities: CapabilityCatalog,
) -> Router
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel + Clone,
{
    Router::new()
        .route(
            "/admin/api/v1/envelope-templates",
            get(list_envelope_templates::<R, L, D>),
        )
        .route(
            "/admin/api/v1/envelope-templates/{member_role}",
            get(get_envelope_template::<R, L, D>).post(author_envelope_template::<R, L, D>),
        )
        .route(
            "/admin/api/v1/capabilities",
            get(get_capabilities::<R, L, D>),
        )
        .route("/admin/api/v1/approvals", get(list_approvals::<R, L, D>))
        .route(
            "/admin/api/v1/envelope-requests/{request_id}/approve",
            post(approve_envelope_request::<R, L, D>),
        )
        .route(
            "/admin/api/v1/envelope-requests/{request_id}/reject",
            post(reject_envelope_request::<R, L, D>),
        )
        .route(
            "/admin/api/v1/approvals/{approval_id}/approve",
            post(approve::<R, L, D>),
        )
        .route(
            "/admin/api/v1/approvals/{approval_id}/file",
            post(file_decision::<R, L, D>),
        )
        .with_state(BrowserAdminState {
            runtimes,
            ledger,
            decisions,
            capabilities,
        })
}

#[utoipa::path(
    get,
    operation_id = "getAdminCapabilities",
    path = "/admin/api/v1/capabilities",
    responses(
        (status = 200, body = CapabilityCatalog),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role is required")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn get_capabilities<R, L, D>(
    Extension(_authority): Extension<BrowserAdminAuthority>,
    State(state): State<BrowserAdminState<R, L, D>>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel + Clone,
{
    Json(state.capabilities).into_response()
}

#[utoipa::path(
    post,
    operation_id = "approveAdminEnvelopeRequest",
    path = "/admin/api/v1/envelope-requests/{request_id}/approve",
    params(
        ("request_id" = String, Path),
        ("X-Steward-CSRF" = String, Header)
    ),
    request_body = BrowserMutationRequest,
    responses(
        (status = 200, body = BrowserEnvelopeRequestDecisionResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role, origin, fetch metadata, or CSRF proof is invalid"),
        (status = 404, description = "Envelope request was not found"),
        (status = 409, description = "Envelope request or template revision is stale or already decided differently"),
        (status = 503, description = "Envelope request authority is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn approve_envelope_request<R, L, D>(
    Extension(authority): Extension<BrowserAdminAuthority>,
    Extension(_proof): Extension<BrowserMutationProof>,
    State(state): State<BrowserAdminState<R, L, D>>,
    Path(request_id): Path<Uuid>,
    Json(_request): Json<BrowserMutationRequest>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel + Clone,
{
    let request = match state.ledger.envelope_request_for_admin(request_id).await {
        Ok(Some(request)) => request,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return ApiError::Store(error).into_response(),
    };
    let instance_id = envelope_instance_id(request.id);
    let digest = match envelope_content_digest(&request.requested_envelope) {
        Ok(digest) => digest,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let approval_id = request.approval_id.unwrap_or_else(Uuid::new_v4);
    let actor = authority.principal().canonical_user_id.as_str();
    match state
        .ledger
        .append_envelope_request_status(
            request.id,
            EnvelopeRequestStatusUpdate {
                from: EnvelopeRequestStatus::Pending,
                to: EnvelopeRequestStatus::Provisioned,
                approval_id: Some(approval_id),
                envelope_instance_id: Some(&instance_id),
                envelope_digest: Some(&digest),
                reason: None,
                approved_envelope: Some(&request.requested_envelope),
                actor,
            },
        )
        .await
    {
        Ok(request) => Json(BrowserEnvelopeRequestDecisionResponse {
            api_version: BROWSER_ADMIN_API_VERSION,
            request: request.into(),
        })
        .into_response(),
        Err(error) => ApiError::Store(error).into_response(),
    }
}

#[utoipa::path(
    post,
    operation_id = "rejectAdminEnvelopeRequest",
    path = "/admin/api/v1/envelope-requests/{request_id}/reject",
    params(
        ("request_id" = String, Path),
        ("X-Steward-CSRF" = String, Header)
    ),
    request_body = RejectEnvelopeRequestBody,
    responses(
        (status = 200, body = BrowserEnvelopeRequestDecisionResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role, origin, fetch metadata, or CSRF proof is invalid"),
        (status = 404, description = "Envelope request was not found"),
        (status = 409, description = "Envelope request was already decided differently"),
        (status = 422, description = "Rejection reason is invalid"),
        (status = 503, description = "Envelope request authority is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn reject_envelope_request<R, L, D>(
    Extension(authority): Extension<BrowserAdminAuthority>,
    Extension(_proof): Extension<BrowserMutationProof>,
    State(state): State<BrowserAdminState<R, L, D>>,
    Path(request_id): Path<Uuid>,
    Json(body): Json<RejectEnvelopeRequestBody>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel + Clone,
{
    let reason = body
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if reason.is_some_and(|value| value.len() > 2_000) {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    }
    let actor = authority.principal().canonical_user_id.as_str();
    match state
        .ledger
        .append_envelope_request_status(
            request_id,
            EnvelopeRequestStatusUpdate {
                from: EnvelopeRequestStatus::Pending,
                to: EnvelopeRequestStatus::Rejected,
                approval_id: None,
                envelope_instance_id: None,
                envelope_digest: None,
                reason,
                approved_envelope: None,
                actor,
            },
        )
        .await
    {
        Ok(request) => Json(BrowserEnvelopeRequestDecisionResponse {
            api_version: BROWSER_ADMIN_API_VERSION,
            request: request.into(),
        })
        .into_response(),
        Err(StoreError::EnvelopeRequestNotFound) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => ApiError::Store(error).into_response(),
    }
}

/// Mount the administrator data plane behind the shared opaque browser-session boundary.
///
/// The middleware remains the only source of administrator authority and mutation proof. The
/// handlers reuse the same ledger, runtime repository, and decision channel as the TokenReview
/// operator routes, so Next.js never becomes an authorization or workflow authority.
pub fn protected_router<R, L, D>(
    runtimes: R,
    ledger: L,
    decisions: D,
    capabilities: CapabilityCatalog,
    browser_auth: BrowserAuthService,
) -> Router
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel + Clone,
{
    protect_browser_admin_routes(
        inner_router(runtimes, ledger, decisions, capabilities),
        browser_auth,
    )
}

/// Mount federated-subject administration behind the same browser administrator boundary.
pub fn protected_federated_subject_router(
    store: PgStore,
    browser_auth: BrowserAuthService,
) -> Router {
    let routes = Router::new()
        .route(
            "/admin/api/v1/federated-subjects",
            get(list_federated_subjects),
        )
        .route(
            "/admin/api/v1/federated-subjects/{subject_id}",
            get(get_federated_subject),
        )
        .route(
            "/admin/api/v1/federated-subjects/{subject_id}/audit",
            get(get_federated_subject_audit),
        )
        .route(
            "/admin/api/v1/federated-subjects/{subject_id}/associate",
            post(associate_federated_subject),
        )
        .route(
            "/admin/api/v1/federated-subjects/{subject_id}/replace",
            post(replace_federated_subject),
        )
        .route(
            "/admin/api/v1/federated-subjects/{subject_id}/disable",
            post(disable_federated_subject),
        )
        .with_state(FederatedSubjectAdminState { store });
    protect_browser_admin_routes(routes, browser_auth)
}

#[utoipa::path(
    get,
    operation_id = "listAdminFederatedSubjects",
    path = "/admin/api/v1/federated-subjects",
    responses(
        (status = 200, body = BrowserFederatedSubjectListResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role is required"),
        (status = 503, description = "Federated subjects are unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn list_federated_subjects(
    Extension(_authority): Extension<BrowserAdminAuthority>,
    State(state): State<FederatedSubjectAdminState>,
) -> Response {
    match state.store.list_federated_subjects(200).await {
        Ok(subjects) => Json(BrowserFederatedSubjectListResponse {
            api_version: BROWSER_ADMIN_API_VERSION,
            federated_subjects: subjects.into_iter().map(Into::into).collect(),
        })
        .into_response(),
        Err(error) => ApiError::Store(error).into_response(),
    }
}

#[utoipa::path(
    get,
    operation_id = "getAdminFederatedSubject",
    path = "/admin/api/v1/federated-subjects/{subject_id}",
    params(("subject_id" = String, Path)),
    responses(
        (status = 200, body = BrowserFederatedSubjectResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role is required"),
        (status = 404, description = "Federated subject was not found"),
        (status = 503, description = "Federated subject is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn get_federated_subject(
    Extension(_authority): Extension<BrowserAdminAuthority>,
    State(state): State<FederatedSubjectAdminState>,
    Path(subject_id): Path<Uuid>,
) -> Response {
    federated_subject_response(state.store.federated_subject(subject_id).await)
}

#[utoipa::path(
    get,
    operation_id = "getAdminFederatedSubjectAudit",
    path = "/admin/api/v1/federated-subjects/{subject_id}/audit",
    params(("subject_id" = String, Path)),
    responses(
        (status = 200, body = BrowserFederatedSubjectAuditResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role is required"),
        (status = 404, description = "Federated subject was not found"),
        (status = 503, description = "Federated-subject audit is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn get_federated_subject_audit(
    Extension(_authority): Extension<BrowserAdminAuthority>,
    State(state): State<FederatedSubjectAdminState>,
    Path(subject_id): Path<Uuid>,
) -> Response {
    match state.store.federated_subject_audit(subject_id).await {
        Ok(events) => Json(BrowserFederatedSubjectAuditResponse {
            api_version: BROWSER_ADMIN_API_VERSION,
            events: events.into_iter().map(Into::into).collect(),
        })
        .into_response(),
        Err(error) => ApiError::Store(error).into_response(),
    }
}

#[utoipa::path(
    post,
    operation_id = "associateAdminFederatedSubject",
    path = "/admin/api/v1/federated-subjects/{subject_id}/associate",
    params(("subject_id" = String, Path), ("X-Steward-CSRF" = String, Header)),
    request_body = AssociateFederatedSubjectBody,
    responses(
        (status = 200, body = BrowserFederatedSubjectResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role, origin, fetch metadata, or CSRF proof is invalid"),
        (status = 404, description = "Federated subject or canonical user was not found"),
        (status = 409, description = "Subject state or revision conflicts"),
        (status = 422, description = "Association request is invalid"),
        (status = 503, description = "Federated subject is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn associate_federated_subject(
    Extension(authority): Extension<BrowserAdminAuthority>,
    Extension(_proof): Extension<BrowserMutationProof>,
    State(state): State<FederatedSubjectAdminState>,
    Path(subject_id): Path<Uuid>,
    Json(body): Json<AssociateFederatedSubjectBody>,
) -> Response {
    federated_subject_response(
        state
            .store
            .associate_federated_subject(FederatedSubjectAssociation {
                subject_id,
                expected_revision: body.expected_revision,
                canonical_user_id: &body.canonical_user_id,
                actor: authority.principal().canonical_user_id.as_str(),
            })
            .await,
    )
}

#[utoipa::path(
    post,
    operation_id = "replaceAdminFederatedSubjectAssociation",
    path = "/admin/api/v1/federated-subjects/{subject_id}/replace",
    params(("subject_id" = String, Path), ("X-Steward-CSRF" = String, Header)),
    request_body = AssociateFederatedSubjectBody,
    responses(
        (status = 200, body = BrowserFederatedSubjectResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role, origin, fetch metadata, or CSRF proof is invalid"),
        (status = 404, description = "Federated subject or canonical user was not found"),
        (status = 409, description = "Subject state or revision conflicts"),
        (status = 422, description = "Replacement request is invalid"),
        (status = 503, description = "Federated subject is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn replace_federated_subject(
    Extension(authority): Extension<BrowserAdminAuthority>,
    Extension(_proof): Extension<BrowserMutationProof>,
    State(state): State<FederatedSubjectAdminState>,
    Path(subject_id): Path<Uuid>,
    Json(body): Json<AssociateFederatedSubjectBody>,
) -> Response {
    federated_subject_response(
        state
            .store
            .replace_federated_subject_association(FederatedSubjectAssociation {
                subject_id,
                expected_revision: body.expected_revision,
                canonical_user_id: &body.canonical_user_id,
                actor: authority.principal().canonical_user_id.as_str(),
            })
            .await,
    )
}

#[utoipa::path(
    post,
    operation_id = "disableAdminFederatedSubject",
    path = "/admin/api/v1/federated-subjects/{subject_id}/disable",
    params(("subject_id" = String, Path), ("X-Steward-CSRF" = String, Header)),
    request_body = DisableFederatedSubjectBody,
    responses(
        (status = 200, body = BrowserFederatedSubjectResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role, origin, fetch metadata, or CSRF proof is invalid"),
        (status = 404, description = "Federated subject was not found"),
        (status = 409, description = "Subject revision conflicts"),
        (status = 422, description = "Disable request is invalid"),
        (status = 503, description = "Federated subject is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn disable_federated_subject(
    Extension(authority): Extension<BrowserAdminAuthority>,
    Extension(_proof): Extension<BrowserMutationProof>,
    State(state): State<FederatedSubjectAdminState>,
    Path(subject_id): Path<Uuid>,
    Json(body): Json<DisableFederatedSubjectBody>,
) -> Response {
    federated_subject_response(
        state
            .store
            .disable_federated_subject(FederatedSubjectDisable {
                subject_id,
                expected_revision: body.expected_revision,
                actor: authority.principal().canonical_user_id.as_str(),
                reason: body.reason.as_deref(),
            })
            .await,
    )
}

fn federated_subject_response(result: Result<FederatedSubjectRecord, StoreError>) -> Response {
    match result {
        Ok(record) => Json(BrowserFederatedSubjectResponse {
            api_version: BROWSER_ADMIN_API_VERSION,
            federated_subject: record.into(),
        })
        .into_response(),
        Err(error) => ApiError::Store(error).into_response(),
    }
}

#[utoipa::path(
    get,
    operation_id = "listAdminEnvelopeTemplates",
    path = "/admin/api/v1/envelope-templates",
    responses(
        (status = 200, body = BrowserEnvelopeTemplateListResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role is required"),
        (status = 503, description = "Envelope templates are unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn list_envelope_templates<R, L, D>(
    Extension(_authority): Extension<BrowserAdminAuthority>,
    State(state): State<BrowserAdminState<R, L, D>>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel + Clone,
{
    match state.ledger.latest_envelopes().await {
        Ok(templates) => Json(BrowserEnvelopeTemplateListResponse {
            api_version: BROWSER_ADMIN_API_VERSION,
            templates: templates
                .into_iter()
                .map(|(member_role, envelope)| BrowserEnvelopeTemplateListItem {
                    member_role,
                    envelope: envelope.into(),
                })
                .collect(),
        })
        .into_response(),
        Err(error) => ApiError::Store(error).into_response(),
    }
}

#[utoipa::path(
    get,
    operation_id = "getAdminEnvelopeTemplate",
    path = "/admin/api/v1/envelope-templates/{member_role}",
    params(("member_role" = String, Path)),
    responses(
        (status = 200, body = BrowserEnvelopeTemplateResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role is required"),
        (status = 404, description = "Envelope template was not found"),
        (status = 503, description = "Envelope templates are unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn get_envelope_template<R, L, D>(
    Extension(_authority): Extension<BrowserAdminAuthority>,
    State(state): State<BrowserAdminState<R, L, D>>,
    Path(member_role): Path<String>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel + Clone,
{
    match state.ledger.latest_envelope(&member_role).await {
        Ok(Some(envelope)) => Json(BrowserEnvelopeTemplateResponse {
            api_version: BROWSER_ADMIN_API_VERSION,
            member_role,
            envelope: envelope.into(),
        })
        .into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => ApiError::Store(error).into_response(),
    }
}

#[utoipa::path(
    post,
    operation_id = "authorAdminEnvelopeTemplate",
    path = "/admin/api/v1/envelope-templates/{member_role}",
    params(
        ("member_role" = String, Path),
        ("X-Steward-CSRF" = String, Header)
    ),
    request_body = BrowserEnvelope,
    responses(
        (status = 201, body = BrowserEnvelopeTemplateResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role, origin, fetch metadata, or CSRF proof is invalid"),
        (status = 409, description = "Envelope revision is not newer than the current revision"),
        (status = 422, description = "Member role, envelope, or deployed capability selection is invalid"),
        (status = 503, description = "Envelope templates are unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn author_envelope_template<R, L, D>(
    Extension(authority): Extension<BrowserAdminAuthority>,
    Extension(_proof): Extension<BrowserMutationProof>,
    State(state): State<BrowserAdminState<R, L, D>>,
    Path(member_role): Path<String>,
    Json(browser_envelope): Json<BrowserEnvelope>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel + Clone,
{
    let envelope: Envelope = browser_envelope.into();
    if member_role.is_empty()
        || envelope.revision <= 0
        || envelope.spec.llms.is_empty()
        || validate_envelope(&envelope).is_err()
    {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    }
    match state.ledger.latest_envelope(&member_role).await {
        Ok(Some(current)) if envelope.revision <= current.revision => {
            return StatusCode::CONFLICT.into_response();
        }
        Ok(_) => {}
        Err(error) => return ApiError::Store(error).into_response(),
    }
    if state.capabilities.models.is_empty()
        || envelope
            .spec
            .llms
            .iter()
            .any(|model| !state.capabilities.models.contains(model))
        || envelope
            .spec
            .tools
            .iter()
            .any(|tool| !state.capabilities.tools.contains(tool))
    {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    }
    match state
        .ledger
        .insert_envelope(
            &member_role,
            &envelope,
            authority.principal().canonical_user_id.as_str(),
        )
        .await
    {
        Ok(()) => (
            StatusCode::CREATED,
            Json(BrowserEnvelopeTemplateResponse {
                api_version: BROWSER_ADMIN_API_VERSION,
                member_role,
                envelope: envelope.into(),
            }),
        )
            .into_response(),
        Err(StoreError::EnvelopeRevisionNotIncreasing) => StatusCode::CONFLICT.into_response(),
        Err(error) => ApiError::Store(error).into_response(),
    }
}

#[utoipa::path(
    get,
    operation_id = "listAdminApprovals",
    path = "/admin/api/v1/approvals",
    responses(
        (status = 200, body = BrowserApprovalsResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role is required"),
        (status = 503, description = "Approvals are unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn list_approvals<R, L, D>(
    Extension(_authority): Extension<BrowserAdminAuthority>,
    State(state): State<BrowserAdminState<R, L, D>>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel + Clone,
{
    let approvals = match state.ledger.pending_approvals().await {
        Ok(approvals) => approvals,
        Err(error) => return ApiError::Store(error).into_response(),
    };
    match state.ledger.pending_envelope_requests().await {
        Ok(envelope_requests) => Json(BrowserApprovalsResponse {
            api_version: BROWSER_ADMIN_API_VERSION,
            approvals: approvals.into_iter().map(Into::into).collect(),
            envelope_requests: envelope_requests.into_iter().map(Into::into).collect(),
        })
        .into_response(),
        Err(error) => ApiError::Store(error).into_response(),
    }
}

#[utoipa::path(
    post,
    operation_id = "approveAdminApproval",
    path = "/admin/api/v1/approvals/{approval_id}/approve",
    params(
        ("approval_id" = String, Path),
        ("X-Steward-CSRF" = String, Header)
    ),
    request_body = ApprovalRequest,
    responses(
        (status = 204, description = "Approval was applied through the existing governed approval path"),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role, origin, fetch metadata, or CSRF proof is invalid"),
        (status = 404, description = "Approval was not found"),
        (status = 409, description = "Approval or its bound runtime is stale"),
        (status = 422, description = "Approval evidence is invalid"),
        (status = 503, description = "Approval authority is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn approve<R, L, D>(
    Extension(authority): Extension<BrowserAdminAuthority>,
    Extension(_proof): Extension<BrowserMutationProof>,
    State(state): State<BrowserAdminState<R, L, D>>,
    Path(approval_id): Path<Uuid>,
    Json(request): Json<ApprovalRequest>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel + Clone,
{
    let admin = AdminContext {
        actor: authority.principal().canonical_user_id.as_str().to_owned(),
    };
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
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => error.into_response(),
    }
}

#[utoipa::path(
    post,
    operation_id = "fileAdminApprovalDecision",
    path = "/admin/api/v1/approvals/{approval_id}/file",
    params(
        ("approval_id" = String, Path),
        ("X-Steward-CSRF" = String, Header)
    ),
    request_body = BrowserMutationRequest,
    responses(
        (status = 200, body = BrowserDecisionReferenceResponse),
        (status = 400, description = "Mutation JSON is malformed"),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role, origin, fetch metadata, or CSRF proof is invalid"),
        (status = 404, description = "Approval was not found"),
        (status = 409, description = "Decision filing is already active or conflicts"),
        (status = 503, description = "Decision filing is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn file_decision<R, L, D>(
    Extension(_authority): Extension<BrowserAdminAuthority>,
    Extension(_proof): Extension<BrowserMutationProof>,
    State(state): State<BrowserAdminState<R, L, D>>,
    Path(approval_id): Path<Uuid>,
    Json(_request): Json<BrowserMutationRequest>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel + Clone,
{
    match file_decision_reference(&state.ledger, &state.decisions, approval_id).await {
        Ok(reference) => Json(BrowserDecisionReferenceResponse {
            api_version: BROWSER_ADMIN_API_VERSION,
            approval_id,
            decision_key: reference.key,
            evidence_url: reference.evidence_url,
        })
        .into_response(),
        Err(error) => error.into_response(),
    }
}
