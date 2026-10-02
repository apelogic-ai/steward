//! Browser administrator APIs for member onboarding and local role assignment.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use steward_store::{
    BrowserRbacAssignment, BrowserRbacAssignmentAction, BrowserRbacAssignmentChange,
    BrowserRbacAssignments, CanonicalUserRecord, PgStore, StoreError,
};
use steward_types::{CanonicalUserId, Email};

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
    pub state: String,
    pub administrator: bool,
    pub member_roles: Vec<String>,
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
        members.push(member_view(user, assignments));
    }
    Json(BrowserMembersResponse {
        api_version: BROWSER_MEMBERS_API_VERSION,
        members,
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
        member: member_view(user, assignments),
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
    Json(BrowserMemberResponse {
        api_version: BROWSER_MEMBERS_API_VERSION,
        member: member_view(user, assignments),
    })
    .into_response()
}

fn member_view(
    user: CanonicalUserRecord,
    assignments: BrowserRbacAssignments,
) -> BrowserMemberView {
    BrowserMemberView {
        user_id: user.user_id.as_str().to_owned(),
        display_email: user.display_email.as_str().to_owned(),
        state: user.state,
        administrator: assignments.is_admin,
        member_roles: assignments.member_roles,
    }
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
