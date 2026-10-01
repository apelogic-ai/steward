//! Stable bearer-authenticated contracts for supported operator frontends.

use std::future::Future;
use std::pin::Pin;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use steward_admission::Envelope;
use steward_store::{
    AdminEnvelopeProvisionRequest, BrowserRbacAssignment, BrowserRbacAssignmentAction,
    BrowserRbacAssignmentChange, BrowserRbacAssignments, CanonicalUserRecord,
    EnvelopeRequestRecord, EnvelopeRequestStatus, EnvelopeTemplatePublication,
    EnvelopeTemplateRevisionRecord, PgStore, StoreError,
};
use steward_types::CanonicalUserId;

use crate::user_envelopes::BrowserEnvelope;
use crate::{AdminContext, RequestAuthenticator, protect_admin_routes};

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub const OPERATOR_API_PREFIX: &str = "/admin/operator/v1";

pub trait OperatorLedger: Clone + Send + Sync + 'static {
    fn canonical_users(&self) -> BoxFuture<'_, Result<Vec<CanonicalUserRecord>, StoreError>>;
    fn canonical_user<'a>(
        &'a self,
        user_id: &'a CanonicalUserId,
    ) -> BoxFuture<'a, Result<Option<CanonicalUserRecord>, StoreError>>;
    fn member_roles(&self) -> BoxFuture<'_, Result<Vec<String>, StoreError>>;
    fn assignments<'a>(
        &'a self,
        user_id: &'a CanonicalUserId,
    ) -> BoxFuture<'a, Result<BrowserRbacAssignments, StoreError>>;
    fn append_assignment<'a>(
        &'a self,
        change: BrowserRbacAssignmentChange<'a>,
    ) -> BoxFuture<'a, Result<(), StoreError>>;
    fn eligible_templates<'a>(
        &'a self,
        member_roles: &'a [String],
    ) -> BoxFuture<'a, Result<Vec<EnvelopeTemplateRevisionRecord>, StoreError>>;
    fn envelope_requests<'a>(
        &'a self,
        user_id: &'a CanonicalUserId,
    ) -> BoxFuture<'a, Result<Vec<EnvelopeRequestRecord>, StoreError>>;
    fn template_revision<'a>(
        &'a self,
        template_id: &'a str,
        revision: i64,
    ) -> BoxFuture<'a, Result<Option<EnvelopeTemplateRevisionRecord>, StoreError>>;
    fn latest_template<'a>(
        &'a self,
        template_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<EnvelopeTemplateRevisionRecord>, StoreError>>;
    fn insert_template<'a>(
        &'a self,
        publication: EnvelopeTemplatePublication<'a>,
    ) -> BoxFuture<'a, Result<(), StoreError>>;
    fn provision<'a>(
        &'a self,
        request: AdminEnvelopeProvisionRequest<'a>,
    ) -> BoxFuture<'a, Result<EnvelopeRequestRecord, StoreError>>;
}

impl OperatorLedger for PgStore {
    fn canonical_users(&self) -> BoxFuture<'_, Result<Vec<CanonicalUserRecord>, StoreError>> {
        Box::pin(async move { PgStore::canonical_users(self).await })
    }

    fn canonical_user<'a>(
        &'a self,
        user_id: &'a CanonicalUserId,
    ) -> BoxFuture<'a, Result<Option<CanonicalUserRecord>, StoreError>> {
        Box::pin(async move { PgStore::canonical_user(self, user_id).await })
    }

    fn member_roles(&self) -> BoxFuture<'_, Result<Vec<String>, StoreError>> {
        Box::pin(async move { PgStore::browser_member_roles(self).await })
    }

    fn assignments<'a>(
        &'a self,
        user_id: &'a CanonicalUserId,
    ) -> BoxFuture<'a, Result<BrowserRbacAssignments, StoreError>> {
        Box::pin(async move { PgStore::browser_rbac_assignments(self, user_id).await })
    }

    fn append_assignment<'a>(
        &'a self,
        change: BrowserRbacAssignmentChange<'a>,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move { PgStore::append_browser_rbac_assignment(self, change).await })
    }

    fn eligible_templates<'a>(
        &'a self,
        member_roles: &'a [String],
    ) -> BoxFuture<'a, Result<Vec<EnvelopeTemplateRevisionRecord>, StoreError>> {
        Box::pin(async move { PgStore::available_envelope_templates(self, member_roles).await })
    }

    fn envelope_requests<'a>(
        &'a self,
        user_id: &'a CanonicalUserId,
    ) -> BoxFuture<'a, Result<Vec<EnvelopeRequestRecord>, StoreError>> {
        Box::pin(async move { PgStore::envelope_requests(self, user_id).await })
    }

    fn template_revision<'a>(
        &'a self,
        template_id: &'a str,
        revision: i64,
    ) -> BoxFuture<'a, Result<Option<EnvelopeTemplateRevisionRecord>, StoreError>> {
        Box::pin(
            async move { PgStore::envelope_template_revision(self, template_id, revision).await },
        )
    }

    fn latest_template<'a>(
        &'a self,
        template_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<EnvelopeTemplateRevisionRecord>, StoreError>> {
        Box::pin(async move { PgStore::latest_envelope_template(self, template_id).await })
    }

    fn insert_template<'a>(
        &'a self,
        publication: EnvelopeTemplatePublication<'a>,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move { PgStore::insert_envelope_template_revision(self, publication).await })
    }

    fn provision<'a>(
        &'a self,
        request: AdminEnvelopeProvisionRequest<'a>,
    ) -> BoxFuture<'a, Result<EnvelopeRequestRecord, StoreError>> {
        Box::pin(async move { PgStore::provision_envelope_for_admin(self, request).await })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct OperatorUserView {
    pub user_id: String,
    pub display_email: String,
    pub organization_id: String,
    pub state: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct OperatorUsersResponse {
    pub users: Vec<OperatorUserView>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct OperatorRolesResponse {
    pub member_roles: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum OperatorAssignmentKind {
    Administrator,
    MemberRole,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum OperatorAssignmentAction {
    Grant,
    Revoke,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OperatorAssignmentRequest {
    pub user_id: String,
    pub kind: OperatorAssignmentKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_role: Option<String>,
    pub action: OperatorAssignmentAction,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct OperatorAssignmentResponse {
    pub user_id: String,
    pub kind: OperatorAssignmentKind,
    pub member_role: Option<String>,
    pub action: OperatorAssignmentAction,
    pub actor: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct OperatorEligibleTemplateView {
    pub template_id: String,
    pub revision: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct OperatorActiveEnvelopeView {
    pub envelope_instance_id: String,
    pub template_id: Option<String>,
    pub template_revision: Option<i64>,
    pub envelope_digest: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct OperatorEffectiveAccessResponse {
    pub user: OperatorUserView,
    pub administrator: bool,
    pub member_roles: Vec<String>,
    pub eligible_templates: Vec<OperatorEligibleTemplateView>,
    pub active_envelopes: Vec<OperatorActiveEnvelopeView>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OperatorTemplateApplyRequest {
    pub display_name: String,
    pub member_roles: Vec<String>,
    pub ceiling: BrowserEnvelope,
    pub auto_provision_threshold: Option<BrowserEnvelope>,
    #[serde(default = "default_true")]
    pub allow_inline_browser_tasks: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct OperatorTemplateResponse {
    pub template_id: String,
    pub display_name: String,
    pub member_roles: Vec<String>,
    pub ceiling: BrowserEnvelope,
    pub auto_provision_threshold: Option<BrowserEnvelope>,
    pub allow_inline_browser_tasks: bool,
}

const fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OperatorProvisionRequest {
    pub owner_user_id: String,
    pub template_id: String,
    pub template_revision: i64,
    pub requested_envelope: BrowserEnvelope,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct OperatorProvisionResponse {
    pub owner_user_id: String,
    pub envelope_instance_id: String,
    pub template_id: String,
    pub template_revision: i64,
    pub envelope_digest: String,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: &'static str,
}

pub fn router<L, A>(ledger: L, authenticator: A) -> Router
where
    L: OperatorLedger,
    A: RequestAuthenticator,
{
    let routes = Router::new()
        .route("/admin/operator/v1/users", get(users::<L>))
        .route("/admin/operator/v1/users/{user_id}", get(user::<L>))
        .route("/admin/operator/v1/roles", get(roles::<L>))
        .route("/admin/operator/v1/rbac", post(mutate_rbac::<L>))
        .route(
            "/admin/operator/v1/users/{user_id}/effective-access",
            get(effective_access::<L>),
        )
        .route(
            "/admin/operator/v1/templates/{template_id}",
            get(latest_template::<L>),
        )
        .route(
            "/admin/operator/v1/templates/{template_id}/revisions/{revision}",
            get(template::<L>).put(apply_template::<L>),
        )
        .route(
            "/admin/operator/v1/envelopes/provision",
            post(provision::<L>),
        )
        .with_state(ledger);
    protect_admin_routes(routes, authenticator)
}

#[utoipa::path(
    get,
    operation_id = "listOperatorUsers",
    path = "/admin/operator/v1/users",
    responses((status = 200, body = OperatorUsersResponse), (status = 401), (status = 403), (status = 503)),
    security(("adminBearer" = []))
)]
pub(crate) async fn users<L: OperatorLedger>(State(ledger): State<L>) -> Response {
    match ledger.canonical_users().await {
        Ok(users) => Json(OperatorUsersResponse {
            users: users.into_iter().map(user_view).collect(),
        })
        .into_response(),
        Err(error) => store_error(error),
    }
}

#[utoipa::path(
    get,
    operation_id = "getOperatorUser",
    path = "/admin/operator/v1/users/{user_id}",
    params(("user_id" = String, Path)),
    responses((status = 200, body = OperatorUserView), (status = 401), (status = 403), (status = 404), (status = 422), (status = 503)),
    security(("adminBearer" = []))
)]
pub(crate) async fn user<L: OperatorLedger>(
    State(ledger): State<L>,
    Path(user_id): Path<String>,
) -> Response {
    let user_id = match CanonicalUserId::parse(user_id) {
        Ok(user_id) => user_id,
        Err(_) => {
            return error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid canonical user ID",
            );
        }
    };
    match ledger.canonical_user(&user_id).await {
        Ok(Some(user)) => Json(user_view(user)).into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, "canonical user not found"),
        Err(error) => store_error(error),
    }
}

#[utoipa::path(
    get,
    operation_id = "listOperatorRoles",
    path = "/admin/operator/v1/roles",
    responses((status = 200, body = OperatorRolesResponse), (status = 401), (status = 403), (status = 503)),
    security(("adminBearer" = []))
)]
pub(crate) async fn roles<L: OperatorLedger>(State(ledger): State<L>) -> Response {
    match ledger.member_roles().await {
        Ok(member_roles) => Json(OperatorRolesResponse { member_roles }).into_response(),
        Err(error) => store_error(error),
    }
}

#[utoipa::path(
    post,
    operation_id = "mutateOperatorRbac",
    path = "/admin/operator/v1/rbac",
    request_body = OperatorAssignmentRequest,
    responses((status = 200, body = OperatorAssignmentResponse), (status = 401), (status = 403), (status = 404), (status = 422), (status = 503)),
    security(("adminBearer" = []))
)]
pub(crate) async fn mutate_rbac<L: OperatorLedger>(
    State(ledger): State<L>,
    Extension(admin): Extension<AdminContext>,
    Json(request): Json<OperatorAssignmentRequest>,
) -> Response {
    let user_id = match CanonicalUserId::parse(request.user_id.clone()) {
        Ok(user_id) => user_id,
        Err(_) => {
            return error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid canonical user ID",
            );
        }
    };
    let assignment = match (request.kind, request.member_role.as_deref()) {
        (OperatorAssignmentKind::Administrator, None) => BrowserRbacAssignment::Administrator,
        (OperatorAssignmentKind::MemberRole, Some(role)) if !role.trim().is_empty() => {
            BrowserRbacAssignment::MemberRole(role.to_owned())
        }
        _ => return error(StatusCode::UNPROCESSABLE_ENTITY, "invalid RBAC assignment"),
    };
    let action = match request.action {
        OperatorAssignmentAction::Grant => BrowserRbacAssignmentAction::Grant,
        OperatorAssignmentAction::Revoke => BrowserRbacAssignmentAction::Revoke,
    };
    match ledger
        .append_assignment(BrowserRbacAssignmentChange {
            user_id: &user_id,
            assignment: &assignment,
            action,
            actor: &admin.actor,
        })
        .await
    {
        Ok(()) => Json(OperatorAssignmentResponse {
            user_id: user_id.as_str().to_owned(),
            kind: request.kind,
            member_role: request.member_role,
            action: request.action,
            actor: admin.actor,
        })
        .into_response(),
        Err(error) => store_error(error),
    }
}

#[utoipa::path(
    get,
    operation_id = "getOperatorEffectiveAccess",
    path = "/admin/operator/v1/users/{user_id}/effective-access",
    params(("user_id" = String, Path)),
    responses((status = 200, body = OperatorEffectiveAccessResponse), (status = 401), (status = 403), (status = 404), (status = 422), (status = 503)),
    security(("adminBearer" = []))
)]
pub(crate) async fn effective_access<L: OperatorLedger>(
    State(ledger): State<L>,
    Path(user_id): Path<String>,
) -> Response {
    let user_id = match CanonicalUserId::parse(user_id) {
        Ok(user_id) => user_id,
        Err(_) => {
            return error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid canonical user ID",
            );
        }
    };
    let user = match ledger.canonical_user(&user_id).await {
        Ok(Some(user)) => user,
        Ok(None) => return error(StatusCode::NOT_FOUND, "canonical user not found"),
        Err(error) => return store_error(error),
    };
    let assignments = match ledger.assignments(&user_id).await {
        Ok(assignments) => assignments,
        Err(error) => return store_error(error),
    };
    let templates = match ledger.eligible_templates(&assignments.member_roles).await {
        Ok(templates) => templates,
        Err(error) => return store_error(error),
    };
    let envelopes = match ledger.envelope_requests(&user_id).await {
        Ok(envelopes) => envelopes,
        Err(error) => return store_error(error),
    };
    let active_envelopes = envelopes
        .into_iter()
        .filter(|record| record.status == EnvelopeRequestStatus::Provisioned)
        .filter_map(|record| {
            Some(OperatorActiveEnvelopeView {
                envelope_instance_id: record.envelope_instance_id?,
                template_id: record.template_id,
                template_revision: record.template_revision,
                envelope_digest: format!("steward:{}", record.envelope_digest?),
            })
        })
        .collect();
    Json(OperatorEffectiveAccessResponse {
        user: user_view(user),
        administrator: assignments.is_admin,
        member_roles: assignments.member_roles,
        eligible_templates: templates
            .into_iter()
            .map(|template| OperatorEligibleTemplateView {
                template_id: template.template_id,
                revision: template.ceiling.revision,
            })
            .collect(),
        active_envelopes,
    })
    .into_response()
}

#[utoipa::path(
    get,
    operation_id = "getOperatorTemplateRevision",
    path = "/admin/operator/v1/templates/{template_id}/revisions/{revision}",
    params(("template_id" = String, Path), ("revision" = i64, Path)),
    responses((status = 200, body = OperatorTemplateResponse), (status = 401), (status = 403), (status = 404), (status = 422), (status = 503)),
    security(("adminBearer" = []))
)]
pub(crate) async fn template<L: OperatorLedger>(
    State(ledger): State<L>,
    Path((template_id, revision)): Path<(String, i64)>,
) -> Response {
    if !crate::browser_admin::valid_template_identifier(&template_id) || revision <= 0 {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid template revision",
        );
    }
    match ledger.template_revision(&template_id, revision).await {
        Ok(Some(template)) => Json(template_view(template)).into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, "template revision not found"),
        Err(error) => store_error(error),
    }
}

#[utoipa::path(
    get,
    operation_id = "getLatestOperatorTemplate",
    path = "/admin/operator/v1/templates/{template_id}",
    params(("template_id" = String, Path)),
    responses((status = 200, body = OperatorTemplateResponse), (status = 401), (status = 403), (status = 404), (status = 422), (status = 503)),
    security(("adminBearer" = []))
)]
pub(crate) async fn latest_template<L: OperatorLedger>(
    State(ledger): State<L>,
    Path(template_id): Path<String>,
) -> Response {
    if !crate::browser_admin::valid_template_identifier(&template_id) {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid template identifier",
        );
    }
    match ledger.latest_template(&template_id).await {
        Ok(Some(template)) => Json(template_view(template)).into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, "template not found"),
        Err(error) => store_error(error),
    }
}

#[utoipa::path(
    put,
    operation_id = "applyOperatorTemplateRevision",
    path = "/admin/operator/v1/templates/{template_id}/revisions/{revision}",
    params(("template_id" = String, Path), ("revision" = i64, Path)),
    request_body = OperatorTemplateApplyRequest,
    responses((status = 200, body = OperatorTemplateResponse), (status = 201, body = OperatorTemplateResponse), (status = 401), (status = 403), (status = 409), (status = 422), (status = 503)),
    security(("adminBearer" = []))
)]
pub(crate) async fn apply_template<L: OperatorLedger>(
    State(ledger): State<L>,
    Extension(admin): Extension<AdminContext>,
    Path((template_id, revision)): Path<(String, i64)>,
    Json(mut request): Json<OperatorTemplateApplyRequest>,
) -> Response {
    if !crate::browser_admin::valid_template_identifier(&template_id)
        || revision <= 0
        || request.ceiling.revision != revision
        || request
            .auto_provision_threshold
            .as_ref()
            .is_some_and(|threshold| threshold.revision != revision)
    {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "template revision mismatch",
        );
    }
    request.member_roles.sort();
    if request.member_roles.is_empty()
        || request
            .member_roles
            .iter()
            .any(|role| role.trim().is_empty())
        || request
            .member_roles
            .windows(2)
            .any(|roles| roles[0] == roles[1])
    {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid template member roles",
        );
    }
    let ceiling: Envelope = request.ceiling.clone().into();
    let threshold = request.auto_provision_threshold.clone().map(Into::into);
    match ledger.template_revision(&template_id, revision).await {
        Ok(Some(existing)) => {
            if existing.display_name != request.display_name
                || existing.member_roles != request.member_roles
                || existing.ceiling != ceiling
                || existing.auto_provision_threshold != threshold
                || existing.allow_inline_browser_tasks != request.allow_inline_browser_tasks
            {
                return error(
                    StatusCode::CONFLICT,
                    "template revision exists with different content",
                );
            }
            Json(template_view(existing)).into_response()
        }
        Ok(None) => match ledger
            .insert_template(EnvelopeTemplatePublication {
                template_id: &template_id,
                display_name: &request.display_name,
                member_roles: &request.member_roles,
                ceiling: &ceiling,
                auto_provision_threshold: threshold.as_ref(),
                allow_inline_browser_tasks: request.allow_inline_browser_tasks,
                authored_by: &admin.actor,
            })
            .await
        {
            Ok(()) => (
                StatusCode::CREATED,
                Json(OperatorTemplateResponse {
                    template_id,
                    display_name: request.display_name,
                    member_roles: request.member_roles,
                    ceiling: request.ceiling,
                    auto_provision_threshold: request.auto_provision_threshold,
                    allow_inline_browser_tasks: request.allow_inline_browser_tasks,
                }),
            )
                .into_response(),
            Err(error) => store_error(error),
        },
        Err(error) => store_error(error),
    }
}

#[utoipa::path(
    post,
    operation_id = "provisionOperatorEnvelope",
    path = "/admin/operator/v1/envelopes/provision",
    request_body = OperatorProvisionRequest,
    responses((status = 200, body = OperatorProvisionResponse), (status = 401), (status = 403), (status = 404), (status = 409), (status = 422), (status = 503)),
    security(("adminBearer" = []))
)]
pub(crate) async fn provision<L: OperatorLedger>(
    State(ledger): State<L>,
    Extension(admin): Extension<AdminContext>,
    Json(request): Json<OperatorProvisionRequest>,
) -> Response {
    let owner_user_id = match CanonicalUserId::parse(request.owner_user_id) {
        Ok(user_id) => user_id,
        Err(_) => {
            return error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid canonical user ID",
            );
        }
    };
    let requested_envelope: Envelope = request.requested_envelope.into();
    match ledger
        .provision(AdminEnvelopeProvisionRequest {
            owner_user_id: &owner_user_id,
            template_id: &request.template_id,
            template_revision: request.template_revision,
            requested_envelope: &requested_envelope,
            idempotency_key: &request.idempotency_key,
            actor: &admin.actor,
        })
        .await
    {
        Ok(record) => match (
            record.envelope_instance_id,
            record.template_id,
            record.template_revision,
            record.envelope_digest,
        ) {
            (
                Some(envelope_instance_id),
                Some(template_id),
                Some(template_revision),
                Some(digest),
            ) => Json(OperatorProvisionResponse {
                owner_user_id: owner_user_id.as_str().to_owned(),
                envelope_instance_id,
                template_id,
                template_revision,
                envelope_digest: format!("steward:{digest}"),
            })
            .into_response(),
            _ => error(
                StatusCode::SERVICE_UNAVAILABLE,
                "invalid provisioning record",
            ),
        },
        Err(error) => store_error(error),
    }
}

fn user_view(user: CanonicalUserRecord) -> OperatorUserView {
    OperatorUserView {
        user_id: user.user_id.as_str().to_owned(),
        display_email: user.display_email.as_str().to_owned(),
        organization_id: user.organization_id.as_str().to_owned(),
        state: user.state,
    }
}

fn template_view(template: EnvelopeTemplateRevisionRecord) -> OperatorTemplateResponse {
    OperatorTemplateResponse {
        template_id: template.template_id,
        display_name: template.display_name,
        member_roles: template.member_roles,
        ceiling: template.ceiling.into(),
        auto_provision_threshold: template.auto_provision_threshold.map(Into::into),
        allow_inline_browser_tasks: template.allow_inline_browser_tasks,
    }
}

fn store_error(error_value: StoreError) -> Response {
    let (status, message) = match error_value {
        StoreError::CanonicalIdentityNotFound | StoreError::EnvelopeTemplateNotFound => {
            (StatusCode::NOT_FOUND, "operator resource not found")
        }
        StoreError::CanonicalIdentityInactive | StoreError::FederatedSubjectDisabled => {
            (StatusCode::FORBIDDEN, "target identity is inactive")
        }
        StoreError::CanonicalIdentityConflict
        | StoreError::EnvelopeRequestDigestConflict
        | StoreError::EnvelopeRequestIdempotencyConflict
        | StoreError::EnvelopeRequestTemplateStale
        | StoreError::EnvelopeRevisionNotIncreasing => (
            StatusCode::CONFLICT,
            "operator request conflicts with current state",
        ),
        StoreError::Database(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "operator dependency unavailable",
        ),
        _ => (StatusCode::UNPROCESSABLE_ENTITY, "invalid operator request"),
    };
    error(status, message)
}

fn error(status: StatusCode, message: &'static str) -> Response {
    (status, Json(ErrorResponse { error: message })).into_response()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, header};
    use tower::ServiceExt;

    use super::*;
    use crate::{AuthenticatedCaller, AuthenticationError, RequestAuthenticator};

    #[derive(Clone)]
    struct FakeAuthenticator {
        is_admin: bool,
    }

    impl RequestAuthenticator for FakeAuthenticator {
        fn authenticate<'a>(
            &'a self,
            bearer_token: &'a str,
        ) -> crate::BoxFuture<'a, Result<AuthenticatedCaller, AuthenticationError>> {
            Box::pin(async move {
                if bearer_token != "operator-token" {
                    return Err(AuthenticationError::InvalidCredentials);
                }
                Ok(AuthenticatedCaller {
                    actor: "system:serviceaccount:steward-test:operator".to_owned(),
                    member_roles: Vec::new(),
                    canonical_user_id: None,
                    is_admin: self.is_admin,
                })
            })
        }
    }

    #[derive(Clone, Default)]
    struct FakeOperatorLedger {
        mutations: Arc<Mutex<Vec<(String, String, BrowserRbacAssignmentAction)>>>,
    }

    impl OperatorLedger for FakeOperatorLedger {
        fn canonical_users(&self) -> BoxFuture<'_, Result<Vec<CanonicalUserRecord>, StoreError>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn canonical_user<'a>(
            &'a self,
            _user_id: &'a CanonicalUserId,
        ) -> BoxFuture<'a, Result<Option<CanonicalUserRecord>, StoreError>> {
            Box::pin(async { Ok(None) })
        }

        fn member_roles(&self) -> BoxFuture<'_, Result<Vec<String>, StoreError>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn assignments<'a>(
            &'a self,
            _user_id: &'a CanonicalUserId,
        ) -> BoxFuture<'a, Result<BrowserRbacAssignments, StoreError>> {
            Box::pin(async { Ok(BrowserRbacAssignments::default()) })
        }

        fn append_assignment<'a>(
            &'a self,
            change: BrowserRbacAssignmentChange<'a>,
        ) -> BoxFuture<'a, Result<(), StoreError>> {
            Box::pin(async move {
                self.mutations
                    .lock()
                    .map_err(|_| StoreError::Database("fake mutation lock poisoned".to_owned()))?
                    .push((
                        change.user_id.as_str().to_owned(),
                        change.actor.to_owned(),
                        change.action,
                    ));
                Ok(())
            })
        }

        fn eligible_templates<'a>(
            &'a self,
            _member_roles: &'a [String],
        ) -> BoxFuture<'a, Result<Vec<EnvelopeTemplateRevisionRecord>, StoreError>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn envelope_requests<'a>(
            &'a self,
            _user_id: &'a CanonicalUserId,
        ) -> BoxFuture<'a, Result<Vec<EnvelopeRequestRecord>, StoreError>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn template_revision<'a>(
            &'a self,
            _template_id: &'a str,
            _revision: i64,
        ) -> BoxFuture<'a, Result<Option<EnvelopeTemplateRevisionRecord>, StoreError>> {
            Box::pin(async { Ok(None) })
        }

        fn latest_template<'a>(
            &'a self,
            _template_id: &'a str,
        ) -> BoxFuture<'a, Result<Option<EnvelopeTemplateRevisionRecord>, StoreError>> {
            Box::pin(async { Ok(None) })
        }

        fn insert_template<'a>(
            &'a self,
            _publication: EnvelopeTemplatePublication<'a>,
        ) -> BoxFuture<'a, Result<(), StoreError>> {
            Box::pin(async { Err(StoreError::InvalidEnvelopeTemplate) })
        }

        fn provision<'a>(
            &'a self,
            _request: AdminEnvelopeProvisionRequest<'a>,
        ) -> BoxFuture<'a, Result<EnvelopeRequestRecord, StoreError>> {
            Box::pin(async { Err(StoreError::InvalidEnvelopeRequest) })
        }
    }

    #[tokio::test]
    async fn rbac_mutation_records_only_the_authenticated_admin_actor() -> Result<(), String> {
        let ledger = FakeOperatorLedger::default();
        let request = || {
            Request::builder()
                .method("POST")
                .uri("/admin/operator/v1/rbac")
                .header(header::AUTHORIZATION, "Bearer operator-token")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"userId":"usr_0123456789abcdef0123456789abcdef","kind":"administrator","action":"grant"}"#,
                ))
                .map_err(|error| format!("build operator request: {error}"))
        };
        let response = router(ledger.clone(), FakeAuthenticator { is_admin: true })
            .oneshot(request()?)
            .await
            .map_err(|error| format!("execute operator request: {error}"))?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024)
            .await
            .map_err(|error| format!("read operator response: {error}"))?;
        let response: OperatorAssignmentResponse = serde_json::from_slice(&body)
            .map_err(|error| format!("parse operator response: {error}"))?;
        assert_eq!(
            response.actor,
            "system:serviceaccount:steward-test:operator"
        );
        assert_eq!(
            ledger
                .mutations
                .lock()
                .map_err(|_| "fake mutation lock poisoned")?
                .as_slice(),
            &[(
                "usr_0123456789abcdef0123456789abcdef".to_owned(),
                "system:serviceaccount:steward-test:operator".to_owned(),
                BrowserRbacAssignmentAction::Grant,
            )]
        );

        let denied = router(ledger.clone(), FakeAuthenticator { is_admin: false })
            .oneshot(request()?)
            .await
            .map_err(|error| format!("execute denied operator request: {error}"))?;
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            ledger
                .mutations
                .lock()
                .map_err(|_| "fake mutation lock poisoned")?
                .len(),
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn template_routes_reject_identifiers_outside_the_catalog_grammar() -> Result<(), String>
    {
        let response = router(
            FakeOperatorLedger::default(),
            FakeAuthenticator { is_admin: true },
        )
        .oneshot(
            Request::builder()
                .uri("/admin/operator/v1/templates/bad%20role")
                .header(header::AUTHORIZATION, "Bearer operator-token")
                .body(Body::empty())
                .map_err(|error| format!("build invalid template request: {error}"))?,
        )
        .await
        .map_err(|error| format!("execute invalid template request: {error}"))?;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        Ok(())
    }
}
