//! Browser administrator APIs for member onboarding and local role assignment.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use steward_store::{
    BrowserRbacAssignment, BrowserRbacAssignmentAction, BrowserRbacAssignmentChange,
    BrowserRbacAssignments, CanonicalUserRecord, FederatedSubjectAssociationMethod,
    FederatedSubjectAuditAction, FederatedSubjectRecord, FederatedSubjectUnlink, PgStore,
    StoreError,
};
use steward_types::{CanonicalUserId, Email, OrganizationId};
use uuid::Uuid;

use crate::browser_auth::{
    BrowserAdminAuthority, BrowserAuthService, BrowserMutationProof, protect_browser_admin_routes,
};

pub const BROWSER_MEMBERS_API_VERSION: &str = "steward.browser-members/v1";

#[derive(Clone)]
pub(crate) struct BrowserMembersState {
    store: PgStore,
    auth: BrowserAuthService,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BrowserMemberView {
    pub user_id: String,
    pub display_email: String,
    pub display_name: Option<String>,
    pub state: String,
    pub administrator: bool,
    pub member_roles: Vec<String>,
    pub created_at: String,
    pub last_sign_in_at: Option<String>,
    pub invited_by: Option<String>,
    pub identity_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BrowserMemberIdentityView {
    pub subject_id: String,
    pub issuer: String,
    pub subject: String,
    pub display_name: Option<String>,
    pub state: String,
    pub linked_at: String,
    pub linked_by: String,
    pub association_method: String,
    pub revision: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BrowserMemberDetailView {
    #[serde(flatten)]
    pub member: BrowserMemberView,
    pub identities: Vec<BrowserMemberIdentityView>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BrowserMembersResponse {
    pub api_version: &'static str,
    pub members: Vec<BrowserMemberView>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BrowserMemberResponse {
    pub api_version: &'static str,
    pub member: BrowserMemberView,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BrowserMemberDetailResponse {
    pub api_version: &'static str,
    pub member: BrowserMemberDetailView,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateBrowserMemberBody {
    pub email: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BrowserMemberAssignmentKind {
    Administrator,
    MemberRole,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BrowserMemberAssignmentAction {
    Grant,
    Revoke,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChangeBrowserMemberRoleBody {
    pub kind: BrowserMemberAssignmentKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_role: Option<String>,
    pub action: BrowserMemberAssignmentAction,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UnlinkBrowserMemberIdentityBody {
    pub expected_revision: i64,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: &'static str,
}

/// Mount member administration behind the normal browser-admin boundary.
pub fn protected_router(store: PgStore, auth: BrowserAuthService) -> Router {
    protect_browser_admin_routes(
        Router::new()
            .route(
                "/admin/api/v1/members",
                get(list_members).post(create_member),
            )
            .route(
                "/admin/api/v1/members/{user_id}/roles",
                post(change_member_role),
            )
            .route("/admin/api/v1/members/{user_id}", get(get_member))
            .route(
                "/admin/api/v1/members/{user_id}/identities/{subject_id}/unlink",
                post(unlink_member_identity),
            )
            .with_state(BrowserMembersState {
                store,
                auth: auth.clone(),
            }),
        auth,
    )
}

#[utoipa::path(
    get,
    operation_id = "listAdminMembers",
    path = "/admin/api/v1/members",
    responses(
        (status = 200, body = BrowserMembersResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role is required"),
        (status = 503, description = "Member records are unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn list_members(
    Extension(authority): Extension<BrowserAdminAuthority>,
    State(state): State<BrowserMembersState>,
) -> Response {
    let organization = match state
        .store
        .canonical_user(&authority.principal().canonical_user_id)
        .await
    {
        Ok(Some(user)) => user.organization_id,
        Ok(None) => {
            return error(
                StatusCode::FORBIDDEN,
                "administrator identity is unavailable",
            );
        }
        Err(error_value) => return store_error(error_value),
    };
    let users = match state
        .store
        .canonical_users_in_organization(&organization)
        .await
    {
        Ok(users) => users,
        Err(error_value) => return store_error(error_value),
    };
    let mut members = Vec::with_capacity(users.len());
    for user in users {
        let assignments = match state.store.browser_rbac_assignments(&user.user_id).await {
            Ok(assignments) => assignments,
            Err(error_value) => return store_error(error_value),
        };
        let identity_count = match state
            .store
            .federated_subjects_for_canonical_user(&user.user_id)
            .await
        {
            Ok(identities) => identities.len(),
            Err(error_value) => return store_error(error_value),
        };
        members.push(member_view(user, assignments, identity_count));
    }
    Json(BrowserMembersResponse {
        api_version: BROWSER_MEMBERS_API_VERSION,
        members,
    })
    .into_response()
}

#[utoipa::path(
    get,
    operation_id = "getAdminMember",
    path = "/admin/api/v1/members/{user_id}",
    params(("user_id" = String, Path)),
    responses(
        (status = 200, body = BrowserMemberDetailResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role is required"),
        (status = 404, description = "Member was not found"),
        (status = 503, description = "Member records are unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn get_member(
    Extension(authority): Extension<BrowserAdminAuthority>,
    State(state): State<BrowserMembersState>,
    Path(user_id): Path<String>,
) -> Response {
    let user_id = match CanonicalUserId::parse(user_id) {
        Ok(user_id) => user_id,
        Err(_) => return error(StatusCode::NOT_FOUND, "member was not found"),
    };
    let organization = match administrator_organization(&state.store, &authority).await {
        Ok(organization) => organization,
        Err(response) => return response,
    };
    let user = match state.store.canonical_user(&user_id).await {
        Ok(Some(user)) if user.organization_id == organization => user,
        Ok(Some(_)) | Ok(None) => return error(StatusCode::NOT_FOUND, "member was not found"),
        Err(error_value) => return store_error(error_value),
    };
    let assignments = match state.store.browser_rbac_assignments(&user_id).await {
        Ok(assignments) => assignments,
        Err(error_value) => return store_error(error_value),
    };
    let subjects = match state
        .store
        .federated_subjects_for_canonical_user(&user_id)
        .await
    {
        Ok(subjects) => subjects,
        Err(error_value) => return store_error(error_value),
    };
    let mut identities = Vec::with_capacity(subjects.len());
    for subject in subjects {
        match member_identity_view(&state.store, &organization, &user_id, subject).await {
            Ok(identity) => identities.push(identity),
            Err(error_value) => return store_error(error_value),
        }
    }
    let member = member_view(user, assignments, identities.len());
    Json(BrowserMemberDetailResponse {
        api_version: BROWSER_MEMBERS_API_VERSION,
        member: BrowserMemberDetailView { member, identities },
    })
    .into_response()
}

#[utoipa::path(
    post,
    operation_id = "createAdminMember",
    path = "/admin/api/v1/members",
    params(("X-Steward-CSRF" = String, Header)),
    request_body = CreateBrowserMemberBody,
    responses(
        (status = 200, body = BrowserMemberResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role, origin, fetch metadata, or CSRF proof is invalid"),
        (status = 422, description = "Member email is invalid"),
        (status = 503, description = "Member records are unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn create_member(
    Extension(authority): Extension<BrowserAdminAuthority>,
    Extension(_proof): Extension<BrowserMutationProof>,
    State(state): State<BrowserMembersState>,
    Json(body): Json<CreateBrowserMemberBody>,
) -> Response {
    let email = match Email::parse(body.email) {
        Ok(email) => email,
        Err(_) => return error(StatusCode::UNPROCESSABLE_ENTITY, "member email is invalid"),
    };
    let organization = match state
        .store
        .canonical_user(&authority.principal().canonical_user_id)
        .await
    {
        Ok(Some(user)) => user.organization_id,
        Ok(None) => {
            return error(
                StatusCode::FORBIDDEN,
                "administrator identity is unavailable",
            );
        }
        Err(error_value) => return store_error(error_value),
    };
    let user = match state
        .store
        .preprovision_canonical_user(
            &organization,
            &email,
            authority.principal().canonical_user_id.as_str(),
        )
        .await
    {
        Ok(user) => user,
        Err(error_value) => return store_error(error_value),
    };
    let assignments = match state.store.browser_rbac_assignments(&user.user_id).await {
        Ok(assignments) => assignments,
        Err(error_value) => return store_error(error_value),
    };
    Json(BrowserMemberResponse {
        api_version: BROWSER_MEMBERS_API_VERSION,
        member: member_view(user, assignments, 0),
    })
    .into_response()
}

#[utoipa::path(
    post,
    operation_id = "changeAdminMemberRole",
    path = "/admin/api/v1/members/{user_id}/roles",
    params(("user_id" = String, Path), ("X-Steward-CSRF" = String, Header)),
    request_body = ChangeBrowserMemberRoleBody,
    responses(
        (status = 200, body = BrowserMemberResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role, origin, fetch metadata, or CSRF proof is invalid"),
        (status = 404, description = "Member was not found"),
        (status = 409, description = "The last active administrator cannot be revoked"),
        (status = 422, description = "Role change is invalid"),
        (status = 503, description = "Member records are unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn change_member_role(
    Extension(authority): Extension<BrowserAdminAuthority>,
    Extension(_proof): Extension<BrowserMutationProof>,
    State(state): State<BrowserMembersState>,
    Path(user_id): Path<String>,
    Json(body): Json<ChangeBrowserMemberRoleBody>,
) -> Response {
    let user_id = match CanonicalUserId::parse(user_id) {
        Ok(user_id) => user_id,
        Err(_) => return error(StatusCode::UNPROCESSABLE_ENTITY, "member ID is invalid"),
    };
    let administrator = match state
        .store
        .canonical_user(&authority.principal().canonical_user_id)
        .await
    {
        Ok(Some(user)) => user,
        Ok(None) => {
            return error(
                StatusCode::FORBIDDEN,
                "administrator identity is unavailable",
            );
        }
        Err(error_value) => return store_error(error_value),
    };
    let user = match state.store.canonical_user(&user_id).await {
        Ok(Some(user)) if user.organization_id == administrator.organization_id => user,
        Ok(Some(_)) | Ok(None) => return error(StatusCode::NOT_FOUND, "member was not found"),
        Err(error_value) => return store_error(error_value),
    };
    let assignment = match (body.kind, body.member_role.as_deref()) {
        (BrowserMemberAssignmentKind::Administrator, None) => BrowserRbacAssignment::Administrator,
        (BrowserMemberAssignmentKind::MemberRole, Some(role)) if !role.trim().is_empty() => {
            BrowserRbacAssignment::MemberRole(role.to_owned())
        }
        _ => return error(StatusCode::UNPROCESSABLE_ENTITY, "member role is invalid"),
    };
    let action = match body.action {
        BrowserMemberAssignmentAction::Grant => BrowserRbacAssignmentAction::Grant,
        BrowserMemberAssignmentAction::Revoke => BrowserRbacAssignmentAction::Revoke,
    };
    if is_self_administrator_revocation(
        &authority.principal().canonical_user_id,
        &user_id,
        &assignment,
        action,
    ) {
        return error(
            StatusCode::CONFLICT,
            "you cannot remove your own administrator access",
        );
    }
    if let Err(error_value) = state
        .store
        .append_browser_rbac_assignment(BrowserRbacAssignmentChange {
            user_id: &user_id,
            assignment: &assignment,
            action,
            actor: authority.principal().canonical_user_id.as_str(),
        })
        .await
    {
        return store_error(error_value);
    }
    if state.auth.revoke_canonical_user_sessions(&user_id).is_err() {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "member role changed but session refresh is unavailable",
        );
    }
    let assignments = match state.store.browser_rbac_assignments(&user_id).await {
        Ok(assignments) => assignments,
        Err(error_value) => return store_error(error_value),
    };
    let identity_count = match state
        .store
        .federated_subjects_for_canonical_user(&user_id)
        .await
    {
        Ok(identities) => identities.len(),
        Err(error_value) => return store_error(error_value),
    };
    Json(BrowserMemberResponse {
        api_version: BROWSER_MEMBERS_API_VERSION,
        member: member_view(user, assignments, identity_count),
    })
    .into_response()
}

#[utoipa::path(
    post,
    operation_id = "unlinkAdminMemberIdentity",
    path = "/admin/api/v1/members/{user_id}/identities/{subject_id}/unlink",
    params(
        ("user_id" = String, Path),
        ("subject_id" = String, Path),
        ("X-Steward-CSRF" = String, Header)
    ),
    request_body = UnlinkBrowserMemberIdentityBody,
    responses(
        (status = 204, description = "Identity was returned to the unassociated pool"),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role, origin, fetch metadata, or CSRF proof is invalid"),
        (status = 404, description = "Member identity was not found"),
        (status = 409, description = "Identity state or revision conflicts"),
        (status = 422, description = "Unlink request is invalid"),
        (status = 503, description = "Member identity is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn unlink_member_identity(
    Extension(authority): Extension<BrowserAdminAuthority>,
    Extension(_proof): Extension<BrowserMutationProof>,
    State(state): State<BrowserMembersState>,
    Path((user_id, subject_id)): Path<(String, Uuid)>,
    Json(body): Json<UnlinkBrowserMemberIdentityBody>,
) -> Response {
    let user_id = match CanonicalUserId::parse(user_id) {
        Ok(user_id) => user_id,
        Err(_) => return error(StatusCode::NOT_FOUND, "member identity was not found"),
    };
    let organization = match administrator_organization(&state.store, &authority).await {
        Ok(organization) => organization,
        Err(response) => return response,
    };
    match state.store.canonical_user(&user_id).await {
        Ok(Some(user)) if user.organization_id == organization => {}
        Ok(Some(_)) | Ok(None) => {
            return error(StatusCode::NOT_FOUND, "member identity was not found");
        }
        Err(error_value) => return store_error(error_value),
    }
    let subject = match state.store.federated_subject(subject_id).await {
        Ok(subject)
            if subject.canonical_user_id.as_ref() == Some(&user_id)
                && subject.state == steward_store::FederatedSubjectState::Associated =>
        {
            subject
        }
        Ok(_) | Err(StoreError::FederatedSubjectNotFound) => {
            return error(StatusCode::NOT_FOUND, "member identity was not found");
        }
        Err(error_value) => return store_error(error_value),
    };
    if subject.revision != body.expected_revision {
        return error(StatusCode::CONFLICT, "member identity revision conflicts");
    }
    match state
        .store
        .unlink_federated_subject(FederatedSubjectUnlink {
            subject_id,
            expected_revision: body.expected_revision,
            canonical_user_id: &user_id,
            actor: authority.principal().canonical_user_id.as_str(),
        })
        .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(error_value) => store_error(error_value),
    }
}

async fn administrator_organization(
    store: &PgStore,
    authority: &BrowserAdminAuthority,
) -> Result<OrganizationId, Response> {
    match store
        .canonical_user(&authority.principal().canonical_user_id)
        .await
    {
        Ok(Some(user)) => Ok(user.organization_id),
        Ok(None) => Err(error(
            StatusCode::FORBIDDEN,
            "administrator identity is unavailable",
        )),
        Err(error_value) => Err(store_error(error_value)),
    }
}

async fn member_identity_view(
    store: &PgStore,
    organization: &OrganizationId,
    user_id: &CanonicalUserId,
    subject: FederatedSubjectRecord,
) -> Result<BrowserMemberIdentityView, StoreError> {
    let audit = store.federated_subject_audit(subject.subject_id).await?;
    let link = audit.iter().rev().find(|event| {
        event.canonical_user_id.as_ref() == Some(user_id)
            && matches!(
                event.action,
                FederatedSubjectAuditAction::V2Seeded
                    | FederatedSubjectAuditAction::ConnectionVerified
                    | FederatedSubjectAuditAction::Associated
                    | FederatedSubjectAuditAction::Replaced
            )
    });
    let linked_at = link
        .map(|event| event.created_at.clone())
        .unwrap_or_else(|| subject.updated_at.clone());
    let linked_by = if subject.association_method
        == Some(FederatedSubjectAssociationMethod::ConnectionVerification)
    {
        "GitHub Connect".to_owned()
    } else if let Some(event) = link {
        match CanonicalUserId::parse(&event.actor) {
            Ok(actor_id) => match store.canonical_user(&actor_id).await? {
                Some(actor) if actor.organization_id == *organization => {
                    actor.display_email.as_str().to_owned()
                }
                _ => event.actor.clone(),
            },
            Err(_) => event.actor.clone(),
        }
    } else {
        "system".to_owned()
    };
    Ok(BrowserMemberIdentityView {
        subject_id: subject.subject_id.to_string(),
        issuer: subject.issuer,
        subject: subject.subject,
        display_name: subject.display_name,
        state: subject.state.as_str().to_owned(),
        linked_at,
        linked_by,
        association_method: subject
            .association_method
            .map_or_else(|| "unknown".to_owned(), |method| method.as_str().to_owned()),
        revision: subject.revision,
    })
}

fn member_view(
    user: CanonicalUserRecord,
    assignments: BrowserRbacAssignments,
    identity_count: usize,
) -> BrowserMemberView {
    let invited_by = (user.state == "pending")
        .then_some(user.invited_by)
        .flatten();
    BrowserMemberView {
        user_id: user.user_id.as_str().to_owned(),
        display_email: user.display_email.as_str().to_owned(),
        display_name: user.display_name,
        state: user.state,
        administrator: assignments.is_admin,
        member_roles: assignments.member_roles,
        created_at: user.created_at,
        last_sign_in_at: user.last_sign_in_at,
        invited_by,
        identity_count,
    }
}

fn is_self_administrator_revocation(
    actor: &CanonicalUserId,
    target: &CanonicalUserId,
    assignment: &BrowserRbacAssignment,
    action: BrowserRbacAssignmentAction,
) -> bool {
    actor == target
        && *assignment == BrowserRbacAssignment::Administrator
        && action == BrowserRbacAssignmentAction::Revoke
}

fn store_error(error_value: StoreError) -> Response {
    let (status, message) = match error_value {
        StoreError::CanonicalIdentityNotFound => (StatusCode::NOT_FOUND, "member was not found"),
        StoreError::LastBrowserAdministrator => (
            StatusCode::CONFLICT,
            "the last active administrator cannot be revoked",
        ),
        StoreError::Database(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "member records are unavailable",
        ),
        _ => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "member request is invalid",
        ),
    };
    error(status, message)
}

fn error(status: StatusCode, message: &'static str) -> Response {
    (status, Json(ErrorResponse { error: message })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_administrator_revocation_is_rejected_without_blocking_other_changes()
    -> Result<(), String> {
        let actor = CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")
            .map_err(|error_value| format!("invalid actor fixture: {error_value}"))?;
        let other = CanonicalUserId::parse("usr_abcdef0123456789abcdef0123456789")
            .map_err(|error_value| format!("invalid target fixture: {error_value}"))?;
        assert!(is_self_administrator_revocation(
            &actor,
            &actor,
            &BrowserRbacAssignment::Administrator,
            BrowserRbacAssignmentAction::Revoke,
        ));
        assert!(!is_self_administrator_revocation(
            &actor,
            &other,
            &BrowserRbacAssignment::Administrator,
            BrowserRbacAssignmentAction::Revoke,
        ));
        assert!(!is_self_administrator_revocation(
            &actor,
            &actor,
            &BrowserRbacAssignment::MemberRole("engineer".to_owned()),
            BrowserRbacAssignmentAction::Revoke,
        ));
        Ok(())
    }
}
