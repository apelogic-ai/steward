//! Read-only administrator setup diagnostics derived from live Steward state.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, Router};
use serde::Serialize;
use steward_store::{AgentRunQuery, PgStore};

use crate::bounded_task_error_category;
use crate::browser_admin::CapabilityCatalog;
use crate::browser_auth::{
    BrowserAdminAuthority, BrowserAuthService, protect_browser_admin_routes,
};

const ADMIN_SETUP_API_VERSION: &str = "steward.admin-setup/v1";
const CONNECTION_RESPONSE_DEADLINE_MS: i64 =
    crate::governed_connections::CONNECTION_RESPONSE_DEADLINE_SECONDS * 1_000;
const CONNECTION_NEAR_DEADLINE_MS: i64 = CONNECTION_RESPONSE_DEADLINE_MS * 3 / 4;
const INSTALLATION_GUIDE: &str =
    "https://github.com/apelogic-ai/steward/blob/main/docs/installation/installation-guide.md";
const EXECUTION_BINDINGS_GUIDE: &str =
    "https://github.com/apelogic-ai/steward/blob/main/docs/installation/execution-bindings.md";

#[derive(Clone)]
pub struct AdminSetupConfig {
    pub orchestration_active: bool,
    pub execution_bindings_active: bool,
    pub resolvable_execution_bindings: usize,
    pub task_identity_discovery_enabled: bool,
    pub github_source_enabled: bool,
    pub github_actor_issuer: Option<String>,
    pub capability_catalog: CapabilityCatalog,
}

#[derive(Clone)]
pub(crate) struct AdminSetupState {
    store: PgStore,
    config: AdminSetupConfig,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum AdminSetupCheckId {
    Orchestration,
    GithubConnect,
    CapabilityCatalog,
    Templates,
    Members,
    GithubActions,
    RunNow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AdminSetupCheckStatus {
    Ready,
    Attention,
    Unknown,
    NotConfigured,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AdminSetupCheck {
    pub id: AdminSetupCheckId,
    pub title: &'static str,
    pub status: AdminSetupCheckStatus,
    pub detail: String,
    pub fix_href: &'static str,
    pub optional: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AdminSetupStatusResponse {
    pub api_version: &'static str,
    pub checks: Vec<AdminSetupCheck>,
}

struct SetupFacts {
    active_other_members: usize,
    member_ready_templates: usize,
    connection_start_duration_ms: Option<i64>,
    latest_submission_error_category: Option<&'static str>,
    unassociated_github_actors: i64,
}

pub fn protected_router(
    store: PgStore,
    config: AdminSetupConfig,
    auth: BrowserAuthService,
) -> Router {
    protect_browser_admin_routes(
        Router::new()
            .route("/admin/api/v1/setup-status", get(get_setup_status))
            .with_state(AdminSetupState { store, config }),
        auth,
    )
}

#[utoipa::path(
    get,
    operation_id = "getAdminSetupStatus",
    path = "/admin/api/v1/setup-status",
    responses(
        (status = 200, body = AdminSetupStatusResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role is required"),
        (status = 503, description = "Authoritative setup state is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn get_setup_status(
    Extension(authority): Extension<BrowserAdminAuthority>,
    State(state): State<AdminSetupState>,
) -> Response {
    let users = match state.store.canonical_users().await {
        Ok(users) => users,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let templates = match state.store.latest_envelope_templates().await {
        Ok(templates) => templates,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let owner = &authority.principal().canonical_user_id;
    let connection_start_duration_ms = match state
        .store
        .latest_successful_connection_start_duration_ms(owner)
        .await
    {
        Ok(duration) => duration,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let latest_run = match state
        .store
        .agent_runs(&AgentRunQuery {
            limit: 1,
            cursor: None,
            phase: None,
            workflow: None,
            owner_user_id: Some(owner.as_str().to_owned()),
            runtime_uid: None,
            user_envelope_instance_id: None,
            task_uid: None,
        })
        .await
    {
        Ok(page) => page.records.into_iter().next(),
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let unassociated_github_actors = match state.config.github_actor_issuer.as_deref() {
        Some(issuer) => match state
            .store
            .unassociated_federated_subject_count(issuer)
            .await
        {
            Ok(count) => count,
            Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        },
        None => 0,
    };
    let facts = SetupFacts {
        active_other_members: users
            .iter()
            .filter(|user| user.state == "active" && user.user_id != *owner)
            .count(),
        member_ready_templates: templates
            .iter()
            .filter(|template| {
                !template.member_roles.is_empty()
                    && !template.ceiling.spec.llms.is_empty()
                    && !template.ceiling.spec.tools.is_empty()
            })
            .count(),
        connection_start_duration_ms,
        latest_submission_error_category: latest_run
            .as_ref()
            .and_then(|run| bounded_task_error_category(run.failure_reason.as_deref())),
        unassociated_github_actors,
    };
    Json(build_status(&state.config, facts)).into_response()
}

fn check(
    id: AdminSetupCheckId,
    title: &'static str,
    status: AdminSetupCheckStatus,
    detail: impl Into<String>,
    fix_href: &'static str,
    optional: bool,
) -> AdminSetupCheck {
    AdminSetupCheck {
        id,
        title,
        status,
        detail: detail.into(),
        fix_href,
        optional,
    }
}

fn build_status(config: &AdminSetupConfig, facts: SetupFacts) -> AdminSetupStatusResponse {
    let orchestration = if !config.orchestration_active {
        check(
            AdminSetupCheckId::Orchestration,
            "Orchestration",
            AdminSetupCheckStatus::Attention,
            "Task orchestration is staged.",
            EXECUTION_BINDINGS_GUIDE,
            false,
        )
    } else if !config.execution_bindings_active {
        check(
            AdminSetupCheckId::Orchestration,
            "Orchestration",
            AdminSetupCheckStatus::Attention,
            "Execution bindings are configured but not active.",
            EXECUTION_BINDINGS_GUIDE,
            false,
        )
    } else if config.resolvable_execution_bindings == 0 {
        check(
            AdminSetupCheckId::Orchestration,
            "Orchestration",
            AdminSetupCheckStatus::Attention,
            "No active execution binding resolves to a configured agent adapter.",
            EXECUTION_BINDINGS_GUIDE,
            false,
        )
    } else {
        check(
            AdminSetupCheckId::Orchestration,
            "Orchestration",
            AdminSetupCheckStatus::Ready,
            format!(
                "Orchestration and {} resolvable execution binding(s) are active.",
                config.resolvable_execution_bindings
            ),
            EXECUTION_BINDINGS_GUIDE,
            false,
        )
    };

    let github_connect = match facts.connection_start_duration_ms {
        None => check(
            AdminSetupCheckId::GithubConnect,
            "GitHub Connect",
            AdminSetupCheckStatus::Attention,
            "No successful GitHub Connect operation has been recorded for this administrator.",
            "/connections",
            false,
        ),
        Some(duration) if duration >= CONNECTION_NEAR_DEADLINE_MS => check(
            AdminSetupCheckId::GithubConnect,
            "GitHub Connect",
            AdminSetupCheckStatus::Attention,
            format!(
                "The latest Connect start succeeded in {duration} ms, near the {CONNECTION_RESPONSE_DEADLINE_MS} ms response deadline."
            ),
            "/connections",
            false,
        ),
        Some(duration) => check(
            AdminSetupCheckId::GithubConnect,
            "GitHub Connect",
            AdminSetupCheckStatus::Ready,
            format!("The latest Connect start succeeded in {duration} ms."),
            "/connections",
            false,
        ),
    };

    let published_tools = config.capability_catalog.tools.len();
    let capability_catalog = check(
        AdminSetupCheckId::CapabilityCatalog,
        "Capability catalog",
        AdminSetupCheckStatus::Unknown,
        format!(
            "{published_tools} tool(s) are published. Capability-catalog v2 does not report an expected tool count or whether it was generated from the gateway's published catalog."
        ),
        "https://github.com/apelogic-ai/steward/issues/234",
        false,
    );
    let templates = if facts.member_ready_templates == 0 {
        check(
            AdminSetupCheckId::Templates,
            "Member-ready template",
            AdminSetupCheckStatus::Attention,
            "No template grants member roles, models, and tools together.",
            "/admin/envelopes/templates",
            false,
        )
    } else {
        check(
            AdminSetupCheckId::Templates,
            "Member-ready template",
            AdminSetupCheckStatus::Ready,
            format!(
                "{} member-ready template(s) are published.",
                facts.member_ready_templates
            ),
            "/admin/envelopes/templates",
            false,
        )
    };
    let members = if facts.active_other_members == 0 {
        check(
            AdminSetupCheckId::Members,
            "Members",
            AdminSetupCheckStatus::Attention,
            "No active member besides this administrator is registered.",
            "https://github.com/apelogic-ai/steward/issues/231",
            false,
        )
    } else {
        check(
            AdminSetupCheckId::Members,
            "Members",
            AdminSetupCheckStatus::Ready,
            format!(
                "{} other active member(s) are registered.",
                facts.active_other_members
            ),
            "https://github.com/apelogic-ai/steward/issues/231",
            false,
        )
    };
    let github_actions = if !config.task_identity_discovery_enabled {
        check(
            AdminSetupCheckId::GithubActions,
            "GitHub Actions automation",
            AdminSetupCheckStatus::NotConfigured,
            "Task identity discovery is not configured.",
            INSTALLATION_GUIDE,
            true,
        )
    } else if !config.github_source_enabled {
        check(
            AdminSetupCheckId::GithubActions,
            "GitHub Actions automation",
            AdminSetupCheckStatus::Attention,
            "Discovery metadata is served, but githubSource is disabled for in-repository packages.",
            INSTALLATION_GUIDE,
            true,
        )
    } else if facts.unassociated_github_actors > 0 {
        check(
            AdminSetupCheckId::GithubActions,
            "GitHub Actions automation",
            AdminSetupCheckStatus::Attention,
            format!(
                "{} observed GitHub actor(s) still need canonical-user association.",
                facts.unassociated_github_actors
            ),
            "https://github.com/apelogic-ai/steward/issues/231",
            true,
        )
    } else if let Some(category) = facts.latest_submission_error_category {
        check(
            AdminSetupCheckId::GithubActions,
            "GitHub Actions automation",
            AdminSetupCheckStatus::Attention,
            format!("The latest owned submission has error category {category}."),
            "/admin/runs",
            true,
        )
    } else {
        check(
            AdminSetupCheckId::GithubActions,
            "GitHub Actions automation",
            AdminSetupCheckStatus::Ready,
            "Discovery metadata and githubSource are configured; no bounded error category is present on the latest owned submission.",
            "/admin/runs",
            true,
        )
    };
    let run_now = check(
        AdminSetupCheckId::RunNow,
        "Run now",
        AdminSetupCheckStatus::NotConfigured,
        "Run now is tracked separately and is not available in this release.",
        "https://github.com/apelogic-ai/steward/issues/227",
        true,
    );

    AdminSetupStatusResponse {
        api_version: ADMIN_SETUP_API_VERSION,
        checks: vec![
            orchestration,
            github_connect,
            capability_catalog,
            templates,
            members,
            github_actions,
            run_now,
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog(tool_count: usize) -> CapabilityCatalog {
        CapabilityCatalog {
            schema_version: "steward.capability-catalog/v2".to_owned(),
            models: Vec::new(),
            tools: (0..tool_count)
                .map(|index| crate::browser_admin::CapabilityTool {
                    provider: "provider-a".to_owned(),
                    resource: format!("resource-{index}"),
                    action: "read".to_owned(),
                    access_class: crate::browser_admin::ToolAccessClass::Read,
                })
                .collect(),
            catalogs: Vec::new(),
        }
    }

    #[test]
    fn missing_prerequisites_have_exact_reasons_and_fix_links() {
        let status = build_status(
            &AdminSetupConfig {
                orchestration_active: false,
                execution_bindings_active: false,
                resolvable_execution_bindings: 0,
                task_identity_discovery_enabled: false,
                github_source_enabled: false,
                github_actor_issuer: None,
                capability_catalog: catalog(0),
            },
            SetupFacts {
                active_other_members: 0,
                member_ready_templates: 0,
                connection_start_duration_ms: None,
                latest_submission_error_category: None,
                unassociated_github_actors: 0,
            },
        );
        assert_eq!(status.checks.len(), 7);
        assert_eq!(status.checks[0].status, AdminSetupCheckStatus::Attention);
        assert_eq!(status.checks[0].detail, "Task orchestration is staged.");
        assert_eq!(status.checks[1].fix_href, "/connections");
        assert_eq!(status.checks[2].status, AdminSetupCheckStatus::Unknown);
        assert!(
            status.checks[2]
                .detail
                .contains("does not report an expected tool count")
        );
        assert_eq!(
            status.checks[5].status,
            AdminSetupCheckStatus::NotConfigured
        );
    }

    #[test]
    fn fixed_prerequisites_turn_ready_and_slow_connect_stays_actionable() {
        let config = AdminSetupConfig {
            orchestration_active: true,
            execution_bindings_active: true,
            resolvable_execution_bindings: 2,
            task_identity_discovery_enabled: true,
            github_source_enabled: true,
            github_actor_issuer: Some("https://identity.example.com".to_owned()),
            capability_catalog: catalog(3),
        };
        let ready = build_status(
            &config,
            SetupFacts {
                active_other_members: 2,
                member_ready_templates: 1,
                connection_start_duration_ms: Some(1_250),
                latest_submission_error_category: None,
                unassociated_github_actors: 0,
            },
        );
        for index in [0, 1, 3, 4, 5] {
            assert_eq!(ready.checks[index].status, AdminSetupCheckStatus::Ready);
        }
        let slow = build_status(
            &config,
            SetupFacts {
                active_other_members: 2,
                member_ready_templates: 1,
                connection_start_duration_ms: Some(CONNECTION_NEAR_DEADLINE_MS),
                latest_submission_error_category: None,
                unassociated_github_actors: 0,
            },
        );
        assert_eq!(slow.checks[1].status, AdminSetupCheckStatus::Attention);
        assert!(
            slow.checks[1]
                .detail
                .contains("near the 40000 ms response deadline")
        );
    }

    #[test]
    fn automation_reports_association_and_only_bounded_run_errors() {
        let config = AdminSetupConfig {
            orchestration_active: true,
            execution_bindings_active: true,
            resolvable_execution_bindings: 1,
            task_identity_discovery_enabled: true,
            github_source_enabled: true,
            github_actor_issuer: Some("https://identity.example.com".to_owned()),
            capability_catalog: catalog(1),
        };
        let unassociated = build_status(
            &config,
            SetupFacts {
                active_other_members: 1,
                member_ready_templates: 1,
                connection_start_duration_ms: Some(1_000),
                latest_submission_error_category: Some("other"),
                unassociated_github_actors: 2,
            },
        );
        assert_eq!(
            unassociated.checks[5].detail,
            "2 observed GitHub actor(s) still need canonical-user association."
        );

        let failed = build_status(
            &config,
            SetupFacts {
                active_other_members: 1,
                member_ready_templates: 1,
                connection_start_duration_ms: Some(1_000),
                latest_submission_error_category: Some("sandbox-execution"),
                unassociated_github_actors: 0,
            },
        );
        assert_eq!(
            failed.checks[5].detail,
            "The latest owned submission has error category sandbox-execution."
        );
    }
}
