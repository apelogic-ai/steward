//! Browser-session-bound Agent Runs read APIs.
//!
//! The user route derives its exact owner scope from the authenticated browser session. It never
//! accepts a client-provided identity and therefore cannot be widened by the page. The separate
//! All Runs route requires a browser-admin session; the existing bearer administrator API remains
//! independent at `/admin/api/v1/runs`.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use steward_store::{AgentRunPage, AgentRunQuery, AgentRunRecord, StoreError};
use steward_types::{CanonicalUserId, RuntimeOwnership, TaskPhase};
use uuid::Uuid;

use crate::browser_auth::{
    BrowserAdminAuthority, BrowserAuthService, BrowserSessionContext, protect_browser_admin_routes,
    protect_browser_routes,
};
use crate::{AgentRunLedger, AgentRunSpendView, bounded_task_error_category};

pub const BROWSER_AGENT_RUNS_API_VERSION: &str = "steward.browser-runs/v1";

#[derive(Clone)]
struct BrowserRunsState<L> {
    ledger: L,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BrowserRunsQuery {
    #[serde(default = "default_limit")]
    limit: u16,
    cursor: Option<Uuid>,
    phase: Option<TaskPhase>,
    workflow: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AllRunsQuery {
    #[serde(default = "default_limit")]
    limit: u16,
    cursor: Option<Uuid>,
    phase: Option<TaskPhase>,
    workflow: Option<String>,
    owner_user_id: Option<CanonicalUserId>,
}

const fn default_limit() -> u16 {
    50
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct BrowserRunView {
    task_uid: Uuid,
    workflow: String,
    coding_agent_runtime: String,
    runtime_uid: Option<String>,
    runtime_ownership: RuntimeOwnership,
    phase: TaskPhase,
    envelope_revision: Option<i64>,
    finalization_requested: bool,
    finalized: bool,
    created_at: String,
    updated_at: String,
    observed_spend: Option<AgentRunSpendView>,
    error_category: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct MyRunsResponse {
    api_version: &'static str,
    runs: Vec<BrowserRunView>,
    next_cursor: Option<Uuid>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct AllRunsView {
    #[serde(flatten)]
    run: BrowserRunView,
    /// Opaque canonical identifier only; display email and acting-user identities stay server-side.
    owner_user_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct AllRunsResponse {
    api_version: &'static str,
    runs: Vec<AllRunsView>,
    next_cursor: Option<Uuid>,
}

fn my_runs_router<L>(ledger: L) -> Router
where
    L: AgentRunLedger,
{
    Router::new()
        .route("/app/api/v1/runs", get(my_runs::<L>))
        .with_state(BrowserRunsState { ledger })
}

fn all_runs_router<L>(ledger: L) -> Router
where
    L: AgentRunLedger,
{
    Router::new()
        .route("/admin/api/v1/all-runs", get(all_runs::<L>))
        .with_state(BrowserRunsState { ledger })
}

/// Mount browser-session-bound Runs APIs.
///
/// `GET /app/api/v1/runs` is exact-identity scoped to the browser principal. `GET
/// /admin/api/v1/all-runs` is browser-admin only and may optionally filter an opaque canonical
/// owner identifier. Neither response exposes emails, provider credentials, commands, prompts or
/// raw failure data.
pub fn protected_router<L>(ledger: L, browser_auth: BrowserAuthService) -> Router
where
    L: AgentRunLedger,
{
    protect_browser_routes(my_runs_router(ledger.clone()), browser_auth.clone()).merge(
        protect_browser_admin_routes(all_runs_router(ledger), browser_auth),
    )
}

async fn my_runs<L>(
    session: Option<Extension<BrowserSessionContext>>,
    State(state): State<BrowserRunsState<L>>,
    Query(query): Query<BrowserRunsQuery>,
) -> Response
where
    L: AgentRunLedger,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let query = AgentRunQuery {
        limit: query.limit,
        cursor: query.cursor,
        phase: query.phase,
        workflow: query.workflow,
        owner_user_id: Some(session.principal.canonical_user_id.as_str().to_owned()),
    };
    match state.ledger.agent_runs(&query).await {
        Ok(page) => Json(MyRunsResponse {
            api_version: BROWSER_AGENT_RUNS_API_VERSION,
            runs: page.records.into_iter().map(browser_run_view).collect(),
            next_cursor: page.next_cursor,
        })
        .into_response(),
        Err(StoreError::InvalidRunQuery | StoreError::InvalidRunCursor) => {
            browser_runs_error(StatusCode::BAD_REQUEST)
        }
        Err(_) => browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
    }
}

async fn all_runs<L>(
    authority: Option<Extension<BrowserAdminAuthority>>,
    State(state): State<BrowserRunsState<L>>,
    Query(query): Query<AllRunsQuery>,
) -> Response
where
    L: AgentRunLedger,
{
    if authority.is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let query = AgentRunQuery {
        limit: query.limit,
        cursor: query.cursor,
        phase: query.phase,
        workflow: query.workflow,
        owner_user_id: query.owner_user_id.map(|id| id.as_str().to_owned()),
    };
    match state.ledger.agent_runs(&query).await {
        Ok(AgentRunPage {
            records,
            next_cursor,
        }) => Json(AllRunsResponse {
            api_version: BROWSER_AGENT_RUNS_API_VERSION,
            runs: records
                .into_iter()
                .map(|record| AllRunsView {
                    owner_user_id: record.owner_user_id.clone(),
                    run: browser_run_view(record),
                })
                .collect(),
            next_cursor,
        })
        .into_response(),
        Err(StoreError::InvalidRunQuery | StoreError::InvalidRunCursor) => {
            browser_runs_error(StatusCode::BAD_REQUEST)
        }
        Err(_) => browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
    }
}

fn browser_run_view(record: AgentRunRecord) -> BrowserRunView {
    BrowserRunView {
        task_uid: record.task_uid,
        workflow: record.workflow,
        coding_agent_runtime: record.coding_agent_runtime,
        runtime_uid: record.runtime_uid,
        runtime_ownership: record.runtime_ownership,
        phase: record.phase,
        envelope_revision: record.envelope_revision,
        finalization_requested: record.finalize_requested,
        finalized: record.finalized,
        created_at: record.created_at,
        updated_at: record.updated_at,
        observed_spend: record.spend.map(|spend| AgentRunSpendView {
            observed_amount: spend.observed_amount,
            currency: spend.currency,
            exhausted: spend.exhausted,
        }),
        error_category: bounded_task_error_category(record.failure_reason.as_deref())
            .map(str::to_owned),
    }
}

fn browser_runs_error(status: StatusCode) -> Response {
    (
        status,
        Json(serde_json::json!({ "error": "agent-runs query is unavailable" })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use steward_store::{AgentRunSpend, AgentRunTimelineEvent};
    use steward_types::{
        AgentRuntimeSpec, AgentType, Budget, Duration, Email, ModelRef, Principal,
    };
    use tower::ServiceExt;

    use super::*;
    use crate::BoxFuture;
    use crate::browser_auth::{
        LocalFakeIdentity, browser_auth_router, local_fake_browser_auth_service,
    };

    #[derive(Clone, Default)]
    struct FakeLedger {
        records: Arc<Mutex<Vec<AgentRunRecord>>>,
        queries: Arc<Mutex<Vec<AgentRunQuery>>>,
    }

    impl AgentRunLedger for FakeLedger {
        fn agent_runs<'a>(
            &'a self,
            query: &'a AgentRunQuery,
        ) -> BoxFuture<'a, Result<AgentRunPage, StoreError>> {
            Box::pin(async move {
                self.queries
                    .lock()
                    .map_err(|_| StoreError::InvalidRunQuery)?
                    .push(query.clone());
                let records = self
                    .records
                    .lock()
                    .map_err(|_| StoreError::InvalidRunQuery)?
                    .iter()
                    .filter(|record| {
                        query.owner_user_id.as_ref().is_none_or(|owner| {
                            record.owner_user_id.as_deref() == Some(owner.as_str())
                        })
                    })
                    .cloned()
                    .collect();
                Ok(AgentRunPage {
                    records,
                    next_cursor: None,
                })
            })
        }

        fn agent_run<'a>(
            &'a self,
            _task_uid: Uuid,
        ) -> BoxFuture<'a, Result<Option<AgentRunRecord>, StoreError>> {
            Box::pin(async { Ok(None) })
        }

        fn agent_run_timeline<'a>(
            &'a self,
            _task_uid: Uuid,
        ) -> BoxFuture<'a, Result<Option<Vec<AgentRunTimelineEvent>>, StoreError>> {
            Box::pin(async { Ok(None) })
        }
    }

    fn run(task_uid: Uuid, owner_user_id: &str) -> AgentRunRecord {
        AgentRunRecord {
            task_uid,
            submitter_service: "steward-run".to_owned(),
            acting_user: Some("alice@example.com".to_owned()),
            owner: "alice@example.com".to_owned(),
            owner_user_id: Some(owner_user_id.to_owned()),
            workflow: "code-review".to_owned(),
            coding_agent_runtime: "agent-v1".to_owned(),
            runtime_uid: Some(format!("runtime-{task_uid}")),
            runtime_ownership: RuntimeOwnership::Provisioned,
            phase: TaskPhase::Succeeded,
            runtime_spec: AgentRuntimeSpec {
                principal: Principal::Service {
                    name: "steward-run".to_owned(),
                    acting_user: Some(Email("alice@example.com".to_owned())),
                },
                owner: Email("alice@example.com".to_owned()),
                canonical_authority: None,
                agent_type: AgentType {
                    name: "agent-v1".to_owned(),
                },
                llms: vec![ModelRef {
                    provider: "provider-a".to_owned(),
                    model: "model-a".to_owned(),
                }],
                tools: Vec::new(),
                budget: Budget {
                    monthly_limit: "100.00".to_owned(),
                    currency: "USD".to_owned(),
                },
                ttl: Duration("24h".to_owned()),
                bindings: None,
            },
            envelope_revision: Some(1),
            finalize_requested: false,
            finalized: false,
            failure_reason: Some("provider returned private details".to_owned()),
            created_at: "2026-08-17T00:00:00.000000Z".to_owned(),
            updated_at: "2026-08-17T00:00:00.000000Z".to_owned(),
            spend: Some(AgentRunSpend {
                observed_amount: "1.25".to_owned(),
                currency: "USD".to_owned(),
                exhausted: false,
                observed_at: "2026-08-17T00:00:00.000000Z".to_owned(),
            }),
            history_partial: false,
        }
    }

    fn cookie(response: &Response, name: &str) -> Result<String, String> {
        response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find(|value| value.starts_with(&format!("{name}=")))
            .and_then(|value| value.split(';').next())
            .map(str::to_owned)
            .ok_or_else(|| format!("response omitted {name} cookie"))
    }

    async fn signed_in_cookie(
        identity: LocalFakeIdentity,
    ) -> Result<(BrowserAuthService, String), String> {
        let service = local_fake_browser_auth_service("http://127.0.0.1:33001", identity)?;
        let login = browser_auth_router(service.clone())
            .oneshot(
                Request::builder()
                    .uri("/admin/auth/login")
                    .body(Body::empty())
                    .map_err(|error| format!("build login request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute login request: {error}"))?;
        let flow_cookie = cookie(&login, "steward-local-oidc-flow")?;
        let authorize = login
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| "login omitted authorize redirect".to_owned())?;
        let authorized = browser_auth_router(service.clone())
            .oneshot(
                Request::builder()
                    .uri(authorize)
                    .body(Body::empty())
                    .map_err(|error| format!("build authorize request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute authorize request: {error}"))?;
        let callback = authorized
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| "authorize omitted callback redirect".to_owned())?;
        let callback = browser_auth_router(service.clone())
            .oneshot(
                Request::builder()
                    .uri(callback)
                    .header(header::COOKIE, flow_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build callback request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute callback request: {error}"))?;
        Ok((service, cookie(&callback, "steward-local-session")?))
    }

    #[tokio::test]
    async fn my_runs_is_bound_to_the_browser_canonical_identity_and_hides_identity_fields()
    -> Result<(), String> {
        let owner = "usr_0123456789abcdef0123456789abcdef";
        let other_owner = "usr_abcdefabcdefabcdefabcdefabcdefab";
        let ledger = FakeLedger::default();
        ledger.records.lock().map_err(|_| "lock records")?.extend([
            run(
                Uuid::parse_str("11111111-1111-4111-8111-111111111111")
                    .map_err(|error| error.to_string())?,
                owner,
            ),
            run(
                Uuid::parse_str("22222222-2222-4222-8222-222222222222")
                    .map_err(|error| error.to_string())?,
                other_owner,
            ),
        ]);
        let (service, session_cookie) = signed_in_cookie(LocalFakeIdentity::User).await?;
        let response = protected_router(ledger.clone(), service)
            .oneshot(
                Request::builder()
                    .uri(format!("/app/api/v1/runs?ownerUserId={other_owner}"))
                    .header(header::COOKIE, session_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build my-runs request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute my-runs request: {error}"))?;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(
            ledger
                .queries
                .lock()
                .map_err(|_| "lock queries")?
                .is_empty()
        );

        let (service, session_cookie) = signed_in_cookie(LocalFakeIdentity::User).await?;
        let response = protected_router(ledger.clone(), service)
            .oneshot(
                Request::builder()
                    .uri("/app/api/v1/runs")
                    .header(header::COOKIE, session_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build scoped my-runs request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute scoped my-runs request: {error}"))?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 16 * 1024)
            .await
            .map_err(|error| format!("read my-runs response: {error}"))?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("parse my-runs response: {error}"))?;
        assert_eq!(value["runs"].as_array().map(Vec::len), Some(1));
        assert_eq!(
            value["runs"][0]["taskUid"],
            "11111111-1111-4111-8111-111111111111"
        );
        assert!(value["runs"][0].get("ownerUserId").is_none());
        assert!(!value.to_string().contains("alice@example.com"));
        assert_eq!(
            ledger.queries.lock().map_err(|_| "lock queries")?[0]
                .owner_user_id
                .as_deref(),
            Some(owner)
        );
        Ok(())
    }

    #[tokio::test]
    async fn all_runs_requires_browser_admin_and_returns_only_opaque_owner_identity()
    -> Result<(), String> {
        let owner = "usr_0123456789abcdef0123456789abcdef";
        let ledger = FakeLedger::default();
        ledger.records.lock().map_err(|_| "lock records")?.push(run(
            Uuid::parse_str("11111111-1111-4111-8111-111111111111")
                .map_err(|error| error.to_string())?,
            owner,
        ));
        let (user_service, user_cookie) = signed_in_cookie(LocalFakeIdentity::User).await?;
        let user = protected_router(ledger.clone(), user_service)
            .oneshot(
                Request::builder()
                    .uri("/admin/api/v1/all-runs")
                    .header(header::COOKIE, user_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build user all-runs request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute user all-runs request: {error}"))?;
        assert_eq!(user.status(), StatusCode::FORBIDDEN);

        let (admin_service, admin_cookie) = signed_in_cookie(LocalFakeIdentity::Admin).await?;
        let admin = protected_router(ledger.clone(), admin_service)
            .oneshot(
                Request::builder()
                    .uri(format!("/admin/api/v1/all-runs?ownerUserId={owner}"))
                    .header(header::COOKIE, admin_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build admin all-runs request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute admin all-runs request: {error}"))?;
        assert_eq!(admin.status(), StatusCode::OK);
        let body = to_bytes(admin.into_body(), 16 * 1024)
            .await
            .map_err(|error| format!("read admin all-runs response: {error}"))?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("parse admin all-runs response: {error}"))?;
        assert_eq!(value["runs"][0]["ownerUserId"], owner);
        assert!(!value.to_string().contains("alice@example.com"));
        assert_eq!(
            ledger.queries.lock().map_err(|_| "lock queries")?[0]
                .owner_user_id
                .as_deref(),
            Some(owner)
        );
        Ok(())
    }
}
