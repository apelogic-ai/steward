//! Browser-session administrator APIs backed by Steward's existing authority paths.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use steward_admission::{
    AdmissionDecision, AdmissionDelta, Envelope, add_budget_amount, envelope_is_within,
    validate_envelope,
};
use steward_store::{
    AdminApprovalRecord, AdminEnvelopeRequestRecord, CumulativeEscalationRecord,
    EnvelopeRequestRecord, EnvelopeRequestStatus, EnvelopeRequestStatusUpdate,
    EnvelopeTemplatePublication, EnvelopeTemplateRevisionRecord,
    FederatedSubjectAssociation, FederatedSubjectAuditRecord, FederatedSubjectDisable,
    FederatedSubjectRecord, PendingApproval, PendingEnvelopeRequest, PgStore, StoreError,
};
use steward_types::direct_package::DirectAdmissionDelta;
use steward_types::{AgentRuntimeSpec, CanonicalUserId, ModelRef, ToolGrant};
use uuid::Uuid;

use crate::browser_auth::{
    BrowserAdminAuthority, BrowserAuthService, BrowserMutationProof, BrowserMutationRequest,
    protect_browser_admin_routes,
};
use crate::user_envelopes::{
    BrowserEnvelope, BrowserEnvelopeSpec, envelope_content_digest, envelope_instance_id,
};
use crate::{
    AdminContext, AdmissionLedger, ApiError, ApprovalRequest, DecisionChannel, RuntimeRepository,
    approve_parked_request, file_decision_reference,
};

const BROWSER_ADMIN_API_VERSION: &str = "steward.browser-admin/v1";
pub const MAX_CAPABILITY_CATALOG_BYTES: usize = 256 * 1024;
const MAX_CAPABILITY_MODELS: usize = 256;
const MAX_CAPABILITY_TOOLS: usize = 1024;
const MAX_CAPABILITY_CATALOGS: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolAccessClass {
    Read,
    Write,
    Destructive,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CapabilityTool {
    pub provider: String,
    pub resource: String,
    pub action: String,
    pub access_class: ToolAccessClass,
}

impl CapabilityTool {
    fn grants(&self, requested: &ToolGrant) -> bool {
        self.provider == requested.provider
            && self.resource == requested.resource
            && self.action == requested.action
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CapabilityProviderCatalog {
    pub provider: String,
    pub catalog_id: String,
    pub version: String,
    pub available: bool,
}

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

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, utoipa::IntoParams)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct FederatedSubjectListQuery {
    /// Exact trusted token issuer. Must be supplied together with `subject`.
    issuer: Option<String>,
    /// Exact authenticated subject. Must be supplied together with `issuer`.
    subject: Option<String>,
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
    pub tools: Vec<CapabilityTool>,
    pub catalogs: Vec<CapabilityProviderCatalog>,
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
        if self.schema_version != "steward.capability-catalog/v2" {
            return Err(
                "capability catalog schemaVersion must be steward.capability-catalog/v2".to_owned(),
            );
        }
        if self.models.len() > MAX_CAPABILITY_MODELS
            || self.tools.len() > MAX_CAPABILITY_TOOLS
            || self.catalogs.len() > MAX_CAPABILITY_CATALOGS
        {
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
            || self.catalogs.iter().any(|catalog| {
                !valid(&catalog.provider) || !valid(&catalog.catalog_id) || !valid(&catalog.version)
            })
        {
            return Err(
                "capability catalog entries must contain bounded exact identifiers".to_owned(),
            );
        }
        let mut model_keys = std::collections::BTreeSet::new();
        let mut tool_keys = std::collections::BTreeSet::new();
        let mut catalog_keys = std::collections::BTreeSet::new();
        if self
            .models
            .iter()
            .any(|model| !model_keys.insert((&model.provider, &model.model)))
            || self
                .tools
                .iter()
                .any(|tool| !tool_keys.insert((&tool.provider, &tool.resource, &tool.action)))
            || self
                .catalogs
                .iter()
                .any(|catalog| !catalog_keys.insert((&catalog.provider, &catalog.catalog_id)))
        {
            return Err("capability catalog entries must be unique".to_owned());
        }
        Ok(())
    }
}

#[cfg(test)]
mod capability_catalog_tests {
    use super::{
        CapabilityCatalog, CapabilityProviderCatalog, CapabilityTool, MAX_CAPABILITY_CATALOG_BYTES,
        MAX_CAPABILITY_MODELS, MAX_CAPABILITY_TOOLS, ToolAccessClass,
    };
    use steward_types::ModelRef;

    fn catalog() -> CapabilityCatalog {
        CapabilityCatalog {
            schema_version: "steward.capability-catalog/v2".to_owned(),
            models: vec![ModelRef {
                provider: "provider-a".to_owned(),
                model: "model-a".to_owned(),
            }],
            tools: vec![CapabilityTool {
                provider: "github".to_owned(),
                resource: "actions_get".to_owned(),
                action: "read".to_owned(),
                access_class: ToolAccessClass::Read,
            }],
            catalogs: vec![CapabilityProviderCatalog {
                provider: "github".to_owned(),
                catalog_id: "github-tools".to_owned(),
                version: "1.6.0".to_owned(),
                available: true,
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
        let unknown = r#"{"schemaVersion":"steward.capability-catalog/v2","models":[],"tools":[],"catalogs":[],"budget":{}}"#;
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
            .map(|index| CapabilityTool {
                provider: "github".to_owned(),
                resource: format!("resource-{index}"),
                action: "read".to_owned(),
                access_class: ToolAccessClass::Read,
            })
            .collect();
        assert!(too_many_tools.validate().is_err());
    }

    #[test]
    fn capability_catalog_requires_access_classes_and_provider_availability() {
        let value = r#"{
          "schemaVersion":"steward.capability-catalog/v2",
          "models":[{"provider":"provider-a","model":"model-a"}],
          "tools":[{
            "provider":"github",
            "resource":"actions_get",
            "action":"read",
            "accessClass":"read"
          }],
          "catalogs":[{
            "provider":"github",
            "catalogId":"github-tools",
            "version":"1.6.0",
            "available":true
          }]
        }"#;
        let parsed = CapabilityCatalog::from_json(value)
            .expect("the v2 catalog should carry presentation metadata from the backend");
        let serialized = serde_json::to_value(parsed).expect("serialize capability catalog");
        assert_eq!(
            serialized.pointer("/tools/0/accessClass"),
            Some(&serde_json::json!("read"))
        );
        assert_eq!(
            serialized.pointer("/catalogs/0/version"),
            Some(&serde_json::json!("1.6.0"))
        );
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserEnvelopeTemplateResponse {
    api_version: &'static str,
    id: String,
    display_name: String,
    member_roles: Vec<String>,
    envelope: BrowserEnvelope,
    auto_provision_threshold: Option<BrowserEnvelope>,
}

#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserEnvelopeTemplateListItem {
    id: String,
    display_name: String,
    member_roles: Vec<String>,
    envelope: BrowserEnvelope,
    auto_provision_threshold: Option<BrowserEnvelope>,
}

#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserEnvelopeTemplateListResponse {
    api_version: &'static str,
    templates: Vec<BrowserEnvelopeTemplateListItem>,
}

#[derive(Clone, Debug, PartialEq, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AuthorEnvelopeTemplateBody {
    display_name: String,
    member_roles: Vec<String>,
    envelope: BrowserEnvelope,
    auto_provision_threshold: Option<BrowserEnvelope>,
}

impl From<EnvelopeTemplateRevisionRecord> for BrowserEnvelopeTemplateListItem {
    fn from(template: EnvelopeTemplateRevisionRecord) -> Self {
        Self {
            id: template.template_id,
            display_name: template.display_name,
            member_roles: template.member_roles,
            envelope: template.ceiling.into(),
            auto_provision_threshold: template.auto_provision_threshold.map(Into::into),
        }
    }
}

fn template_response(template: EnvelopeTemplateRevisionRecord) -> BrowserEnvelopeTemplateResponse {
    BrowserEnvelopeTemplateResponse {
        api_version: BROWSER_ADMIN_API_VERSION,
        id: template.template_id,
        display_name: template.display_name,
        member_roles: template.member_roles,
        envelope: template.ceiling.into(),
        auto_provision_threshold: template.auto_provision_threshold.map(Into::into),
    }
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
    rationale: Option<String>,
    evidence_url: Option<String>,
    decision_key: Option<String>,
    expires_at: Option<String>,
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
            rationale: request.rationale,
            evidence_url: request.evidence_url,
            decision_key: request.decision_key,
            expires_at: request.expires_at,
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserEnvelopeRequestDecisionReferenceResponse {
    api_version: &'static str,
    #[schema(value_type = String, format = "uuid")]
    request_id: Uuid,
    decision_key: String,
    evidence_url: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RejectEnvelopeRequestBody {
    reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ApproveEnvelopeRequestBody {
    rationale: String,
    evidence_url: Option<String>,
    expires_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserApprovalsResponse {
    api_version: &'static str,
    approvals: Vec<BrowserApprovalView>,
    envelope_requests: Vec<BrowserEnvelopeRequestView>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AdminRequestKind {
    CeilingExceeded,
    CumulativeExhausted,
    WithinCeiling,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AdminRequestSource {
    EnvelopeRequest,
    RuntimeException,
    Escalation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AdminRequestState {
    Requested,
    Escalated,
    AutoApproved,
    Approved,
    Rejected,
    Expired,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AdminRequestRequester {
    user_id: String,
    display_email: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AdminRequestTemplate {
    id: String,
    display_name: String,
    revision: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AdminRequestDecision {
    rationale: Option<String>,
    evidence_url: Option<String>,
    decision_key: Option<String>,
    expires_at: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AdminRequestHistoryEvent {
    state: AdminRequestState,
    at: String,
    actor: String,
    reason: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EscalationDimension {
    LlmSpend,
    RuntimeMinutes,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EscalationPeriod {
    start: String,
    end: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EscalationMeter {
    dimension: EscalationDimension,
    used: String,
    limit: String,
    unit: String,
    observed_at: String,
    exhausted: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EscalationView {
    envelope_instance_id: String,
    period: EscalationPeriod,
    meters: Vec<EscalationMeter>,
    #[schema(value_type = String, format = "uuid")]
    blocked_task_uid: Uuid,
    parked_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct EscalationTopUpRequest {
    dimension: EscalationDimension,
    amount: String,
    valid_until: String,
    rationale: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct EscalationDenyRequest {
    rationale: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AdminRequestView {
    id: String,
    kind: AdminRequestKind,
    source: AdminRequestSource,
    state: AdminRequestState,
    requester: AdminRequestRequester,
    template: AdminRequestTemplate,
    created_at: String,
    state_at: String,
    state_actor: String,
    #[schema(value_type = Vec<DirectAdmissionDelta>)]
    deltas: Vec<AdmissionDelta>,
    requested_envelope: Option<BrowserEnvelope>,
    template_envelope: Option<BrowserEnvelope>,
    escalation: Option<EscalationView>,
    decision: Option<AdminRequestDecision>,
    history: Vec<AdminRequestHistoryEvent>,
}

#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AdminRequestsResponse {
    api_version: &'static str,
    requests: Vec<AdminRequestView>,
    next_cursor: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AdminRequestResponse {
    api_version: &'static str,
    request: AdminRequestView,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AdminRequestsSummaryResponse {
    api_version: &'static str,
    needs_action: usize,
    escalated: usize,
    requested: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AdminRequestsQuery {
    state: Option<String>,
    kind: Option<String>,
    cursor: Option<String>,
    limit: Option<usize>,
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
            "/admin/api/v1/envelope-templates/{template_id}",
            get(get_envelope_template::<R, L, D>)
                .post(author_legacy_envelope_template::<R, L, D>)
                .put(author_envelope_template::<R, L, D>),
        )
        .route(
            "/admin/api/v1/capabilities",
            get(get_capabilities::<R, L, D>),
        )
        .route("/admin/api/v1/approvals", get(list_approvals::<R, L, D>))
        .route(
            "/admin/api/v1/requests",
            get(list_admin_requests::<R, L, D>),
        )
        .route(
            "/admin/api/v1/requests/summary",
            get(admin_requests_summary::<R, L, D>),
        )
        .route(
            "/admin/api/v1/requests/{request_id}",
            get(get_admin_request::<R, L, D>),
        )
        .route(
            "/admin/api/v1/envelope-requests/{request_id}/approve",
            post(approve_envelope_request::<R, L, D>),
        )
        .route(
            "/admin/api/v1/envelope-requests/{request_id}/reject",
            post(reject_envelope_request::<R, L, D>),
        )
        .route(
            "/admin/api/v1/envelope-requests/{request_id}/file",
            post(file_envelope_request::<R, L, D>),
        )
        .route(
            "/admin/api/v1/approvals/{approval_id}/approve",
            post(approve::<R, L, D>),
        )
        .route(
            "/admin/api/v1/approvals/{approval_id}/file",
            post(file_decision::<R, L, D>),
        )
        .route(
            "/admin/api/v1/escalations/{escalation_id}/top-up",
            post(top_up_escalation::<R, L, D>),
        )
        .route(
            "/admin/api/v1/escalations/{escalation_id}/deny",
            post(deny_escalation::<R, L, D>),
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
    request_body = ApproveEnvelopeRequestBody,
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
    Json(body): Json<ApproveEnvelopeRequestBody>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel + Clone,
{
    let rationale = body.rationale.trim();
    if rationale.is_empty() || rationale.len() > 2_000 {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    }
    let request = match state.ledger.envelope_request_for_admin(request_id).await {
        Ok(Some(request)) => request,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return ApiError::Store(error).into_response(),
    };
    if body
        .evidence_url
        .as_ref()
        .zip(request.evidence_url.as_ref())
        .is_some_and(|(provided, filed)| provided != filed)
    {
        return StatusCode::CONFLICT.into_response();
    }
    let evidence_url = body
        .evidence_url
        .as_deref()
        .or(request.evidence_url.as_deref());
    if evidence_url.is_some_and(|value| !valid_https_url(value)) {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    }
    let instance_id = envelope_instance_id(request.id);
    let digest = match envelope_content_digest(&request.requested_envelope) {
        Ok(digest) => digest,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let approval_id = request.approval_id.unwrap_or_else(Uuid::new_v4);
    let actor = authority.principal().canonical_user_id.as_str();
    let decision_key = request.decision_key.clone();
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
                rationale: Some(rationale),
                evidence_url,
                expires_at: body.expires_at.as_deref(),
                approved_envelope: Some(&request.requested_envelope),
                actor,
            },
        )
        .await
    {
        Ok(request) => {
            if let (Some(key), Some(evidence_url)) = (decision_key, evidence_url) {
                if let Err(error) = state
                    .decisions
                    .record_resolution(&steward_ports::DecisionResolution {
                        request_id: request_id.to_string(),
                        key,
                        decided_by: actor.to_owned(),
                        rationale: rationale.to_owned(),
                        evidence_url: evidence_url.to_owned(),
                    })
                    .await
                {
                    return ApiError::DecisionChannel(format!("{error:?}")).into_response();
                }
            }
            Json(BrowserEnvelopeRequestDecisionResponse {
                api_version: BROWSER_ADMIN_API_VERSION,
                request: request.into(),
            })
            .into_response()
        }
        Err(error) => ApiError::Store(error).into_response(),
    }
}

fn valid_https_url(value: &str) -> bool {
    value.len() <= 2_048
        && reqwest::Url::parse(value).is_ok_and(|url| {
            url.scheme() == "https"
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
        })
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
                rationale: None,
                evidence_url: None,
                expires_at: None,
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

#[utoipa::path(
    post,
    operation_id = "fileAdminEnvelopeRequest",
    path = "/admin/api/v1/envelope-requests/{request_id}/file",
    params(
        ("request_id" = String, Path, format = "uuid"),
        ("X-Steward-CSRF" = String, Header)
    ),
    request_body = BrowserMutationRequest,
    responses(
        (status = 200, body = BrowserEnvelopeRequestDecisionReferenceResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role, origin, fetch metadata, or CSRF proof is invalid"),
        (status = 404, description = "Envelope request was not found"),
        (status = 409, description = "Envelope request is no longer governed by the current template revision"),
        (status = 422, description = "Envelope request does not exceed its template ceiling"),
        (status = 503, description = "Envelope request or decision channel is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn file_envelope_request<R, L, D>(
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
    match state
        .ledger
        .envelope_request_decision_reference(request_id)
        .await
    {
        Ok(Some(reference)) => {
            return Json(BrowserEnvelopeRequestDecisionReferenceResponse {
                api_version: BROWSER_ADMIN_API_VERSION,
                request_id,
                decision_key: reference.decision_key,
                evidence_url: reference.evidence_url,
            })
            .into_response();
        }
        Ok(None) => {}
        Err(error) => return ApiError::Store(error).into_response(),
    }
    let request = match state.ledger.envelope_request_for_admin(request_id).await {
        Ok(Some(request)) => request,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return ApiError::Store(error).into_response(),
    };
    let template = match state
        .ledger
        .latest_envelope_template(&request.template_id)
        .await
    {
        Ok(Some(template)) if template.ceiling.revision == request.template_revision => template,
        Ok(Some(_)) => return StatusCode::CONFLICT.into_response(),
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return ApiError::Store(error).into_response(),
    };
    let counterexample =
        match steward_admission::envelope_is_within(&request.requested_envelope, &template.ceiling)
        {
            Ok(AdmissionDecision::Reject { deltas }) => AdmissionDecision::Reject { deltas }
                .counterexample()
                .unwrap_or_else(|| "Envelope request exceeds its template ceiling".to_owned()),
            Ok(AdmissionDecision::Admit) => {
                return StatusCode::UNPROCESSABLE_ENTITY.into_response();
            }
            Err(_) => return StatusCode::UNPROCESSABLE_ENTITY.into_response(),
        };
    let reference = match state
        .decisions
        .request(&steward_ports::DecisionRequest {
            request_id: request_id.to_string(),
            runtime_uid: format!("envelope-request/{request_id}"),
            actor: request.owner_user_id.as_str().to_owned(),
            member_role: request.template_id.clone(),
            counterexample,
        })
        .await
    {
        Ok(reference) => reference,
        Err(error) => return ApiError::DecisionChannel(format!("{error:?}")).into_response(),
    };
    if let Err(error) = state
        .ledger
        .link_envelope_request_decision_reference(
            request_id,
            &reference.key,
            &reference.evidence_url,
            authority.principal().canonical_user_id.as_str(),
        )
        .await
    {
        return ApiError::Store(error).into_response();
    }
    Json(BrowserEnvelopeRequestDecisionReferenceResponse {
        api_version: BROWSER_ADMIN_API_VERSION,
        request_id,
        decision_key: reference.key,
        evidence_url: reference.evidence_url,
    })
    .into_response()
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
    params(FederatedSubjectListQuery),
    responses(
        (status = 200, body = BrowserFederatedSubjectListResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role is required"),
        (status = 422, description = "Exact lookup parameters are incomplete or invalid"),
        (status = 503, description = "Federated subjects are unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn list_federated_subjects(
    Extension(_authority): Extension<BrowserAdminAuthority>,
    State(state): State<FederatedSubjectAdminState>,
    Query(query): Query<FederatedSubjectListQuery>,
) -> Response {
    let subjects = match (query.issuer.as_deref(), query.subject.as_deref()) {
        (None, None) => state.store.list_federated_subjects(200).await,
        (Some(issuer), Some(subject)) => state
            .store
            .federated_subject_by_external_identity(issuer, subject)
            .await
            .map(|record| record.into_iter().collect()),
        _ => Err(StoreError::InvalidFederatedSubject),
    };
    match subjects {
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
    match state.ledger.latest_envelope_templates().await {
        Ok(templates) => Json(BrowserEnvelopeTemplateListResponse {
            api_version: BROWSER_ADMIN_API_VERSION,
            templates: templates.into_iter().map(Into::into).collect(),
        })
        .into_response(),
        Err(error) => ApiError::Store(error).into_response(),
    }
}

#[utoipa::path(
    get,
    operation_id = "getAdminEnvelopeTemplate",
    path = "/admin/api/v1/envelope-templates/{template_id}",
    params(("template_id" = String, Path)),
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
    Path(template_id): Path<String>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel + Clone,
{
    match state.ledger.latest_envelope_template(&template_id).await {
        Ok(Some(template)) => Json(template_response(template)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => ApiError::Store(error).into_response(),
    }
}

#[utoipa::path(
    put,
    operation_id = "authorAdminEnvelopeTemplate",
    path = "/admin/api/v1/envelope-templates/{template_id}",
    params(
        ("template_id" = String, Path),
        ("X-Steward-CSRF" = String, Header)
    ),
    request_body = AuthorEnvelopeTemplateBody,
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
    Path(template_id): Path<String>,
    Json(body): Json<AuthorEnvelopeTemplateBody>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel + Clone,
{
    let envelope: Envelope = body.envelope.into();
    let auto_provision_threshold = body.auto_provision_threshold.map(Into::into);
    if !valid_template_authoring(
        &template_id,
        &body.display_name,
        &body.member_roles,
        &envelope,
        auto_provision_threshold.as_ref(),
    ) {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    }
    match state.ledger.latest_envelope_template(&template_id).await {
        Ok(Some(current)) if envelope.revision <= current.ceiling.revision => {
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
        || envelope.spec.tools.iter().any(|tool| {
            !state
                .capabilities
                .tools
                .iter()
                .any(|available| available.grants(tool))
        })
    {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    }
    match state
        .ledger
        .insert_envelope_template_revision(EnvelopeTemplatePublication {
            template_id: &template_id,
            display_name: &body.display_name,
            member_roles: &body.member_roles,
            ceiling: &envelope,
            auto_provision_threshold: auto_provision_threshold.as_ref(),
            authored_by: authority.principal().canonical_user_id.as_str(),
        })
        .await
    {
        Ok(()) => (
            StatusCode::CREATED,
            Json(BrowserEnvelopeTemplateResponse {
                api_version: BROWSER_ADMIN_API_VERSION,
                id: template_id,
                display_name: body.display_name,
                member_roles: body.member_roles,
                envelope: envelope.into(),
                auto_provision_threshold: auto_provision_threshold.map(Into::into),
            }),
        )
            .into_response(),
        Err(StoreError::EnvelopeRevisionNotIncreasing) => StatusCode::CONFLICT.into_response(),
        Err(StoreError::InvalidEnvelopeTemplate) => {
            StatusCode::UNPROCESSABLE_ENTITY.into_response()
        }
        Err(error) => ApiError::Store(error).into_response(),
    }
}

#[utoipa::path(
    post,
    operation_id = "authorLegacyAdminEnvelopeTemplate",
    path = "/admin/api/v1/envelope-templates/{template_id}",
    params(
        ("template_id" = String, Path),
        ("X-Steward-CSRF" = String, Header)
    ),
    request_body = BrowserEnvelope,
    responses(
        (status = 201, body = BrowserEnvelopeTemplateResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role, origin, fetch metadata, or CSRF proof is invalid"),
        (status = 409, description = "Envelope revision is not newer than the current revision"),
        (status = 422, description = "Template identifier, envelope, or deployed capability selection is invalid"),
        (status = 503, description = "Envelope templates are unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn author_legacy_envelope_template<R, L, D>(
    Extension(authority): Extension<BrowserAdminAuthority>,
    Extension(_proof): Extension<BrowserMutationProof>,
    State(state): State<BrowserAdminState<R, L, D>>,
    Path(template_id): Path<String>,
    Json(browser_envelope): Json<BrowserEnvelope>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel + Clone,
{
    let body = AuthorEnvelopeTemplateBody {
        display_name: template_id.clone(),
        member_roles: vec![template_id.clone()],
        envelope: browser_envelope,
        auto_provision_threshold: None,
    };
    author_envelope_template::<R, L, D>(
        Extension(authority),
        Extension(_proof),
        State(state),
        Path(template_id),
        Json(body),
    )
    .await
}

fn valid_template_authoring(
    template_id: &str,
    display_name: &str,
    member_roles: &[String],
    envelope: &Envelope,
    auto_provision_threshold: Option<&Envelope>,
) -> bool {
    let valid_identifier = |value: &str| {
        let bytes = value.as_bytes();
        !bytes.is_empty()
            && bytes.len() <= 128
            && bytes[0].is_ascii_alphanumeric()
            && bytes.iter().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':')
            })
    };
    valid_identifier(template_id)
        && !display_name.is_empty()
        && display_name.trim() == display_name
        && display_name.chars().count() <= 128
        && !member_roles.is_empty()
        && member_roles.len() <= 64
        && member_roles.iter().all(|role| valid_identifier(role))
        && member_roles.windows(2).all(|roles| roles[0] < roles[1])
        && envelope.revision > 0
        && !envelope.spec.llms.is_empty()
        && validate_envelope(envelope).is_ok()
        && auto_provision_threshold.is_none_or(|threshold| {
            threshold.revision == envelope.revision
                && validate_envelope(threshold).is_ok()
                && matches!(
                    steward_admission::envelope_is_within(threshold, envelope),
                    Ok(AdmissionDecision::Admit)
                )
        })
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

fn envelope_request_state(status: EnvelopeRequestStatus, automatic: bool) -> AdminRequestState {
    match status {
        EnvelopeRequestStatus::Pending => AdminRequestState::Requested,
        EnvelopeRequestStatus::Approved => AdminRequestState::Approved,
        EnvelopeRequestStatus::Provisioned if automatic => AdminRequestState::AutoApproved,
        EnvelopeRequestStatus::Provisioned => AdminRequestState::Approved,
        EnvelopeRequestStatus::Rejected => AdminRequestState::Rejected,
        EnvelopeRequestStatus::Stale | EnvelopeRequestStatus::Conflict => {
            AdminRequestState::Expired
        }
    }
}

fn decision(
    rationale: Option<String>,
    evidence_url: Option<String>,
    decision_key: Option<String>,
    expires_at: Option<String>,
) -> Option<AdminRequestDecision> {
    (rationale.is_some()
        || evidence_url.is_some()
        || decision_key.is_some()
        || expires_at.is_some())
    .then_some(AdminRequestDecision {
        rationale,
        evidence_url,
        decision_key,
        expires_at,
    })
}

fn envelope_admin_request(record: AdminEnvelopeRequestRecord) -> AdminRequestView {
    let automatic = record.request.status == EnvelopeRequestStatus::Provisioned
        && record.request.status_actor == "system:auto";
    let deltas = match envelope_is_within(
        &record.request.requested_envelope,
        &record.template_envelope,
    ) {
        Ok(AdmissionDecision::Reject { deltas }) => deltas,
        Ok(AdmissionDecision::Admit) | Err(_) => Vec::new(),
    };
    let state = envelope_request_state(record.request.status, automatic);
    let state_actor = if automatic {
        "system:auto".to_owned()
    } else {
        record.request.status_actor.clone()
    };
    AdminRequestView {
        id: record.request.id.to_string(),
        kind: if deltas.is_empty() {
            AdminRequestKind::WithinCeiling
        } else {
            AdminRequestKind::CeilingExceeded
        },
        source: AdminRequestSource::EnvelopeRequest,
        state,
        requester: AdminRequestRequester {
            user_id: record.request.owner_user_id.as_str().to_owned(),
            display_email: record.owner_display_email,
        },
        template: AdminRequestTemplate {
            id: record.request.template_id,
            display_name: record.template_display_name,
            revision: record.request.template_revision,
        },
        created_at: record.request.created_at.clone(),
        state_at: record.request.status_at.clone(),
        state_actor: state_actor.clone(),
        deltas,
        requested_envelope: Some(record.request.requested_envelope.into()),
        template_envelope: Some(record.template_envelope.into()),
        escalation: None,
        decision: decision(
            record.request.rationale,
            record.request.evidence_url,
            record.request.decision_key,
            record.request.expires_at,
        ),
        history: vec![AdminRequestHistoryEvent {
            state,
            at: record.request.status_at,
            actor: state_actor,
            reason: record.request.reason,
        }],
    }
}

fn approval_admin_request(record: AdminApprovalRecord) -> AdminRequestView {
    let state = match record.state.as_str() {
        "approved" => AdminRequestState::Approved,
        "rejected" => AdminRequestState::Rejected,
        _ => AdminRequestState::Escalated,
    };
    let requested = BrowserEnvelope {
        revision: record.envelope_revision,
        spec: BrowserEnvelopeSpec {
            llms: record.proposed_spec.llms,
            tools: record.proposed_spec.tools,
            budget: record.proposed_spec.budget,
            ttl: record.proposed_spec.ttl,
            runner: record.proposed_spec.runner,
        },
    };
    AdminRequestView {
        id: record.approval_id.to_string(),
        kind: AdminRequestKind::CeilingExceeded,
        source: AdminRequestSource::RuntimeException,
        state,
        requester: AdminRequestRequester {
            user_id: record.requester_user_id,
            display_email: record.requester_display_email,
        },
        template: AdminRequestTemplate {
            id: record.member_role.clone(),
            display_name: record.member_role,
            revision: record.envelope_revision,
        },
        created_at: record.created_at,
        state_at: record.state_at.clone(),
        state_actor: record.state_actor.clone(),
        deltas: record.deltas,
        requested_envelope: Some(requested),
        template_envelope: Some(record.template_envelope.into()),
        escalation: None,
        decision: decision(
            record.rationale,
            record.evidence_url,
            record.decision_key,
            None,
        ),
        history: vec![AdminRequestHistoryEvent {
            state,
            at: record.state_at,
            actor: record.state_actor,
            reason: None,
        }],
    }
}

fn escalation_admin_request(record: CumulativeEscalationRecord) -> AdminRequestView {
    let state = if record.grant_id.is_some() && record.grant_active == Some(false) {
        AdminRequestState::Expired
    } else if record.grant_id.is_some() {
        AdminRequestState::Approved
    } else if record.denial_rationale.is_some() {
        AdminRequestState::Rejected
    } else {
        AdminRequestState::Escalated
    };
    let state_at = record
        .decision_at
        .clone()
        .unwrap_or_else(|| record.parked_at.clone());
    let state_actor = record
        .decision_actor
        .clone()
        .unwrap_or_else(|| "system:meter".to_owned());
    let history = if state == AdminRequestState::Expired {
        vec![
            AdminRequestHistoryEvent {
                state: AdminRequestState::Approved,
                at: state_at.clone(),
                actor: state_actor.clone(),
                reason: None,
            },
            AdminRequestHistoryEvent {
                state,
                at: record
                    .grant_valid_until
                    .clone()
                    .unwrap_or_else(|| state_at.clone()),
                actor: "system:expiry".to_owned(),
                reason: None,
            },
        ]
    } else {
        vec![AdminRequestHistoryEvent {
            state,
            at: state_at.clone(),
            actor: state_actor.clone(),
            reason: None,
        }]
    };
    AdminRequestView {
        id: record.escalation_id.to_string(),
        kind: AdminRequestKind::CumulativeExhausted,
        source: AdminRequestSource::Escalation,
        state,
        requester: AdminRequestRequester {
            user_id: record.requester_user_id,
            display_email: record.requester_display_email,
        },
        template: AdminRequestTemplate {
            id: record.template_id,
            display_name: record.template_display_name,
            revision: record.template_revision,
        },
        created_at: record.parked_at.clone(),
        state_at: state_at.clone(),
        state_actor: state_actor.clone(),
        deltas: Vec::new(),
        requested_envelope: None,
        template_envelope: None,
        escalation: Some(EscalationView {
            envelope_instance_id: record.envelope_instance_id,
            period: EscalationPeriod {
                start: record.period_start,
                end: record.period_end,
            },
            meters: vec![EscalationMeter {
                dimension: EscalationDimension::LlmSpend,
                used: record.observed_amount,
                limit: record.limit,
                unit: record.currency,
                observed_at: record.observed_at,
                exhausted: true,
            }],
            blocked_task_uid: record.blocked_task_uid,
            parked_at: record.parked_at,
        }),
        decision: record
            .grant_rationale
            .or(record.denial_rationale)
            .map(|rationale| AdminRequestDecision {
                rationale: Some(rationale),
                evidence_url: None,
                decision_key: None,
                expires_at: record.grant_valid_until,
            }),
        history,
    }
}

async fn admin_request_views<L>(ledger: &L) -> Result<Vec<AdminRequestView>, StoreError>
where
    L: AdmissionLedger,
{
    let mut requests = ledger
        .admin_envelope_requests()
        .await?
        .into_iter()
        .map(envelope_admin_request)
        .collect::<Vec<_>>();
    requests.extend(
        ledger
            .admin_approvals()
            .await?
            .into_iter()
            .map(approval_admin_request),
    );
    requests.extend(
        ledger
            .admin_escalations()
            .await?
            .into_iter()
            .map(escalation_admin_request),
    );
    requests.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| right.id.cmp(&left.id))
    });
    Ok(requests)
}

#[utoipa::path(
    post,
    operation_id = "topUpAdminEscalation",
    path = "/admin/api/v1/escalations/{escalation_id}/top-up",
    params(
        ("escalation_id" = i64, Path),
        ("X-Steward-CSRF" = String, Header)
    ),
    request_body = EscalationTopUpRequest,
    responses(
        (status = 200, body = AdminRequestResponse),
        (status = 400, description = "Top-up is invalid or runtime-minutes enforcement is unavailable"),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role or mutation proof is invalid"),
        (status = 404, description = "Escalation or bound runtime was not found"),
        (status = 409, description = "Escalation was already decided differently"),
        (status = 503, description = "Escalation authority is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn top_up_escalation<R, L, D>(
    Extension(authority): Extension<BrowserAdminAuthority>,
    proof: Option<Extension<BrowserMutationProof>>,
    State(state): State<BrowserAdminState<R, L, D>>,
    Path(escalation_id): Path<i64>,
    Json(request): Json<EscalationTopUpRequest>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel + Clone,
{
    if proof.is_none() {
        return StatusCode::FORBIDDEN.into_response();
    }
    if request.rationale.trim().is_empty() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if request.dimension != EscalationDimension::LlmSpend {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Some(escalation) = (match state.ledger.cumulative_escalation(escalation_id).await {
        Ok(escalation) => escalation,
        Err(error) => return ApiError::Store(error).into_response(),
    }) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if state
        .runtimes
        .get_bound(
            &escalation.runtime_namespace,
            &escalation.runtime_name,
            &escalation.runtime_uid,
        )
        .await
        .is_err()
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let amount = match add_budget_amount("0", &request.amount) {
        Ok(amount)
            if amount
                .bytes()
                .any(|byte| byte.is_ascii_digit() && byte != b'0') =>
        {
            amount
        }
        _ => return StatusCode::BAD_REQUEST.into_response(),
    };
    let actor = authority.principal().canonical_user_id.as_str();
    if let Err(error) = state
        .ledger
        .record_escalation_top_up(
            escalation_id,
            &amount,
            &request.valid_until,
            &request.rationale,
            actor,
        )
        .await
    {
        return ApiError::Store(error).into_response();
    }
    match state.ledger.cumulative_escalation(escalation_id).await {
        Ok(Some(record)) => Json(AdminRequestResponse {
            api_version: BROWSER_ADMIN_API_VERSION,
            request: escalation_admin_request(record),
        })
        .into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => ApiError::Store(error).into_response(),
    }
}

#[utoipa::path(
    post,
    operation_id = "denyAdminEscalation",
    path = "/admin/api/v1/escalations/{escalation_id}/deny",
    params(
        ("escalation_id" = i64, Path),
        ("X-Steward-CSRF" = String, Header)
    ),
    request_body = EscalationDenyRequest,
    responses(
        (status = 200, body = AdminRequestResponse),
        (status = 400, description = "Denial rationale is invalid"),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role or mutation proof is invalid"),
        (status = 404, description = "Escalation was not found"),
        (status = 409, description = "Escalation was already decided differently"),
        (status = 503, description = "Escalation authority is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn deny_escalation<R, L, D>(
    Extension(authority): Extension<BrowserAdminAuthority>,
    proof: Option<Extension<BrowserMutationProof>>,
    State(state): State<BrowserAdminState<R, L, D>>,
    Path(escalation_id): Path<i64>,
    Json(request): Json<EscalationDenyRequest>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel + Clone,
{
    if proof.is_none() {
        return StatusCode::FORBIDDEN.into_response();
    }
    if request.rationale.trim().is_empty() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if let Err(error) = state
        .ledger
        .deny_cumulative_escalation(
            escalation_id,
            &request.rationale,
            authority.principal().canonical_user_id.as_str(),
        )
        .await
    {
        return ApiError::Store(error).into_response();
    }
    match state.ledger.cumulative_escalation(escalation_id).await {
        Ok(Some(record)) => Json(AdminRequestResponse {
            api_version: BROWSER_ADMIN_API_VERSION,
            request: escalation_admin_request(record),
        })
        .into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => ApiError::Store(error).into_response(),
    }
}

fn state_filter_matches(filter: &str, state: AdminRequestState) -> Option<bool> {
    Some(match filter {
        "all" => true,
        "needs_action" => matches!(
            state,
            AdminRequestState::Requested | AdminRequestState::Escalated
        ),
        "requested" => state == AdminRequestState::Requested,
        "escalated" => state == AdminRequestState::Escalated,
        "auto_approved" => state == AdminRequestState::AutoApproved,
        "approved" => state == AdminRequestState::Approved,
        "rejected" => state == AdminRequestState::Rejected,
        "expired" => state == AdminRequestState::Expired,
        _ => return None,
    })
}

fn kind_filter_matches(filter: &str, kind: AdminRequestKind) -> Option<bool> {
    Some(match filter {
        "ceiling_exceeded" => kind == AdminRequestKind::CeilingExceeded,
        "cumulative_exhausted" => kind == AdminRequestKind::CumulativeExhausted,
        "within_ceiling" => kind == AdminRequestKind::WithinCeiling,
        _ => return None,
    })
}

fn admin_request_cursor(request: &AdminRequestView) -> String {
    format!("{}|{}", request.created_at, request.id)
}

#[utoipa::path(
    get,
    operation_id = "listAdminRequests",
    path = "/admin/api/v1/requests",
    params(AdminRequestsQuery),
    responses(
        (status = 200, body = AdminRequestsResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role is required"),
        (status = 422, description = "Filter, cursor, or limit is invalid"),
        (status = 503, description = "Request read model is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn list_admin_requests<R, L, D>(
    Extension(_authority): Extension<BrowserAdminAuthority>,
    State(state): State<BrowserAdminState<R, L, D>>,
    Query(query): Query<AdminRequestsQuery>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel + Clone,
{
    let limit = query.limit.unwrap_or(50);
    if !(1..=100).contains(&limit) {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    }
    let mut requests = match admin_request_views(&state.ledger).await {
        Ok(requests) => requests,
        Err(error) => return ApiError::Store(error).into_response(),
    };
    if let Some(filter) = query.state.as_deref() {
        if state_filter_matches(filter, AdminRequestState::Requested).is_none() {
            return StatusCode::UNPROCESSABLE_ENTITY.into_response();
        }
        requests.retain(|request| state_filter_matches(filter, request.state) == Some(true));
    }
    if let Some(filter) = query.kind.as_deref() {
        if kind_filter_matches(filter, AdminRequestKind::WithinCeiling).is_none() {
            return StatusCode::UNPROCESSABLE_ENTITY.into_response();
        }
        requests.retain(|request| kind_filter_matches(filter, request.kind) == Some(true));
    }
    let start = if let Some(cursor) = query.cursor.as_deref() {
        let Some(position) = requests
            .iter()
            .position(|request| admin_request_cursor(request) == cursor)
        else {
            return StatusCode::UNPROCESSABLE_ENTITY.into_response();
        };
        position + 1
    } else {
        0
    };
    let page = requests
        .into_iter()
        .skip(start)
        .take(limit + 1)
        .collect::<Vec<_>>();
    let has_more = page.len() > limit;
    let requests = page.into_iter().take(limit).collect::<Vec<_>>();
    let next_cursor = has_more
        .then(|| requests.last().map(admin_request_cursor))
        .flatten();
    Json(AdminRequestsResponse {
        api_version: BROWSER_ADMIN_API_VERSION,
        requests,
        next_cursor,
    })
    .into_response()
}

#[utoipa::path(
    get,
    operation_id = "getAdminRequest",
    path = "/admin/api/v1/requests/{request_id}",
    params(("request_id" = String, Path)),
    responses(
        (status = 200, body = AdminRequestResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role is required"),
        (status = 404, description = "Request was not found"),
        (status = 503, description = "Request read model is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn get_admin_request<R, L, D>(
    Extension(_authority): Extension<BrowserAdminAuthority>,
    State(state): State<BrowserAdminState<R, L, D>>,
    Path(request_id): Path<String>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel + Clone,
{
    let mut request = match admin_request_views(&state.ledger).await {
        Ok(requests) => requests
            .into_iter()
            .find(|request| request.id == request_id),
        Err(error) => return ApiError::Store(error).into_response(),
    };
    let Some(mut request) = request.take() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if request.source == AdminRequestSource::EnvelopeRequest {
        let Ok(id) = Uuid::parse_str(&request_id) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let automatic = request.state == AdminRequestState::AutoApproved;
        request.history = match state.ledger.envelope_request_history(id).await {
            Ok(history) => history
                .into_iter()
                .map(|event| AdminRequestHistoryEvent {
                    state: envelope_request_state(event.status, automatic),
                    at: event.at,
                    actor: if automatic && event.status == EnvelopeRequestStatus::Provisioned {
                        "system:auto".to_owned()
                    } else {
                        event.actor
                    },
                    reason: event.reason,
                })
                .collect(),
            Err(error) => return ApiError::Store(error).into_response(),
        };
    }
    Json(AdminRequestResponse {
        api_version: BROWSER_ADMIN_API_VERSION,
        request,
    })
    .into_response()
}

#[utoipa::path(
    get,
    operation_id = "getAdminRequestsSummary",
    path = "/admin/api/v1/requests/summary",
    responses(
        (status = 200, body = AdminRequestsSummaryResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role is required"),
        (status = 503, description = "Request read model is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn admin_requests_summary<R, L, D>(
    Extension(_authority): Extension<BrowserAdminAuthority>,
    State(state): State<BrowserAdminState<R, L, D>>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel + Clone,
{
    let requests = match admin_request_views(&state.ledger).await {
        Ok(requests) => requests,
        Err(error) => return ApiError::Store(error).into_response(),
    };
    let escalated = requests
        .iter()
        .filter(|request| request.state == AdminRequestState::Escalated)
        .count();
    let requested = requests
        .iter()
        .filter(|request| request.state == AdminRequestState::Requested)
        .count();
    Json(AdminRequestsSummaryResponse {
        api_version: BROWSER_ADMIN_API_VERSION,
        needs_action: escalated + requested,
        escalated,
        requested,
    })
    .into_response()
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
