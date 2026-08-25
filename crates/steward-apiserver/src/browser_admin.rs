//! Browser-session administrator APIs backed by Steward's existing authority paths.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::Serialize;
use steward_admission::{AdmissionDecision, Envelope, validate_envelope};
use steward_store::{PendingApproval, PendingEnvelopeRequest, StoreError};
use steward_types::AgentRuntimeSpec;
use uuid::Uuid;

use crate::browser_auth::{
    BrowserAdminAuthority, BrowserAuthService, BrowserMutationProof, BrowserMutationRequest,
    protect_browser_admin_routes,
};
use crate::user_envelopes::BrowserEnvelope;
use crate::{
    AdminContext, AdmissionLedger, ApiError, ApprovalRequest, DecisionChannel, RuntimeRepository,
    approve_parked_request, file_decision_reference,
};

const BROWSER_ADMIN_API_VERSION: &str = "steward.browser-admin/v1";

#[derive(Clone)]
pub(crate) struct BrowserAdminState<R, L, D> {
    runtimes: R,
    ledger: L,
    decisions: D,
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
            created_at: request.created_at,
        }
    }
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

fn inner_router<R, L, D>(runtimes: R, ledger: L, decisions: D) -> Router
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
        .route("/admin/api/v1/approvals", get(list_approvals::<R, L, D>))
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
        })
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
    browser_auth: BrowserAuthService,
) -> Router
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel + Clone,
{
    protect_browser_admin_routes(inner_router(runtimes, ledger, decisions), browser_auth)
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
        (status = 422, description = "Member role or envelope is invalid"),
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
    if member_role.is_empty() || envelope.revision <= 0 || validate_envelope(&envelope).is_err() {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    }
    match state.ledger.latest_envelope(&member_role).await {
        Ok(Some(current)) if envelope.revision <= current.revision => {
            return StatusCode::CONFLICT.into_response();
        }
        Ok(_) => {}
        Err(error) => return ApiError::Store(error).into_response(),
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
