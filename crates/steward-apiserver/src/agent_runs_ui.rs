//! Browser-session-bound Agent Runs read APIs.
//!
//! The user route derives its exact owner scope from the authenticated browser session. It never
//! accepts a client-provided identity and therefore cannot be widened by the page. The separate
//! All Runs route requires a browser-admin session; the existing bearer administrator API remains
//! independent at `/admin/api/v1/runs`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use steward_store::{
    AgentRunExecutionLog, AgentRunLogStream, AgentRunPage, AgentRunQuery, AgentRunRecord,
    AgentRunTimelineEvent, AgentRunTimelineKind, StoreError, TaskRecord,
};
use steward_types::direct_package::{ExecutionLogMode, TaskOrigin};
use steward_types::{CanonicalUserId, RuntimeOwnership, TaskPhase};
use uuid::Uuid;

use crate::browser_auth::{
    BrowserAdminAuthority, BrowserAuthService, BrowserMutationProof, BrowserSessionBinding,
    BrowserSessionContext, protect_browser_admin_routes, protect_browser_routes,
};
use crate::connections::{
    ConnectionBrokerError, ConnectionSession, ConnectionSubject, GithubWorkflowRerunBroker,
    GithubWorkflowRerunRequest,
};
use crate::tasks::{BrowserTaskRerunError, BrowserTaskRerunner};
use crate::{AgentRunLedger, AgentRunSpendView, BoxFuture, bounded_task_error_category};

pub const BROWSER_AGENT_RUNS_API_VERSION: &str = "steward.browser-runs/v1";

#[derive(Serialize, utoipa::ToSchema)]
struct RerunErrorResponse {
    error: &'static str,
}

#[derive(Clone)]
pub(crate) struct BrowserRunsState<L> {
    ledger: L,
    github_rerunner: Arc<dyn BrowserGithubRerunner>,
    browser_task_rerunner: Arc<dyn BrowserTaskRerunner>,
}

trait BrowserGithubRerunner: Send + Sync {
    fn rerun<'a>(
        &'a self,
        session: &'a BrowserSessionContext,
        request: &'a GithubWorkflowRerunRequest,
    ) -> BoxFuture<'a, Result<(), ConnectionBrokerError>>;
}

impl<P> BrowserGithubRerunner for P
where
    P: GithubWorkflowRerunBroker<BrowserSessionBinding>,
{
    fn rerun<'a>(
        &'a self,
        session: &'a BrowserSessionContext,
        request: &'a GithubWorkflowRerunRequest,
    ) -> BoxFuture<'a, Result<(), ConnectionBrokerError>> {
        Box::pin(async move {
            let connection = ConnectionSession {
                subject: ConnectionSubject {
                    canonical_user_id: session.principal.canonical_user_id.clone(),
                    display_email: session.principal.display_email.as_str().to_owned(),
                },
                binding: session.binding.clone(),
            };
            GithubWorkflowRerunBroker::rerun(self, &connection, request).await
        })
    }
}

#[derive(Clone, Copy)]
struct DisabledGithubRerunner;

impl BrowserGithubRerunner for DisabledGithubRerunner {
    fn rerun<'a>(
        &'a self,
        _session: &'a BrowserSessionContext,
        _request: &'a GithubWorkflowRerunRequest,
    ) -> BoxFuture<'a, Result<(), ConnectionBrokerError>> {
        Box::pin(async { Err(ConnectionBrokerError::Unavailable) })
    }
}

#[derive(Clone, Copy)]
struct DisabledBrowserTaskRerunner;

impl BrowserTaskRerunner for DisabledBrowserTaskRerunner {
    fn rerun<'a>(
        &'a self,
        _session: &'a BrowserSessionContext,
        _source: &'a TaskRecord,
        _idempotency_key: &'a str,
    ) -> BoxFuture<'a, Result<Uuid, BrowserTaskRerunError>> {
        Box::pin(async { Err(BrowserTaskRerunError::Unavailable) })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct BrowserRunsQuery {
    #[serde(default = "default_limit")]
    limit: u16,
    cursor: Option<Uuid>,
    phase: Option<TaskPhase>,
    workflow: Option<String>,
    runtime_uid: Option<String>,
    envelope_instance_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AllRunsQuery {
    #[serde(default = "default_limit")]
    limit: u16,
    cursor: Option<Uuid>,
    phase: Option<TaskPhase>,
    workflow: Option<String>,
    owner_user_id: Option<CanonicalUserId>,
    runtime_uid: Option<String>,
}

const fn default_limit() -> u16 {
    50
}

#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserRunView {
    #[schema(value_type = String, format = "uuid")]
    task_uid: Uuid,
    origin: TaskOrigin,
    package: Option<BrowserRunPackageView>,
    workflow: String,
    workflow_name: Option<String>,
    workflow_version: Option<i64>,
    workflow_digest: Option<String>,
    user_envelope_instance_id: Option<String>,
    user_envelope_revision: Option<i64>,
    user_envelope_digest: Option<String>,
    coding_agent_runtime: String,
    runtime_uid: Option<String>,
    runtime_ownership: RuntimeOwnership,
    phase: TaskPhase,
    finalization_requested: bool,
    finalized: bool,
    created_at: String,
    updated_at: String,
    observed_spend: Option<AgentRunSpendView>,
    error_category: Option<String>,
    trigger: Option<BrowserRunTrigger>,
    execution_log: ExecutionLogMode,
    rerun_supported: bool,
    rerun_unavailable_reason: Option<&'static str>,
    stages: Vec<BrowserRunStage>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserRunPackageView {
    source: String,
    revision: String,
    path: String,
    content_digest: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserRunPackageContentResponse {
    #[schema(value_type = String, format = "uuid")]
    task_uid: Uuid,
    files: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserRunTrigger {
    provider: &'static str,
    repository: String,
    event: String,
    actor: String,
    #[serde(rename = "ref")]
    git_ref: String,
    sha: String,
    run_id: String,
    run_attempt: u32,
    run_url: String,
    caller_workflow: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BrowserRunStageState {
    Pending,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BrowserRunExitCategory {
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BrowserRunStageId {
    Admission,
    ProvisionRuntime,
    AgentExecution,
    Finalize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserRunStage {
    id: BrowserRunStageId,
    display_name: &'static str,
    state: BrowserRunStageState,
    steps: Vec<BrowserRunStep>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserRunStep {
    id: &'static str,
    display_name: &'static str,
    state: BrowserRunStageState,
    log_streams: Vec<&'static str>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserRunFacets {
    phase: BrowserRunPhaseFacets,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) struct BrowserRunPhaseFacets {
    submitted: u64,
    parked: u64,
    queued: u64,
    running: u64,
    succeeded: u64,
    failed: u64,
    cancelled: u64,
}

impl BrowserRunPhaseFacets {
    fn from_counts(counts: &std::collections::BTreeMap<String, u64>) -> Self {
        Self {
            submitted: counts.get("submitted").copied().unwrap_or_default(),
            parked: counts.get("parked").copied().unwrap_or_default(),
            queued: counts.get("queued").copied().unwrap_or_default(),
            running: counts.get("running").copied().unwrap_or_default(),
            succeeded: counts.get("succeeded").copied().unwrap_or_default(),
            failed: counts.get("failed").copied().unwrap_or_default(),
            cancelled: counts.get("cancelled").copied().unwrap_or_default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MyRunsResponse {
    api_version: &'static str,
    runs: Vec<BrowserRunView>,
    #[schema(value_type = Option<String>, format = "uuid")]
    next_cursor: Option<Uuid>,
    facets: BrowserRunFacets,
}

#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AllRunsView {
    #[serde(flatten)]
    run: BrowserRunView,
    /// Opaque canonical identifier only; display email and acting-user identities stay server-side.
    owner_user_id: Option<String>,
    owner_display_email: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AllRunsResponse {
    api_version: &'static str,
    runs: Vec<AllRunsView>,
    #[schema(value_type = Option<String>, format = "uuid")]
    next_cursor: Option<Uuid>,
    facets: BrowserRunFacets,
}

#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserRunResponse {
    api_version: &'static str,
    run: BrowserRunView,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RerunRequest {
    idempotency_key: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RerunResponse {
    api_version: &'static str,
    #[schema(value_type = String, format = "uuid")]
    task_uid: Uuid,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RerunPendingResponse {
    api_version: &'static str,
    state: RerunPendingState,
    retry_after_ms: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RerunPendingState {
    Pending,
}

#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "kind"
)]
#[schema(rename_all = "camelCase")]
pub(crate) enum BrowserRunTimelineEvent {
    Phase {
        phase: TaskPhase,
        at: String,
    },
    FinalizationRequested {
        at: String,
    },
    Finalized {
        at: String,
    },
    Admitted {
        #[schema(rename = "envelopeRevision")]
        envelope_revision: Option<i64>,
        #[schema(rename = "envelopeDigest")]
        envelope_digest: Option<String>,
        at: String,
    },
    RuntimeBound {
        #[schema(rename = "runtimeUid")]
        runtime_uid: String,
        ownership: RuntimeOwnership,
        at: String,
    },
    ExecutionStarted {
        at: String,
    },
    ExecutionEnded {
        #[schema(rename = "exitCategory")]
        exit_category: BrowserRunExitCategory,
        at: String,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AdmittedStageDetails {
    envelope_revision: Option<i64>,
    envelope_digest: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeBoundStageDetails {
    runtime_uid: String,
    ownership: RuntimeOwnership,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExecutionEndedStageDetails {
    exit_category: BrowserRunExitCategory,
}

#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserRunTimelineResponse {
    api_version: &'static str,
    #[schema(value_type = String, format = "uuid")]
    task_uid: Uuid,
    events: Vec<BrowserRunTimelineEvent>,
}

#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserRunEventSnapshot {
    api_version: &'static str,
    event_id: u64,
    run: BrowserRunView,
    timeline: BrowserRunTimelineResponse,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct BrowserExecutionLogQuery {
    after: Option<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserExecutionLogResponse {
    stream: String,
    content: String,
    truncated: bool,
    size_bytes: usize,
    complete: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserRunOutputFile {
    path: String,
    size_bytes: usize,
    download_url: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserRunOutputsResponse {
    #[schema(value_type = String, format = "uuid")]
    task_uid: Uuid,
    files: Vec<BrowserRunOutputFile>,
}

const MAX_BROWSER_LOG_CHUNK_BYTES: usize = 64 * 1024;

fn my_runs_router<L>(
    ledger: L,
    github_rerunner: Arc<dyn BrowserGithubRerunner>,
    browser_task_rerunner: Arc<dyn BrowserTaskRerunner>,
) -> Router
where
    L: AgentRunLedger,
{
    Router::new()
        .route("/app/api/v1/runs", get(my_runs::<L>))
        .route("/app/api/v1/runs/{task_uid}", get(my_run::<L>))
        .route(
            "/app/api/v1/runs/{task_uid}/events",
            get(my_run_events::<L>),
        )
        .route(
            "/app/api/v1/runs/{task_uid}/package",
            get(my_run_package::<L>),
        )
        .route(
            "/app/api/v1/runs/{task_uid}/cancel",
            post(cancel_my_run::<L>),
        )
        .route("/app/api/v1/runs/{task_uid}/rerun", post(rerun_my_run::<L>))
        .route(
            "/app/api/v1/runs/{task_uid}/timeline",
            get(my_run_timeline::<L>),
        )
        .route(
            "/app/api/v1/runs/{task_uid}/logs/{stream}",
            get(my_run_execution_log::<L>),
        )
        .route(
            "/app/api/v1/runs/{task_uid}/outputs",
            get(my_run_outputs::<L>),
        )
        .route(
            "/app/api/v1/runs/{task_uid}/outputs/{*path}",
            get(download_my_run_output::<L>),
        )
        .with_state(BrowserRunsState {
            ledger,
            github_rerunner,
            browser_task_rerunner,
        })
}

#[utoipa::path(
    get,
    path = "/app/api/v1/runs/{task_uid}/package",
    params(("task_uid" = String, Path, format = "uuid")),
    responses(
        (status = 200, body = BrowserRunPackageContentResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 404, description = "Exact successful inline package is unavailable"),
        (status = 503, description = "Run package is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn my_run_package<L>(
    session: Option<Extension<BrowserSessionContext>>,
    State(state): State<BrowserRunsState<L>>,
    Path(task_uid): Path<Uuid>,
) -> Response
where
    L: AgentRunLedger,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    match scoped_run(
        &state.ledger,
        task_uid,
        Some(session.principal.canonical_user_id.as_str().to_owned()),
    )
    .await
    {
        Ok(Some(record))
            if record.phase == TaskPhase::Succeeded
                && record.task_origin == TaskOrigin::Browser =>
        {
            let Some(evidence) = record
                .browser_task_evidence
                .filter(|evidence| evidence.source == "inline")
            else {
                return StatusCode::NOT_FOUND.into_response();
            };
            let Some(files) = evidence.inline_files else {
                return StatusCode::NOT_FOUND.into_response();
            };
            Json(BrowserRunPackageContentResponse { task_uid, files }).into_response()
        }
        Ok(Some(_) | None) => StatusCode::NOT_FOUND.into_response(),
        Err(StoreError::InvalidRunQuery | StoreError::InvalidRunCursor) => {
            browser_runs_error(StatusCode::BAD_REQUEST)
        }
        Err(_) => browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
    }
}

#[utoipa::path(
    get,
    path = "/app/api/v1/runs/{task_uid}/outputs",
    params(("task_uid" = String, Path, format = "uuid")),
    responses(
        (status = 200, body = BrowserRunOutputsResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 404, description = "Completed run output is unavailable"),
        (status = 503, description = "Run output is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn my_run_outputs<L>(
    session: Option<Extension<BrowserSessionContext>>,
    State(state): State<BrowserRunsState<L>>,
    Path(task_uid): Path<Uuid>,
) -> Response
where
    L: AgentRunLedger,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let archive = match state
        .ledger
        .agent_run_output_archive(task_uid, session.principal.canonical_user_id.as_str())
        .await
    {
        Ok(Some(archive)) => archive,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let entries = match output_archive_entries(&archive) {
        Ok(entries) => entries,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    Json(BrowserRunOutputsResponse {
        task_uid,
        files: entries
            .into_iter()
            .map(|entry| BrowserRunOutputFile {
                download_url: format!(
                    "/app/api/v1/runs/{task_uid}/outputs/{}",
                    encode_output_path(&entry.path)
                ),
                path: entry.path,
                size_bytes: entry.size,
            })
            .collect(),
    })
    .into_response()
}

#[utoipa::path(
    get,
    path = "/app/api/v1/runs/{task_uid}/outputs/{path}",
    params(
        ("task_uid" = String, Path, format = "uuid"),
        ("path" = String, Path)
    ),
    responses(
        (status = 200, description = "Opaque run output file", content_type = "application/octet-stream"),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 404, description = "Run output file is unavailable"),
        (status = 503, description = "Run output is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn download_my_run_output<L>(
    session: Option<Extension<BrowserSessionContext>>,
    State(state): State<BrowserRunsState<L>>,
    Path((task_uid, path)): Path<(Uuid, String)>,
) -> Response
where
    L: AgentRunLedger,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let archive = match state
        .ledger
        .agent_run_output_archive(task_uid, session.principal.canonical_user_id.as_str())
        .await
    {
        Ok(Some(archive)) => archive,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let Ok(entries) = output_archive_entries(&archive) else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let Some(entry) = entries.into_iter().find(|entry| entry.path == path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(content_type) = HeaderValue::from_str("application/octet-stream") else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let filename = entry.path.rsplit('/').next().unwrap_or("output");
    let Ok(disposition) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
    else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, content_type);
    headers.insert(header::CONTENT_DISPOSITION, disposition);
    (
        headers,
        archive[entry.offset..entry.offset + entry.size].to_vec(),
    )
        .into_response()
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct OutputArchiveEntry {
    path: String,
    offset: usize,
    size: usize,
}

fn output_archive_entries(archive: &[u8]) -> Result<Vec<OutputArchiveEntry>, ()> {
    let mut offset = 0_usize;
    let mut entries = Vec::new();
    let mut seen = BTreeSet::new();
    while offset
        .checked_add(512)
        .filter(|end| *end <= archive.len())
        .is_some()
    {
        let header = &archive[offset..offset + 512];
        if header.iter().all(|byte| *byte == 0) {
            return Ok(entries);
        }
        let path = tar_path(header)?;
        let size = tar_octal(&header[124..136])?;
        let data_offset = offset.checked_add(512).ok_or(())?;
        let data_end = data_offset
            .checked_add(size)
            .filter(|end| *end <= archive.len())
            .ok_or(())?;
        let kind = header[156];
        if kind == 0 || kind == b'0' {
            if !matches!(
                path.as_str(),
                ".steward/diagnostics/stdout.log" | ".steward/diagnostics/stderr.log"
            ) {
                let relative = path.strip_prefix("out/").ok_or(())?;
                let relative =
                    steward_types::direct_package::RelativePath::parse(relative.to_owned())
                        .map_err(|_| ())?;
                if !seen.insert(relative.as_str().to_owned()) {
                    return Err(());
                }
                entries.push(OutputArchiveEntry {
                    path: relative.as_str().to_owned(),
                    offset: data_offset,
                    size,
                });
            }
        } else if !matches!(kind, b'5' | b'x' | b'g') {
            return Err(());
        }
        let padded = size.checked_add(511).ok_or(())? / 512 * 512;
        offset = data_offset
            .checked_add(padded)
            .filter(|next| *next >= data_end)
            .ok_or(())?;
    }
    Err(())
}

fn tar_path(header: &[u8]) -> Result<String, ()> {
    fn field(bytes: &[u8]) -> Result<&str, ()> {
        let end = bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(bytes.len());
        std::str::from_utf8(&bytes[..end]).map_err(|_| ())
    }
    let name = field(&header[..100])?;
    let prefix = field(&header[345..500])?;
    if name.is_empty() {
        return Err(());
    }
    Ok(if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}/{name}")
    })
}

fn tar_octal(bytes: &[u8]) -> Result<usize, ()> {
    let text = std::str::from_utf8(bytes).map_err(|_| ())?;
    let text = text.trim_matches(['\0', ' ']);
    if text.is_empty() {
        return Ok(0);
    }
    usize::from_str_radix(text, 8).map_err(|_| ())
}

fn encode_output_path(path: &str) -> String {
    path.split('/')
        .map(|segment| {
            let mut encoded = String::new();
            for byte in segment.bytes() {
                if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                    encoded.push(char::from(byte));
                } else {
                    encoded.push_str(&format!("%{byte:02X}"));
                }
            }
            encoded
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn all_runs_router<L>(ledger: L, github_rerunner: Arc<dyn BrowserGithubRerunner>) -> Router
where
    L: AgentRunLedger,
{
    Router::new()
        .route("/admin/api/v1/all-runs", get(all_runs::<L>))
        .route("/admin/api/v1/all-runs/{task_uid}", get(all_run::<L>))
        .route(
            "/admin/api/v1/all-runs/{task_uid}/timeline",
            get(all_run_timeline::<L>),
        )
        .route(
            "/admin/api/v1/all-runs/{task_uid}/logs/{stream}",
            get(all_run_execution_log::<L>),
        )
        .with_state(BrowserRunsState {
            ledger,
            github_rerunner,
            browser_task_rerunner: Arc::new(DisabledBrowserTaskRerunner),
        })
}

/// Mount browser-session-bound Runs APIs.
///
/// `GET /app/api/v1/runs` and its detail/timeline descendants are exact-identity scoped to the
/// browser principal. `GET /admin/api/v1/all-runs` and its descendants are browser-admin only
/// and may optionally filter an opaque canonical owner identifier. Neither response exposes
/// emails, provider credentials, commands, prompts or raw failure data.
pub fn protected_router<L>(ledger: L, browser_auth: BrowserAuthService) -> Router
where
    L: AgentRunLedger,
{
    let github_rerunner: Arc<dyn BrowserGithubRerunner> = Arc::new(DisabledGithubRerunner);
    let browser_task_rerunner: Arc<dyn BrowserTaskRerunner> = Arc::new(DisabledBrowserTaskRerunner);
    protect_browser_routes(
        my_runs_router(
            ledger.clone(),
            github_rerunner.clone(),
            browser_task_rerunner,
        ),
        browser_auth.clone(),
    )
    .merge(protect_browser_admin_routes(
        all_runs_router(ledger, github_rerunner),
        browser_auth,
    ))
}

pub fn protected_router_with_github_reruns<L, P>(
    ledger: L,
    github_rerunner: P,
    browser_auth: BrowserAuthService,
) -> Router
where
    L: AgentRunLedger,
    P: GithubWorkflowRerunBroker<BrowserSessionBinding>,
{
    let github_rerunner: Arc<dyn BrowserGithubRerunner> = Arc::new(github_rerunner);
    let browser_task_rerunner: Arc<dyn BrowserTaskRerunner> = Arc::new(DisabledBrowserTaskRerunner);
    protect_browser_routes(
        my_runs_router(
            ledger.clone(),
            github_rerunner.clone(),
            browser_task_rerunner,
        ),
        browser_auth.clone(),
    )
    .merge(protect_browser_admin_routes(
        all_runs_router(ledger, github_rerunner),
        browser_auth,
    ))
}

pub fn protected_router_with_rerunners<L, P>(
    ledger: L,
    github_rerunner: P,
    browser_task_rerunner: Arc<dyn BrowserTaskRerunner>,
    browser_auth: BrowserAuthService,
) -> Router
where
    L: AgentRunLedger,
    P: GithubWorkflowRerunBroker<BrowserSessionBinding>,
{
    let github_rerunner: Arc<dyn BrowserGithubRerunner> = Arc::new(github_rerunner);
    protect_browser_routes(
        my_runs_router(
            ledger.clone(),
            github_rerunner.clone(),
            browser_task_rerunner,
        ),
        browser_auth.clone(),
    )
    .merge(protect_browser_admin_routes(
        all_runs_router(ledger, github_rerunner),
        browser_auth,
    ))
}

pub fn protected_router_with_task_reruns<L>(
    ledger: L,
    browser_task_rerunner: Arc<dyn BrowserTaskRerunner>,
    browser_auth: BrowserAuthService,
) -> Router
where
    L: AgentRunLedger,
{
    let github_rerunner: Arc<dyn BrowserGithubRerunner> = Arc::new(DisabledGithubRerunner);
    protect_browser_routes(
        my_runs_router(
            ledger.clone(),
            github_rerunner.clone(),
            browser_task_rerunner,
        ),
        browser_auth.clone(),
    )
    .merge(protect_browser_admin_routes(
        all_runs_router(ledger, github_rerunner),
        browser_auth,
    ))
}

#[utoipa::path(
    get,
    path = "/app/api/v1/runs",
    params(
        ("limit" = Option<u16>, Query),
        ("cursor" = Option<String>, Query),
        ("phase" = Option<TaskPhase>, Query),
        ("workflow" = Option<String>, Query),
        ("runtimeUid" = Option<String>, Query),
        ("envelopeInstanceId" = Option<String>, Query)
    ),
    responses(
        (status = 200, body = MyRunsResponse),
        (status = 400, description = "Run query is invalid"),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 503, description = "Run history is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn my_runs<L>(
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
        runtime_uid: query.runtime_uid,
        user_envelope_instance_id: query.envelope_instance_id,
        task_uid: None,
    };
    let facets = match state.ledger.agent_run_phase_facets(&query).await {
        Ok(phase) => BrowserRunFacets {
            phase: BrowserRunPhaseFacets::from_counts(&phase),
        },
        Err(_) => return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
    };
    match state.ledger.agent_runs(&query).await {
        Ok(page) => Json(MyRunsResponse {
            api_version: BROWSER_AGENT_RUNS_API_VERSION,
            runs: page.records.into_iter().map(browser_run_view).collect(),
            next_cursor: page.next_cursor,
            facets,
        })
        .into_response(),
        Err(StoreError::InvalidRunQuery | StoreError::InvalidRunCursor) => {
            browser_runs_error(StatusCode::BAD_REQUEST)
        }
        Err(_) => browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
    }
}

#[utoipa::path(
    get,
    path = "/admin/api/v1/all-runs",
    params(
        ("limit" = Option<u16>, Query),
        ("cursor" = Option<String>, Query),
        ("phase" = Option<TaskPhase>, Query),
        ("workflow" = Option<String>, Query),
        ("ownerUserId" = Option<CanonicalUserId>, Query),
        ("runtimeUid" = Option<String>, Query)
    ),
    responses(
        (status = 200, body = AllRunsResponse),
        (status = 400, description = "Run query is invalid"),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role is required"),
        (status = 503, description = "Run history is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn all_runs<L>(
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
        runtime_uid: query.runtime_uid,
        user_envelope_instance_id: None,
        task_uid: None,
    };
    let facets = match state.ledger.agent_run_phase_facets(&query).await {
        Ok(phase) => BrowserRunFacets {
            phase: BrowserRunPhaseFacets::from_counts(&phase),
        },
        Err(_) => return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
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
                    owner_display_email: record.owner_display_email.clone(),
                    run: browser_run_view(record),
                })
                .collect(),
            next_cursor,
            facets,
        })
        .into_response(),
        Err(StoreError::InvalidRunQuery | StoreError::InvalidRunCursor) => {
            browser_runs_error(StatusCode::BAD_REQUEST)
        }
        Err(_) => browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
    }
}

#[utoipa::path(
    get,
    path = "/app/api/v1/runs/{task_uid}",
    params(("task_uid" = String, Path)),
    responses(
        (status = 200, body = BrowserRunResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 404, description = "Run was not found in the user's scope"),
        (status = 503, description = "Run history is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn my_run<L>(
    session: Option<Extension<BrowserSessionContext>>,
    State(state): State<BrowserRunsState<L>>,
    Path(task_uid): Path<Uuid>,
) -> Response
where
    L: AgentRunLedger,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    single_run_response(
        &state.ledger,
        task_uid,
        Some(session.principal.canonical_user_id.as_str().to_owned()),
    )
    .await
}

#[utoipa::path(
    get,
    operation_id = "myRunEvents",
    path = "/app/api/v1/runs/{task_uid}/events",
    params(
        ("task_uid" = String, Path, format = "uuid"),
        ("Last-Event-ID" = Option<u64>, Header)
    ),
    responses(
        (status = 200, body = String, content_type = "text/event-stream"),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 404, description = "Run was not found in the user's scope"),
        (status = 503, description = "Run history is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn my_run_events<L>(
    session: Option<Extension<BrowserSessionContext>>,
    State(state): State<BrowserRunsState<L>>,
    Path(task_uid): Path<Uuid>,
    headers: HeaderMap,
) -> Response
where
    L: AgentRunLedger,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let owner_user_id = Some(session.principal.canonical_user_id.as_str().to_owned());
    let record = match scoped_run(&state.ledger, task_uid, owner_user_id).await {
        Ok(Some(record)) => record,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
    };
    let timeline = match state.ledger.agent_run_timeline(task_uid).await {
        Ok(Some(events)) => match events
            .into_iter()
            .rev()
            .map(browser_timeline_event)
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(events) => BrowserRunTimelineResponse {
                api_version: BROWSER_AGENT_RUNS_API_VERSION,
                task_uid,
                events,
            },
            Err(_) => return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
        },
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
    };
    let last_event_id = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
        .min(u128::from(u64::MAX)) as u64;
    let event_id = now.max(last_event_id.saturating_add(1));
    let snapshot = BrowserRunEventSnapshot {
        api_version: BROWSER_AGENT_RUNS_API_VERSION,
        event_id,
        run: browser_run_view(record),
        timeline,
    };
    let data = match serde_json::to_string(&snapshot) {
        Ok(data) => data,
        Err(_) => return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
    };
    let body =
        format!("retry: 2000\nid: {event_id}\nevent: snapshot\ndata: {data}\n\n: heartbeat\n\n");
    let mut response = body.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-transform"),
    );
    response.headers_mut().insert(
        HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    response
}

#[utoipa::path(
    post,
    operation_id = "cancelMyRun",
    path = "/app/api/v1/runs/{task_uid}/cancel",
    params(
        ("task_uid" = String, Path, format = "uuid"),
        ("X-Steward-CSRF" = String, Header)
    ),
    responses(
        (status = 200, body = BrowserRunResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Origin, fetch metadata, or CSRF proof is invalid"),
        (status = 404, description = "Run was not found in the user's scope"),
        (status = 409, description = "Run has already reached a terminal phase"),
        (status = 503, description = "Run cancellation is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn cancel_my_run<L>(
    session: Option<Extension<BrowserSessionContext>>,
    _proof: Extension<BrowserMutationProof>,
    State(state): State<BrowserRunsState<L>>,
    Path(task_uid): Path<Uuid>,
) -> Response
where
    L: AgentRunLedger,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    match state
        .ledger
        .cancel_agent_run(task_uid, session.principal.canonical_user_id.as_str())
        .await
    {
        Ok(Some(record)) => Json(BrowserRunResponse {
            api_version: BROWSER_AGENT_RUNS_API_VERSION,
            run: browser_run_view(record),
        })
        .into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(StoreError::InvalidTaskTransition) => StatusCode::CONFLICT.into_response(),
        Err(_) => browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
    }
}

#[utoipa::path(
    post,
    operation_id = "rerunMyRun",
    path = "/app/api/v1/runs/{task_uid}/rerun",
    params(("task_uid" = String, Path, format = "uuid"), ("X-Steward-CSRF" = String, Header)),
    request_body = RerunRequest,
    responses(
        (status = 201, body = RerunResponse),
        (status = 200, body = RerunResponse, description = "Idempotent replay"),
        (status = 202, body = RerunPendingResponse, description = "GitHub accepted the rerun and Steward is awaiting the correlated Task"),
        (status = 400, description = "Idempotency key is invalid"),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Origin, fetch metadata, or CSRF proof is invalid"),
        (status = 404, description = "Run was not found in the user's scope"),
        (status = 409, description = "The original envelope is no longer active or GitHub connection authorization is pending"),
        (status = 422, description = "The persisted browser package no longer fits the current Envelope"),
        (status = 503, body = RerunErrorResponse, description = "Run submission is unavailable; connections.orchestration_not_active identifies staged task orchestration")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn rerun_my_run<L>(
    session: Option<Extension<BrowserSessionContext>>,
    _proof: Extension<BrowserMutationProof>,
    State(state): State<BrowserRunsState<L>>,
    Path(source_task_uid): Path<Uuid>,
    Json(request): Json<RerunRequest>,
) -> Response
where
    L: AgentRunLedger,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let key = request.idempotency_key.trim();
    if key.is_empty() || key.len() > 255 || key.bytes().any(|byte| byte.is_ascii_control()) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let owner_user_id = session.principal.canonical_user_id.as_str();
    let source = match state
        .ledger
        .rerun_source(source_task_uid, owner_user_id)
        .await
    {
        Ok(Some(source)) => source,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
    };
    let idempotency_key = format!("browser-rerun:{source_task_uid}:{key}");
    if let Some(evidence) = source.direct_task_evidence.as_ref() {
        let provenance = &evidence.source_provenance;
        let repository = provenance.repository.name.as_str();
        let Some((repository_owner, repository_name)) = repository.split_once('/') else {
            return StatusCode::CONFLICT.into_response();
        };
        if repository_owner.is_empty()
            || repository_name.is_empty()
            || repository_name.contains('/')
        {
            return StatusCode::CONFLICT.into_response();
        }
        let run_id = provenance.run.id.as_str();
        let Ok(numeric_run_id) = run_id.parse::<u64>() else {
            return StatusCode::CONFLICT.into_response();
        };
        let correlated = state
            .ledger
            .github_rerun_task(owner_user_id, repository, run_id, provenance.run.attempt)
            .await;
        match correlated {
            Ok(Some(task)) => {
                return (
                    StatusCode::OK,
                    Json(RerunResponse {
                        api_version: BROWSER_AGENT_RUNS_API_VERSION,
                        task_uid: task.task_uid,
                    }),
                )
                    .into_response();
            }
            Ok(None) => {}
            Err(_) => return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
        }
        let dispatch = GithubWorkflowRerunRequest {
            owner: repository_owner.to_owned(),
            repository: repository_name.to_owned(),
            run_id: numeric_run_id,
            idempotency_key,
        };
        if let Err(error) = state.github_rerunner.rerun(&session, &dispatch).await {
            return match error {
                ConnectionBrokerError::OAuthFlowPending => StatusCode::CONFLICT.into_response(),
                ConnectionBrokerError::OrchestrationNotActive => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(RerunErrorResponse {
                        error: "connections.orchestration_not_active",
                    }),
                )
                    .into_response(),
                ConnectionBrokerError::RuntimeAuthenticationFailed
                | ConnectionBrokerError::ProxyPolicyDenied
                | ConnectionBrokerError::ProviderAuthorizationFailed
                | ConnectionBrokerError::TokenGrantFailed
                | ConnectionBrokerError::ProviderResponseInvalid
                | ConnectionBrokerError::GatewayTransportFailed
                | ConnectionBrokerError::GatewayStatusInvalid
                | ConnectionBrokerError::GatewayBodyUnavailable
                | ConnectionBrokerError::GatewayUnavailable
                | ConnectionBrokerError::RuntimeCreateFailed
                | ConnectionBrokerError::RuntimeStartFailed
                | ConnectionBrokerError::DeadlineExceeded
                | ConnectionBrokerError::GatewayHttp { .. }
                | ConnectionBrokerError::Unavailable => {
                    browser_runs_error(StatusCode::SERVICE_UNAVAILABLE)
                }
            };
        }
        return match state
            .ledger
            .github_rerun_task(owner_user_id, repository, run_id, provenance.run.attempt)
            .await
        {
            Ok(Some(task)) => (
                StatusCode::CREATED,
                Json(RerunResponse {
                    api_version: BROWSER_AGENT_RUNS_API_VERSION,
                    task_uid: task.task_uid,
                }),
            )
                .into_response(),
            Ok(None) => (
                StatusCode::ACCEPTED,
                Json(RerunPendingResponse {
                    api_version: BROWSER_AGENT_RUNS_API_VERSION,
                    state: RerunPendingState::Pending,
                    retry_after_ms: 1_000,
                }),
            )
                .into_response(),
            Err(_) => browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
        };
    }
    match state
        .browser_task_rerunner
        .rerun(&session, &source, &idempotency_key)
        .await
    {
        Ok(task_uid) => (
            StatusCode::CREATED,
            Json(RerunResponse {
                api_version: BROWSER_AGENT_RUNS_API_VERSION,
                task_uid,
            }),
        )
            .into_response(),
        Err(BrowserTaskRerunError::Unsupported) => StatusCode::CONFLICT.into_response(),
        Err(BrowserTaskRerunError::EnvelopeUnavailable) => StatusCode::CONFLICT.into_response(),
        Err(BrowserTaskRerunError::Rejected) => StatusCode::UNPROCESSABLE_ENTITY.into_response(),
        Err(BrowserTaskRerunError::Unavailable) => {
            browser_runs_error(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

#[utoipa::path(
    get,
    path = "/admin/api/v1/all-runs/{task_uid}",
    params(("task_uid" = String, Path)),
    responses(
        (status = 200, body = BrowserRunResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role is required"),
        (status = 404, description = "Run was not found"),
        (status = 503, description = "Run history is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn all_run<L>(
    authority: Option<Extension<BrowserAdminAuthority>>,
    State(state): State<BrowserRunsState<L>>,
    Path(task_uid): Path<Uuid>,
) -> Response
where
    L: AgentRunLedger,
{
    if authority.is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    single_run_response(&state.ledger, task_uid, None).await
}

#[utoipa::path(
    get,
    path = "/app/api/v1/runs/{task_uid}/logs/{stream}",
    params(
        ("task_uid" = String, Path, format = "uuid"),
        ("stream" = String, Path, description = "Exact execution stream: stdout or stderr"),
        ("after" = usize, Query, description = "Required by generated clients for the typed JSON response; legacy callers may omit it for text/plain")
    ),
    responses(
        (status = 200, content(
            (BrowserExecutionLogResponse = "application/json"),
            (String = "text/plain")
        )),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 404, description = "Execution log was not found in the user's scope"),
        (status = 503, description = "Run history is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn my_run_execution_log<L>(
    session: Option<Extension<BrowserSessionContext>>,
    State(state): State<BrowserRunsState<L>>,
    Path((task_uid, stream)): Path<(Uuid, String)>,
    Query(query): Query<BrowserExecutionLogQuery>,
) -> Response
where
    L: AgentRunLedger,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    execution_log_response(
        &state.ledger,
        task_uid,
        Some(session.principal.canonical_user_id.as_str()),
        &stream,
        query.after.unwrap_or(0),
        query.after.is_some(),
    )
    .await
}

#[utoipa::path(
    get,
    path = "/admin/api/v1/all-runs/{task_uid}/logs/{stream}",
    params(
        ("task_uid" = String, Path, format = "uuid"),
        ("stream" = String, Path, description = "Exact execution stream: stdout or stderr"),
        ("after" = usize, Query, description = "Required by generated clients for the typed JSON response; legacy callers may omit it for text/plain")
    ),
    responses(
        (status = 200, content(
            (BrowserExecutionLogResponse = "application/json"),
            (String = "text/plain")
        )),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role is required"),
        (status = 404, description = "Execution log was not found"),
        (status = 503, description = "Run history is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn all_run_execution_log<L>(
    authority: Option<Extension<BrowserAdminAuthority>>,
    State(state): State<BrowserRunsState<L>>,
    Path((task_uid, stream)): Path<(Uuid, String)>,
    Query(query): Query<BrowserExecutionLogQuery>,
) -> Response
where
    L: AgentRunLedger,
{
    if authority.is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    execution_log_response(
        &state.ledger,
        task_uid,
        None,
        &stream,
        query.after.unwrap_or(0),
        query.after.is_some(),
    )
    .await
}

async fn execution_log_response<L>(
    ledger: &L,
    task_uid: Uuid,
    owner_user_id: Option<&str>,
    stream: &str,
    after: usize,
    typed: bool,
) -> Response
where
    L: AgentRunLedger,
{
    let stream_kind = match stream {
        "stdout" => AgentRunLogStream::Stdout,
        "stderr" => AgentRunLogStream::Stderr,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    match ledger
        .agent_run_execution_log(task_uid, owner_user_id, stream_kind)
        .await
    {
        Ok(Some(log)) => execution_log_body(stream, log, after, typed),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
    }
}

fn execution_log_body(
    stream: &str,
    log: AgentRunExecutionLog,
    after: usize,
    typed: bool,
) -> Response {
    if !typed {
        let mut response = log.content.into_response();
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        );
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response.headers_mut().insert(
            HeaderName::from_static("x-content-type-options"),
            HeaderValue::from_static("nosniff"),
        );
        return response;
    }
    let size_bytes = log.content.len();
    let start = after.min(size_bytes);
    let end = start
        .saturating_add(MAX_BROWSER_LOG_CHUNK_BYTES)
        .min(size_bytes);
    let mut response = Json(BrowserExecutionLogResponse {
        stream: stream.to_owned(),
        content: String::from_utf8_lossy(&log.content[start..end]).into_owned(),
        truncated: end < size_bytes,
        size_bytes,
        complete: log.complete,
    })
    .into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    response
}

#[utoipa::path(
    get,
    path = "/app/api/v1/runs/{task_uid}/timeline",
    params(("task_uid" = String, Path)),
    responses(
        (status = 200, body = BrowserRunTimelineResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 404, description = "Run was not found in the user's scope"),
        (status = 503, description = "Run history is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn my_run_timeline<L>(
    session: Option<Extension<BrowserSessionContext>>,
    State(state): State<BrowserRunsState<L>>,
    Path(task_uid): Path<Uuid>,
) -> Response
where
    L: AgentRunLedger,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    scoped_timeline_response(
        &state.ledger,
        task_uid,
        Some(session.principal.canonical_user_id.as_str().to_owned()),
    )
    .await
}

#[utoipa::path(
    get,
    path = "/admin/api/v1/all-runs/{task_uid}/timeline",
    params(("task_uid" = String, Path)),
    responses(
        (status = 200, body = BrowserRunTimelineResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role is required"),
        (status = 404, description = "Run was not found"),
        (status = 503, description = "Run history is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn all_run_timeline<L>(
    authority: Option<Extension<BrowserAdminAuthority>>,
    State(state): State<BrowserRunsState<L>>,
    Path(task_uid): Path<Uuid>,
) -> Response
where
    L: AgentRunLedger,
{
    if authority.is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    scoped_timeline_response(&state.ledger, task_uid, None).await
}

async fn single_run_response<L>(
    ledger: &L,
    task_uid: Uuid,
    owner_user_id: Option<String>,
) -> Response
where
    L: AgentRunLedger,
{
    match scoped_run(ledger, task_uid, owner_user_id).await {
        Ok(Some(record)) => Json(BrowserRunResponse {
            api_version: BROWSER_AGENT_RUNS_API_VERSION,
            run: browser_run_view(record),
        })
        .into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(StoreError::InvalidRunQuery | StoreError::InvalidRunCursor) => {
            browser_runs_error(StatusCode::BAD_REQUEST)
        }
        Err(_) => browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
    }
}

async fn scoped_timeline_response<L>(
    ledger: &L,
    task_uid: Uuid,
    owner_user_id: Option<String>,
) -> Response
where
    L: AgentRunLedger,
{
    match scoped_run(ledger, task_uid, owner_user_id).await {
        Ok(Some(_)) => match ledger.agent_run_timeline(task_uid).await {
            Ok(Some(events)) => Json(BrowserRunTimelineResponse {
                api_version: BROWSER_AGENT_RUNS_API_VERSION,
                task_uid,
                events: match events
                    .into_iter()
                    .rev()
                    .map(browser_timeline_event)
                    .collect::<Result<Vec<_>, _>>()
                {
                    Ok(events) => events,
                    Err(_) => return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
                },
            })
            .into_response(),
            Ok(None) => StatusCode::NOT_FOUND.into_response(),
            Err(_) => browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
        },
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(StoreError::InvalidRunQuery | StoreError::InvalidRunCursor) => {
            browser_runs_error(StatusCode::BAD_REQUEST)
        }
        Err(_) => browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
    }
}

async fn scoped_run<L>(
    ledger: &L,
    task_uid: Uuid,
    owner_user_id: Option<String>,
) -> Result<Option<AgentRunRecord>, StoreError>
where
    L: AgentRunLedger,
{
    let page = ledger
        .agent_runs(&AgentRunQuery {
            limit: 1,
            cursor: None,
            phase: None,
            workflow: None,
            owner_user_id,
            runtime_uid: None,
            user_envelope_instance_id: None,
            task_uid: Some(task_uid),
        })
        .await?;
    Ok(page.records.into_iter().next())
}

fn browser_run_view(record: AgentRunRecord) -> BrowserRunView {
    let trigger = record
        .direct_task_evidence
        .as_ref()
        .map(|evidence| browser_run_trigger(&evidence.source_provenance));
    let execution_log = record
        .direct_task_evidence
        .as_ref()
        .map(|evidence| evidence.diagnostics.execution_log)
        .or_else(|| {
            record
                .browser_task_evidence
                .as_ref()
                .map(|evidence| evidence.diagnostics.execution_log)
        })
        .unwrap_or(ExecutionLogMode::Off);
    let stages = browser_run_stages(
        record.phase,
        record.runtime_uid.is_some(),
        record.finalize_requested,
        record.finalized,
        execution_log == ExecutionLogMode::Full,
    );
    let rerun_supported = record.direct_task_evidence.is_some()
        || (record.task_origin == TaskOrigin::Browser && record.browser_task_evidence.is_some());
    let package = record
        .browser_task_evidence
        .as_ref()
        .map(|evidence| BrowserRunPackageView {
            source: evidence.source.clone(),
            revision: evidence.revision.clone(),
            path: evidence.path.as_str().to_owned(),
            content_digest: Some(evidence.closure_digest.as_str().to_owned()),
        })
        .or_else(|| {
            record
                .direct_task_evidence
                .as_ref()
                .map(|evidence| BrowserRunPackageView {
                    source: evidence.package.repository.as_str().to_owned(),
                    revision: evidence.package.commit.as_str().to_owned(),
                    path: evidence.package.path.as_str().to_owned(),
                    content_digest: Some(evidence.closure_digest.as_str().to_owned()),
                })
        })
        .or_else(|| {
            Some(BrowserRunPackageView {
                source: format!("steward:registry/{}", record.workflow_name.as_deref()?),
                revision: format!("steward:version:{}", record.workflow_version?),
                path: "task-definition.json".to_owned(),
                content_digest: record.workflow_digest.clone(),
            })
        });
    BrowserRunView {
        task_uid: record.task_uid,
        origin: record.task_origin,
        package,
        workflow: record.workflow,
        workflow_name: record.workflow_name,
        workflow_version: record.workflow_version,
        workflow_digest: record.workflow_digest,
        user_envelope_instance_id: record.user_envelope_instance_id,
        user_envelope_revision: record.user_envelope_revision,
        user_envelope_digest: record.user_envelope_digest,
        coding_agent_runtime: record.coding_agent_runtime,
        runtime_uid: record.runtime_uid,
        runtime_ownership: record.runtime_ownership,
        phase: record.phase,
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
        trigger,
        execution_log,
        rerun_supported,
        rerun_unavailable_reason: (!rerun_supported)
            .then_some("This run does not have a replayable package source."),
        stages,
    }
}

fn browser_run_trigger(
    provenance: &steward_types::direct_package::SourceProvenance,
) -> BrowserRunTrigger {
    let repository = provenance.repository.name.as_str().to_owned();
    let run_id = provenance.run.id.as_str().to_owned();
    BrowserRunTrigger {
        provider: "github",
        run_url: format!("https://github.com/{repository}/actions/runs/{run_id}"),
        repository,
        event: provenance.event.as_str().to_owned(),
        actor: provenance.actor.as_str().to_owned(),
        git_ref: provenance.git_ref.as_str().to_owned(),
        sha: provenance
            .triggered_sha
            .as_str()
            .strip_prefix("git:sha1:")
            .unwrap_or(provenance.triggered_sha.as_str())
            .to_owned(),
        run_id,
        run_attempt: provenance.run.attempt,
        caller_workflow: provenance.caller_workflow.workflow_ref.as_str().to_owned(),
    }
}

fn browser_run_stages(
    phase: TaskPhase,
    runtime_bound: bool,
    finalization_requested: bool,
    finalized: bool,
    execution_log_captured: bool,
) -> Vec<BrowserRunStage> {
    let terminal = matches!(
        phase,
        TaskPhase::Succeeded | TaskPhase::Failed | TaskPhase::Cancelled
    );
    let provision_state = if runtime_bound {
        BrowserRunStageState::Succeeded
    } else if phase == TaskPhase::Cancelled {
        BrowserRunStageState::Cancelled
    } else if phase == TaskPhase::Failed {
        BrowserRunStageState::Failed
    } else if matches!(
        phase,
        TaskPhase::Submitted | TaskPhase::Parked | TaskPhase::Queued
    ) {
        BrowserRunStageState::Running
    } else {
        BrowserRunStageState::Pending
    };
    let execution_state = match phase {
        TaskPhase::Submitted | TaskPhase::Parked | TaskPhase::Queued => {
            BrowserRunStageState::Pending
        }
        TaskPhase::Running => BrowserRunStageState::Running,
        TaskPhase::Succeeded => BrowserRunStageState::Succeeded,
        TaskPhase::Failed => BrowserRunStageState::Failed,
        TaskPhase::Cancelled => BrowserRunStageState::Cancelled,
    };
    let finalize_state = if finalized {
        BrowserRunStageState::Succeeded
    } else if finalization_requested || terminal {
        BrowserRunStageState::Running
    } else {
        BrowserRunStageState::Pending
    };
    vec![
        BrowserRunStage {
            id: BrowserRunStageId::Admission,
            display_name: "Admission",
            state: BrowserRunStageState::Succeeded,
            steps: Vec::new(),
        },
        BrowserRunStage {
            id: BrowserRunStageId::ProvisionRuntime,
            display_name: "Provision runtime",
            state: provision_state,
            steps: Vec::new(),
        },
        BrowserRunStage {
            id: BrowserRunStageId::AgentExecution,
            display_name: "Agent execution",
            state: execution_state,
            steps: vec![BrowserRunStep {
                id: "execution",
                display_name: "Agent execution",
                state: execution_state,
                log_streams: if execution_log_captured {
                    vec!["stdout", "stderr"]
                } else {
                    Vec::new()
                },
            }],
        },
        BrowserRunStage {
            id: BrowserRunStageId::Finalize,
            display_name: "Finalize",
            state: finalize_state,
            steps: Vec::new(),
        },
    ]
}

fn browser_timeline_event(
    event: AgentRunTimelineEvent,
) -> Result<BrowserRunTimelineEvent, StoreError> {
    Ok(match event.kind {
        AgentRunTimelineKind::Phase(phase) => BrowserRunTimelineEvent::Phase {
            phase,
            at: event.at,
        },
        AgentRunTimelineKind::FinalizationRequested => {
            BrowserRunTimelineEvent::FinalizationRequested { at: event.at }
        }
        AgentRunTimelineKind::Finalized => BrowserRunTimelineEvent::Finalized { at: event.at },
        AgentRunTimelineKind::Stage {
            event_kind,
            details,
        } => match event_kind.as_str() {
            "admitted" => {
                let details = serde_json::from_value::<AdmittedStageDetails>(details)
                    .map_err(|_| StoreError::InvalidTaskTransition)?;
                BrowserRunTimelineEvent::Admitted {
                    envelope_revision: details.envelope_revision,
                    envelope_digest: details.envelope_digest,
                    at: event.at,
                }
            }
            "runtime_bound" => {
                let details = serde_json::from_value::<RuntimeBoundStageDetails>(details)
                    .map_err(|_| StoreError::InvalidTaskTransition)?;
                BrowserRunTimelineEvent::RuntimeBound {
                    runtime_uid: details.runtime_uid,
                    ownership: details.ownership,
                    at: event.at,
                }
            }
            "execution_started" => BrowserRunTimelineEvent::ExecutionStarted { at: event.at },
            "execution_ended" => {
                let details = serde_json::from_value::<ExecutionEndedStageDetails>(details)
                    .map_err(|_| StoreError::InvalidTaskTransition)?;
                BrowserRunTimelineEvent::ExecutionEnded {
                    exit_category: details.exit_category,
                    at: event.at,
                }
            }
            _ => return Err(StoreError::InvalidTaskTransition),
        },
    })
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
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, Mutex};

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use steward_store::{AgentRunSpend, AgentRunTimelineEvent, TaskRecord};
    use steward_types::direct_package::{BrowserTaskEvidence, ContentDigest, RelativePath};
    use steward_types::{
        AgentRuntimeSpec, AgentType, Budget, Duration, Email, ModelRef, Principal,
    };
    use tower::ServiceExt;

    use super::*;
    use crate::BoxFuture;
    use crate::browser_auth::{
        LocalFakeIdentity, browser_auth_router, local_fake_browser_auth_service,
    };

    type FakeExecutionLogs = Arc<Mutex<HashMap<(Uuid, AgentRunLogStream), Vec<u8>>>>;
    type GithubRerunQuery = (String, String, String, u32);

    #[derive(Clone, Default)]
    struct FakeLedger {
        records: Arc<Mutex<Vec<AgentRunRecord>>>,
        queries: Arc<Mutex<Vec<AgentRunQuery>>>,
        logs: FakeExecutionLogs,
        outputs: Arc<Mutex<HashMap<Uuid, Vec<u8>>>>,
        rerun_sources: Arc<Mutex<HashMap<Uuid, TaskRecord>>>,
        github_matches: Arc<Mutex<VecDeque<Option<TaskRecord>>>>,
        github_queries: Arc<Mutex<Vec<GithubRerunQuery>>>,
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
                let records =
                    self.records
                        .lock()
                        .map_err(|_| StoreError::InvalidRunQuery)?
                        .iter()
                        .filter(|record| {
                            query.owner_user_id.as_ref().is_none_or(|owner| {
                                record.owner_user_id.as_deref() == Some(owner.as_str())
                            }) && query.runtime_uid.as_ref().is_none_or(|runtime_uid| {
                                record.runtime_uid.as_deref() == Some(runtime_uid.as_str())
                            }) && query.user_envelope_instance_id.as_ref().is_none_or(
                                |instance_id| {
                                    record.user_envelope_instance_id.as_deref()
                                        == Some(instance_id.as_str())
                                },
                            ) && query
                                .task_uid
                                .is_none_or(|task_uid| record.task_uid == task_uid)
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

        fn agent_run_phase_facets<'a>(
            &'a self,
            query: &'a AgentRunQuery,
        ) -> BoxFuture<'a, Result<std::collections::BTreeMap<String, u64>, StoreError>> {
            Box::pin(async move {
                let records = self
                    .records
                    .lock()
                    .map_err(|_| StoreError::InvalidRunQuery)?;
                let mut facets = std::collections::BTreeMap::new();
                for record in
                    records.iter().filter(|record| {
                        query.owner_user_id.as_ref().is_none_or(|owner| {
                            record.owner_user_id.as_deref() == Some(owner.as_str())
                        }) && query
                            .workflow
                            .as_ref()
                            .is_none_or(|workflow| &record.workflow == workflow)
                            && query.runtime_uid.as_ref().is_none_or(|runtime_uid| {
                                record.runtime_uid.as_deref() == Some(runtime_uid.as_str())
                            })
                            && query
                                .user_envelope_instance_id
                                .as_ref()
                                .is_none_or(|instance_id| {
                                    record.user_envelope_instance_id.as_deref()
                                        == Some(instance_id.as_str())
                                })
                    })
                {
                    let phase = serde_json::to_value(record.phase)
                        .ok()
                        .and_then(|value| value.as_str().map(str::to_owned))
                        .ok_or(StoreError::InvalidRunQuery)?;
                    *facets.entry(phase).or_insert(0) += 1;
                }
                Ok(facets)
            })
        }

        fn cancel_agent_run<'a>(
            &'a self,
            task_uid: Uuid,
            owner_user_id: &'a str,
        ) -> BoxFuture<'a, Result<Option<AgentRunRecord>, StoreError>> {
            Box::pin(async move {
                let mut records = self
                    .records
                    .lock()
                    .map_err(|_| StoreError::InvalidRunQuery)?;
                let Some(record) = records.iter_mut().find(|record| {
                    record.task_uid == task_uid
                        && record.owner_user_id.as_deref() == Some(owner_user_id)
                }) else {
                    return Ok(None);
                };
                if matches!(
                    record.phase,
                    TaskPhase::Succeeded | TaskPhase::Failed | TaskPhase::Cancelled
                ) {
                    return Err(StoreError::InvalidTaskTransition);
                }
                record.finalize_requested = true;
                if matches!(
                    record.phase,
                    TaskPhase::Submitted | TaskPhase::Parked | TaskPhase::Queued
                ) {
                    record.phase = TaskPhase::Cancelled;
                }
                Ok(Some(record.clone()))
            })
        }

        fn rerun_source<'a>(
            &'a self,
            task_uid: Uuid,
            owner_user_id: &'a str,
        ) -> BoxFuture<'a, Result<Option<TaskRecord>, StoreError>> {
            Box::pin(async move {
                Ok(self
                    .rerun_sources
                    .lock()
                    .map_err(|_| StoreError::InvalidRunQuery)?
                    .get(&task_uid)
                    .filter(|record| record.owner_user_id.as_deref() == Some(owner_user_id))
                    .cloned())
            })
        }

        fn github_rerun_task<'a>(
            &'a self,
            owner_user_id: &'a str,
            repository: &'a str,
            run_id: &'a str,
            after_attempt: u32,
        ) -> BoxFuture<'a, Result<Option<TaskRecord>, StoreError>> {
            Box::pin(async move {
                self.github_queries
                    .lock()
                    .map_err(|_| StoreError::InvalidRunQuery)?
                    .push((
                        owner_user_id.to_owned(),
                        repository.to_owned(),
                        run_id.to_owned(),
                        after_attempt,
                    ));
                Ok(self
                    .github_matches
                    .lock()
                    .map_err(|_| StoreError::InvalidRunQuery)?
                    .pop_front()
                    .flatten())
            })
        }

        fn agent_run_timeline<'a>(
            &'a self,
            task_uid: Uuid,
        ) -> BoxFuture<'a, Result<Option<Vec<AgentRunTimelineEvent>>, StoreError>> {
            Box::pin(async move {
                let known = self
                    .records
                    .lock()
                    .map_err(|_| StoreError::InvalidRunQuery)?
                    .iter()
                    .any(|record| record.task_uid == task_uid);
                Ok(known.then(|| {
                    vec![
                        AgentRunTimelineEvent {
                            kind: AgentRunTimelineKind::Stage {
                                event_kind: "admitted".to_owned(),
                                details: serde_json::json!({
                                    "envelopeRevision": 4,
                                    "envelopeDigest": format!("sha256:{}", "b".repeat(64)),
                                }),
                            },
                            provenance: steward_store::AgentRunTimelineProvenance::Recorded,
                            at: "2026-08-16T23:59:00.000000Z".to_owned(),
                        },
                        AgentRunTimelineEvent {
                            kind: AgentRunTimelineKind::Phase(TaskPhase::Running),
                            provenance: steward_store::AgentRunTimelineProvenance::Recorded,
                            at: "2026-08-17T00:00:00.000000Z".to_owned(),
                        },
                        AgentRunTimelineEvent {
                            kind: AgentRunTimelineKind::Finalized,
                            provenance: steward_store::AgentRunTimelineProvenance::Recorded,
                            at: "2026-08-17T00:01:00.000000Z".to_owned(),
                        },
                    ]
                }))
            })
        }

        fn agent_run_execution_log<'a>(
            &'a self,
            task_uid: Uuid,
            owner_user_id: Option<&'a str>,
            stream: AgentRunLogStream,
        ) -> BoxFuture<'a, Result<Option<AgentRunExecutionLog>, StoreError>> {
            Box::pin(async move {
                let visible = self
                    .records
                    .lock()
                    .map_err(|_| StoreError::InvalidRunQuery)?
                    .iter()
                    .any(|record| {
                        record.task_uid == task_uid
                            && owner_user_id
                                .is_none_or(|owner| record.owner_user_id.as_deref() == Some(owner))
                    });
                if !visible {
                    return Ok(None);
                }
                Ok(self
                    .logs
                    .lock()
                    .map_err(|_| StoreError::InvalidRunQuery)?
                    .get(&(task_uid, stream))
                    .cloned()
                    .map(|content| AgentRunExecutionLog {
                        content,
                        complete: true,
                    }))
            })
        }

        fn agent_run_output_archive<'a>(
            &'a self,
            task_uid: Uuid,
            owner_user_id: &'a str,
        ) -> BoxFuture<'a, Result<Option<Vec<u8>>, StoreError>> {
            Box::pin(async move {
                let visible = self
                    .records
                    .lock()
                    .map_err(|_| StoreError::InvalidRunQuery)?
                    .iter()
                    .any(|record| {
                        record.task_uid == task_uid
                            && record.phase == TaskPhase::Succeeded
                            && record.owner_user_id.as_deref() == Some(owner_user_id)
                    });
                if !visible {
                    return Ok(None);
                }
                Ok(self
                    .outputs
                    .lock()
                    .map_err(|_| StoreError::InvalidRunQuery)?
                    .get(&task_uid)
                    .cloned())
            })
        }
    }

    fn run(task_uid: Uuid, owner_user_id: &str) -> AgentRunRecord {
        AgentRunRecord {
            task_uid,
            submitter_service: "steward-run".to_owned(),
            acting_user: Some("alice@example.com".to_owned()),
            owner: "alice@example.com".to_owned(),
            owner_user_id: Some(owner_user_id.to_owned()),
            owner_display_email: Some("alice@example.com".to_owned()),
            workflow: "repository-review@1".to_owned(),
            workflow_name: Some("repository-review".to_owned()),
            workflow_version: Some(1),
            workflow_digest: Some(format!("sha256:{}", "a".repeat(64))),
            user_envelope_instance_id: Some("envelope-instance-1".to_owned()),
            user_envelope_revision: Some(4),
            user_envelope_digest: Some(format!("sha256:{}", "b".repeat(64))),
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
                    single_run_limit: None,
                    currency: "USD".to_owned(),
                },
                ttl: Duration("24h".to_owned()),
                runner: steward_types::RunnerRequirements::default(),
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
            direct_task_evidence: None,
            task_origin: steward_types::direct_package::TaskOrigin::Unknown,
            browser_task_evidence: None,
        }
    }

    fn github_task(
        task_uid: Uuid,
        owner_user_id: &str,
        attempt: u32,
    ) -> Result<TaskRecord, String> {
        let view = run(task_uid, owner_user_id);
        let mut evidence = serde_json::from_str::<serde_json::Value>(include_str!(
            "../../../docs/contracts/task/v2/fixtures/positive/task-binding-evidence.json"
        ))
        .map_err(|error| format!("parse direct evidence fixture: {error}"))?;
        evidence["taskUid"] = serde_json::json!(task_uid);
        evidence["sourceProvenance"]["repository"]["name"] =
            serde_json::json!("example-org/example-repo");
        evidence["sourceProvenance"]["run"]["id"] = serde_json::json!("12345");
        evidence["sourceProvenance"]["run"]["attempt"] = serde_json::json!(attempt);
        let evidence = serde_json::from_value(evidence)
            .map_err(|error| format!("decode direct evidence fixture: {error}"))?;
        Ok(TaskRecord {
            task_uid,
            idempotency_key: format!("github-attempt-{attempt}"),
            submitter_service: view.submitter_service,
            acting_user: view.acting_user,
            acting_user_id: Some(owner_user_id.to_owned()),
            owner: view.owner,
            owner_user_id: view.owner_user_id,
            identity_binding_state: "bound".to_owned(),
            workflow: view.workflow,
            workflow_name: view.workflow_name,
            workflow_version: view.workflow_version,
            workflow_digest: view.workflow_digest,
            user_envelope_instance_id: view.user_envelope_instance_id,
            user_envelope_revision: view.user_envelope_revision,
            user_envelope_digest: view.user_envelope_digest,
            authority_kind: Some("user-envelope".to_owned()),
            user_envelope_snapshot: None,
            internal_authority_id: None,
            internal_authority_version: None,
            internal_authority_digest: None,
            coding_agent_runtime: view.coding_agent_runtime,
            runtime_uid: view.runtime_uid,
            runtime_namespace: "steward-test".to_owned(),
            runtime_name: format!("runtime-{task_uid}"),
            runtime_ownership: view.runtime_ownership,
            phase: view.phase,
            runtime_spec: view.runtime_spec,
            agent_command: vec!["agent".to_owned()],
            execution_binding: None,
            source_provenance: None,
            direct_task_evidence: Some(evidence),
            task_origin: steward_types::direct_package::TaskOrigin::GithubActions,
            browser_task_evidence: None,
            envelope_revision: view.envelope_revision,
            orchestration_version: 3,
            orchestration_operation_id: Some(Uuid::new_v4()),
            candidate_digest: Some(format!("sha256:{}", "c".repeat(64))),
            service_envelope_digest: None,
            original_admission_decision: Some("admit".to_owned()),
            original_admission_deltas: Some(Vec::new()),
            input_archive: None,
            output_archive: None,
            execute_requested: true,
            cancel_requested: false,
            finalize_requested: view.finalize_requested,
            finalized: view.finalized,
            failure_reason: view.failure_reason,
        })
    }

    #[derive(Clone, Default)]
    struct FakeGithubRerunner {
        requests: Arc<Mutex<Vec<(String, GithubWorkflowRerunRequest)>>>,
    }

    impl GithubWorkflowRerunBroker<BrowserSessionBinding> for FakeGithubRerunner {
        fn rerun<'a>(
            &'a self,
            session: &'a ConnectionSession<BrowserSessionBinding>,
            request: &'a GithubWorkflowRerunRequest,
        ) -> BoxFuture<'a, Result<(), ConnectionBrokerError>> {
            Box::pin(async move {
                self.requests
                    .lock()
                    .map_err(|_| ConnectionBrokerError::Unavailable)?
                    .push((
                        session.subject.canonical_user_id.as_str().to_owned(),
                        request.clone(),
                    ));
                Ok(())
            })
        }
    }

    #[derive(Clone)]
    struct FailingGithubRerunner(ConnectionBrokerError);

    impl GithubWorkflowRerunBroker<BrowserSessionBinding> for FailingGithubRerunner {
        fn rerun<'a>(
            &'a self,
            _session: &'a ConnectionSession<BrowserSessionBinding>,
            _request: &'a GithubWorkflowRerunRequest,
        ) -> BoxFuture<'a, Result<(), ConnectionBrokerError>> {
            Box::pin(async move { Err(self.0.clone()) })
        }
    }

    #[derive(Clone)]
    struct FakeBrowserTaskRerunner {
        rerun_task_uid: Uuid,
        requests: Arc<Mutex<Vec<(Uuid, String)>>>,
    }

    impl BrowserTaskRerunner for FakeBrowserTaskRerunner {
        fn rerun<'a>(
            &'a self,
            _session: &'a BrowserSessionContext,
            source: &'a TaskRecord,
            idempotency_key: &'a str,
        ) -> BoxFuture<'a, Result<Uuid, BrowserTaskRerunError>> {
            Box::pin(async move {
                self.requests
                    .lock()
                    .map_err(|_| BrowserTaskRerunError::Unavailable)?
                    .push((source.task_uid, idempotency_key.to_owned()));
                Ok(self.rerun_task_uid)
            })
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
        let (service, cookie, _) = signed_in_cookie_and_csrf(identity).await?;
        Ok((service, cookie))
    }

    async fn signed_in_cookie_and_csrf(
        identity: LocalFakeIdentity,
    ) -> Result<(BrowserAuthService, String, String), String> {
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
        let session_cookie = cookie(&callback, "steward-local-session")?;
        let session = browser_auth_router(service.clone())
            .oneshot(
                Request::builder()
                    .uri("/admin/api/v1/session")
                    .header(header::COOKIE, &session_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build session request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute session request: {error}"))?;
        let body = to_bytes(session.into_body(), 64 * 1024)
            .await
            .map_err(|error| format!("read session response: {error}"))?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("parse session response: {error}"))?;
        let csrf = value["csrf"]
            .as_str()
            .ok_or_else(|| "session response omitted csrf".to_owned())?
            .to_owned();
        Ok((service, session_cookie, csrf))
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
        assert_eq!(value["runs"][0]["workflowName"], "repository-review");
        assert_eq!(value["runs"][0]["workflowVersion"], 1);
        assert_eq!(
            value["runs"][0]["workflowDigest"],
            format!("sha256:{}", "a".repeat(64))
        );
        assert_eq!(
            value["runs"][0]["userEnvelopeInstanceId"],
            "envelope-instance-1"
        );
        assert_eq!(value["runs"][0]["userEnvelopeRevision"], 4);
        assert_eq!(
            value["runs"][0]["userEnvelopeDigest"],
            format!("sha256:{}", "b".repeat(64))
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
    async fn run_events_are_owner_scoped_and_resume_after_last_event_id() -> Result<(), String> {
        let owner = "usr_0123456789abcdef0123456789abcdef";
        let other_owner = "usr_abcdefabcdefabcdefabcdefabcdefab";
        let own_task = Uuid::parse_str("11111111-1111-4111-8111-111111111111")
            .map_err(|error| error.to_string())?;
        let other_task = Uuid::parse_str("22222222-2222-4222-8222-222222222222")
            .map_err(|error| error.to_string())?;
        let ledger = FakeLedger::default();
        ledger
            .records
            .lock()
            .map_err(|_| "lock records")?
            .extend([run(own_task, owner), run(other_task, other_owner)]);
        let (service, session_cookie) = signed_in_cookie(LocalFakeIdentity::User).await?;
        let app = protected_router(ledger, service);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/app/api/v1/runs/{own_task}/events"))
                    .header(header::COOKIE, &session_cookie)
                    .header("last-event-id", (u64::MAX - 1).to_string())
                    .body(Body::empty())
                    .map_err(|error| format!("build run event request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute run event request: {error}"))?;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE),
            Some(&header::HeaderValue::from_static("text/event-stream"))
        );
        let body = to_bytes(response.into_body(), 64 * 1024)
            .await
            .map_err(|error| format!("read run event response: {error}"))?;
        let body = std::str::from_utf8(&body)
            .map_err(|error| format!("run event response was not UTF-8: {error}"))?;
        assert!(body.contains(&format!("id: {}", u64::MAX)));
        assert!(body.contains(&format!("\"taskUid\":\"{own_task}\"")));

        let hidden = app
            .oneshot(
                Request::builder()
                    .uri(format!("/app/api/v1/runs/{other_task}/events"))
                    .header(header::COOKIE, session_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build hidden run event request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute hidden run event request: {error}"))?;
        assert_eq!(hidden.status(), StatusCode::NOT_FOUND);
        Ok(())
    }

    #[tokio::test]
    async fn my_runs_accepts_an_envelope_instance_filter() -> Result<(), String> {
        let owner = "usr_0123456789abcdef0123456789abcdef";
        let ledger = FakeLedger::default();
        ledger.records.lock().map_err(|_| "lock records")?.push(run(
            Uuid::parse_str("11111111-1111-4111-8111-111111111111")
                .map_err(|error| error.to_string())?,
            owner,
        ));
        let (service, session_cookie) = signed_in_cookie(LocalFakeIdentity::User).await?;
        let response = protected_router(ledger.clone(), service)
            .oneshot(
                Request::builder()
                    .uri("/app/api/v1/runs?envelopeInstanceId=envelope-instance-1")
                    .header(header::COOKIE, session_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build envelope-runs request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute envelope-runs request: {error}"))?;
        assert_eq!(response.status(), StatusCode::OK);
        {
            let queries = ledger.queries.lock().map_err(|_| "lock queries")?;
            assert_eq!(
                queries[0].user_envelope_instance_id.as_deref(),
                Some("envelope-instance-1")
            );
            assert!(queries[0].runtime_uid.is_none());
        }
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(
                &to_bytes(response.into_body(), 16 * 1024)
                    .await
                    .map_err(|error| format!("read envelope-runs response: {error}"))?
            )
            .map_err(|error| format!("parse envelope-runs response: {error}"))?["runs"]
                .as_array()
                .map(Vec::len),
            Some(1)
        );
        Ok(())
    }

    #[tokio::test]
    async fn all_runs_requires_browser_admin_and_returns_owner_display_email() -> Result<(), String>
    {
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
        assert_eq!(value["runs"][0]["ownerDisplayEmail"], "alice@example.com");
        assert_eq!(value["facets"]["phase"]["succeeded"], 1);
        assert_eq!(
            ledger.queries.lock().map_err(|_| "lock queries")?[0]
                .owner_user_id
                .as_deref(),
            Some(owner)
        );
        Ok(())
    }

    #[tokio::test]
    async fn my_run_detail_and_timeline_are_owner_scoped_and_reverse_chronological()
    -> Result<(), String> {
        let owner = "usr_0123456789abcdef0123456789abcdef";
        let other_owner = "usr_abcdefabcdefabcdefabcdefabcdefab";
        let own_task = Uuid::parse_str("11111111-1111-4111-8111-111111111111")
            .map_err(|error| error.to_string())?;
        let other_task = Uuid::parse_str("22222222-2222-4222-8222-222222222222")
            .map_err(|error| error.to_string())?;
        let ledger = FakeLedger::default();
        ledger
            .records
            .lock()
            .map_err(|_| "lock records")?
            .extend([run(own_task, owner), run(other_task, other_owner)]);

        let (service, session_cookie) = signed_in_cookie(LocalFakeIdentity::User).await?;
        let absent = protected_router(ledger.clone(), service)
            .oneshot(
                Request::builder()
                    .uri(format!("/app/api/v1/runs/{other_task}"))
                    .header(header::COOKIE, session_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build cross-owner detail request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute cross-owner detail request: {error}"))?;
        assert_eq!(absent.status(), StatusCode::NOT_FOUND);

        let (service, session_cookie) = signed_in_cookie(LocalFakeIdentity::User).await?;
        let timeline = protected_router(ledger.clone(), service)
            .oneshot(
                Request::builder()
                    .uri(format!("/app/api/v1/runs/{own_task}/timeline"))
                    .header(header::COOKIE, session_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build scoped timeline request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute scoped timeline request: {error}"))?;
        assert_eq!(timeline.status(), StatusCode::OK);
        let body = to_bytes(timeline.into_body(), 16 * 1024)
            .await
            .map_err(|error| format!("read scoped timeline response: {error}"))?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("parse scoped timeline response: {error}"))?;
        assert_eq!(value["events"][0]["kind"], "finalized");
        assert_eq!(value["events"][2]["kind"], "admitted");
        assert_eq!(value["events"][2]["envelopeRevision"], 4);
        assert!(value["events"][2].get("details").is_none());
        assert_eq!(
            ledger.queries.lock().map_err(|_| "lock queries")?[1]
                .owner_user_id
                .as_deref(),
            Some(owner)
        );
        assert_eq!(
            ledger.queries.lock().map_err(|_| "lock queries")?[1].task_uid,
            Some(own_task)
        );
        Ok(())
    }

    #[tokio::test]
    async fn cancelling_a_terminal_run_is_a_conflict() -> Result<(), String> {
        let owner = "usr_0123456789abcdef0123456789abcdef";
        let task_uid = Uuid::parse_str("11111111-1111-4111-8111-111111111111")
            .map_err(|error| error.to_string())?;
        let ledger = FakeLedger::default();
        ledger
            .records
            .lock()
            .map_err(|_| "lock records")?
            .push(run(task_uid, owner));
        let (service, session_cookie, csrf) =
            signed_in_cookie_and_csrf(LocalFakeIdentity::User).await?;
        let response = protected_router(ledger, service)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/app/api/v1/runs/{task_uid}/cancel"))
                    .header(header::COOKIE, session_cookie)
                    .header(header::ORIGIN, "http://127.0.0.1:33001")
                    .header("sec-fetch-site", "same-origin")
                    .header("x-steward-csrf", csrf)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .map_err(|error| format!("build cancel request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute cancel request: {error}"))?;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        Ok(())
    }

    #[tokio::test]
    async fn execution_logs_are_owner_scoped_typed_and_incremental() -> Result<(), String> {
        let owner = "usr_0123456789abcdef0123456789abcdef";
        let other_owner = "usr_abcdefabcdefabcdefabcdefabcdefab";
        let own_task = Uuid::parse_str("11111111-1111-4111-8111-111111111111")
            .map_err(|error| error.to_string())?;
        let other_task = Uuid::parse_str("22222222-2222-4222-8222-222222222222")
            .map_err(|error| error.to_string())?;
        let ledger = FakeLedger::default();
        ledger
            .records
            .lock()
            .map_err(|_| "lock records")?
            .extend([run(own_task, owner), run(other_task, other_owner)]);
        ledger.logs.lock().map_err(|_| "lock logs")?.insert(
            (own_task, AgentRunLogStream::Stdout),
            b"completed\n".to_vec(),
        );
        ledger.logs.lock().map_err(|_| "lock logs")?.insert(
            (other_task, AgentRunLogStream::Stderr),
            b"failed\n".to_vec(),
        );

        let (service, session_cookie) = signed_in_cookie(LocalFakeIdentity::User).await?;
        let response = protected_router(ledger.clone(), service)
            .oneshot(
                Request::builder()
                    .uri(format!("/app/api/v1/runs/{own_task}/logs/stdout?after=0"))
                    .header(header::COOKIE, session_cookie)
                    .header(header::ACCEPT, "application/json")
                    .body(Body::empty())
                    .map_err(|error| format!("build stdout request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute stdout request: {error}"))?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024)
            .await
            .map_err(|error| format!("read stdout response: {error}"))?;
        let body: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("decode stdout response: {error}"))?;
        assert_eq!(body["stream"], "stdout");
        assert_eq!(body["content"], "completed\n");
        assert_eq!(body["sizeBytes"], 10);
        assert_eq!(body["complete"], true);
        assert_eq!(body["truncated"], false);

        let (service, session_cookie) = signed_in_cookie(LocalFakeIdentity::User).await?;
        let legacy = protected_router(ledger.clone(), service)
            .oneshot(
                Request::builder()
                    .uri(format!("/app/api/v1/runs/{own_task}/logs/stdout"))
                    .header(header::COOKIE, session_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build legacy stdout request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute legacy stdout request: {error}"))?;
        assert_eq!(legacy.status(), StatusCode::OK);
        assert_eq!(
            legacy.headers().get(header::CONTENT_TYPE),
            Some(&header::HeaderValue::from_static(
                "text/plain; charset=utf-8"
            ))
        );
        assert_eq!(
            legacy.headers().get(header::CACHE_CONTROL),
            Some(&header::HeaderValue::from_static("no-store"))
        );
        assert_eq!(
            legacy.headers().get("x-content-type-options"),
            Some(&header::HeaderValue::from_static("nosniff"))
        );
        let legacy_body = to_bytes(legacy.into_body(), 1024)
            .await
            .map_err(|error| format!("read legacy stdout response: {error}"))?;
        assert_eq!(legacy_body.as_ref(), b"completed\n");

        let (service, session_cookie) = signed_in_cookie(LocalFakeIdentity::User).await?;
        let hidden = protected_router(ledger, service)
            .oneshot(
                Request::builder()
                    .uri(format!("/app/api/v1/runs/{other_task}/logs/stderr"))
                    .header(header::COOKIE, session_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build cross-owner stderr request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute cross-owner stderr request: {error}"))?;
        assert_eq!(hidden.status(), StatusCode::NOT_FOUND);
        Ok(())
    }

    #[tokio::test]
    async fn completed_outputs_are_listed_downloadable_and_owner_scoped() -> Result<(), String> {
        let owner = "usr_0123456789abcdef0123456789abcdef";
        let other_owner = "usr_abcdefabcdefabcdefabcdefabcdefab";
        let own_task = Uuid::parse_str("11111111-1111-4111-8111-111111111111")
            .map_err(|error| error.to_string())?;
        let other_task = Uuid::parse_str("22222222-2222-4222-8222-222222222222")
            .map_err(|error| error.to_string())?;
        let ledger = FakeLedger::default();
        ledger
            .records
            .lock()
            .map_err(|_| "lock records")?
            .extend([run(own_task, owner), run(other_task, other_owner)]);
        ledger.outputs.lock().map_err(|_| "lock outputs")?.extend([
            (own_task, output_tar("out/report.txt", b"complete\n")),
            (other_task, output_tar("out/secret.txt", b"hidden\n")),
        ]);

        let (service, session_cookie) = signed_in_cookie(LocalFakeIdentity::User).await?;
        let app = protected_router(ledger.clone(), service);
        let listed = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/app/api/v1/runs/{own_task}/outputs"))
                    .header(header::COOKIE, &session_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build output list request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute output list request: {error}"))?;
        assert_eq!(listed.status(), StatusCode::OK);
        let listed = to_bytes(listed.into_body(), 4096)
            .await
            .map_err(|error| format!("read output list: {error}"))?;
        let listed: serde_json::Value = serde_json::from_slice(&listed)
            .map_err(|error| format!("decode output list: {error}"))?;
        assert_eq!(listed["files"][0]["path"], "report.txt");
        assert_eq!(listed["files"][0]["sizeBytes"], 9);

        let downloaded = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/app/api/v1/runs/{own_task}/outputs/report.txt"))
                    .header(header::COOKIE, &session_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build output download request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute output download request: {error}"))?;
        assert_eq!(downloaded.status(), StatusCode::OK);
        assert_eq!(
            downloaded.headers().get(header::CONTENT_DISPOSITION),
            Some(&header::HeaderValue::from_static(
                "attachment; filename=\"report.txt\""
            ))
        );
        let downloaded = to_bytes(downloaded.into_body(), 4096)
            .await
            .map_err(|error| format!("read output download: {error}"))?;
        assert_eq!(downloaded.as_ref(), b"complete\n");

        let hidden = app
            .oneshot(
                Request::builder()
                    .uri(format!("/app/api/v1/runs/{other_task}/outputs/secret.txt"))
                    .header(header::COOKIE, session_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build hidden output request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute hidden output request: {error}"))?;
        assert_eq!(hidden.status(), StatusCode::NOT_FOUND);
        Ok(())
    }

    #[tokio::test]
    async fn successful_inline_package_is_exact_and_owner_scoped() -> Result<(), String> {
        let owner = "usr_0123456789abcdef0123456789abcdef";
        let other_owner = "usr_abcdefabcdefabcdefabcdefabcdefab";
        let own_task = Uuid::parse_str("33333333-3333-4333-8333-333333333333")
            .map_err(|error| error.to_string())?;
        let other_task = Uuid::parse_str("44444444-4444-4444-8444-444444444444")
            .map_err(|error| error.to_string())?;
        let failed_task = Uuid::parse_str("55555555-5555-4555-8555-555555555555")
            .map_err(|error| error.to_string())?;
        let digest = format!("steward:sha256:{}", "d".repeat(64));
        let evidence = BrowserTaskEvidence {
            source: "inline".to_owned(),
            revision: digest.clone(),
            path: RelativePath::parse("task-definition.json")?,
            closure: None,
            closure_digest: ContentDigest::parse(digest)?,
            inline_files: Some(BTreeMap::from([
                ("prompt.md".to_owned(), "Say hello.\n".to_owned()),
                (
                    "task-definition.json".to_owned(),
                    "{\"schemaVersion\":\"steward.task-definition/v2\"}".to_owned(),
                ),
            ])),
            diagnostics: Default::default(),
        };
        let mut own = run(own_task, owner);
        own.task_origin = TaskOrigin::Browser;
        own.browser_task_evidence = Some(evidence.clone());
        let mut other = run(other_task, other_owner);
        other.task_origin = TaskOrigin::Browser;
        other.browser_task_evidence = Some(evidence.clone());
        let mut failed = run(failed_task, owner);
        failed.phase = TaskPhase::Failed;
        failed.task_origin = TaskOrigin::Browser;
        failed.browser_task_evidence = Some(evidence);
        let ledger = FakeLedger::default();
        ledger
            .records
            .lock()
            .map_err(|_| "lock records")?
            .extend([own, other, failed]);

        let (service, session_cookie) = signed_in_cookie(LocalFakeIdentity::User).await?;
        let app = protected_router(ledger, service);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/app/api/v1/runs/{own_task}/package"))
                    .header(header::COOKIE, &session_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build exact package request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute exact package request: {error}"))?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 4096)
            .await
            .map_err(|error| format!("read exact package response: {error}"))?;
        let body: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("decode exact package response: {error}"))?;
        assert_eq!(body["taskUid"], own_task.to_string());
        assert_eq!(body["files"]["prompt.md"], "Say hello.\n");

        for hidden_task in [other_task, failed_task] {
            let hidden = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/app/api/v1/runs/{hidden_task}/package"))
                        .header(header::COOKIE, &session_cookie)
                        .body(Body::empty())
                        .map_err(|error| format!("build hidden package request: {error}"))?,
                )
                .await
                .map_err(|error| format!("execute hidden package request: {error}"))?;
            assert_eq!(hidden.status(), StatusCode::NOT_FOUND);
        }
        Ok(())
    }

    #[tokio::test]
    async fn browser_origin_rerun_uses_the_browser_submission_boundary() -> Result<(), String> {
        let owner = "usr_0123456789abcdef0123456789abcdef";
        let source_task_uid = Uuid::parse_str("11111111-1111-4111-8111-111111111111")
            .map_err(|error| error.to_string())?;
        let rerun_task_uid = Uuid::parse_str("22222222-2222-4222-8222-222222222222")
            .map_err(|error| error.to_string())?;
        let mut source = github_task(source_task_uid, owner, 1)?;
        source.direct_task_evidence = None;
        source.task_origin = TaskOrigin::Browser;
        source.browser_task_evidence = Some(BrowserTaskEvidence {
            source: "inline".to_owned(),
            revision: format!("steward:sha256:{}", "d".repeat(64)),
            path: RelativePath::parse("task-definition.json")?,
            closure: None,
            closure_digest: ContentDigest::parse(format!("steward:sha256:{}", "d".repeat(64)))?,
            inline_files: Some(BTreeMap::from([(
                "task-definition.json".to_owned(),
                "{}".to_owned(),
            )])),
            diagnostics: Default::default(),
        });
        let ledger = FakeLedger::default();
        ledger
            .rerun_sources
            .lock()
            .map_err(|_| "lock rerun sources")?
            .insert(source_task_uid, source);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let rerunner: Arc<dyn BrowserTaskRerunner> = Arc::new(FakeBrowserTaskRerunner {
            rerun_task_uid,
            requests: requests.clone(),
        });
        let (service, session_cookie, csrf) =
            signed_in_cookie_and_csrf(LocalFakeIdentity::User).await?;
        let response = protected_router_with_task_reruns(ledger, rerunner, service)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/app/api/v1/runs/{source_task_uid}/rerun"))
                    .header(header::COOKIE, session_cookie)
                    .header(header::ORIGIN, "http://127.0.0.1:33001")
                    .header("sec-fetch-site", "same-origin")
                    .header("x-steward-csrf", csrf)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"idempotencyKey":"one-click"}"#))
                    .map_err(|error| format!("build browser rerun request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute browser rerun request: {error}"))?;
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = to_bytes(response.into_body(), 4096)
            .await
            .map_err(|error| format!("read browser rerun response: {error}"))?;
        let body: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("decode browser rerun response: {error}"))?;
        assert_eq!(body["taskUid"], rerun_task_uid.to_string());
        assert_eq!(
            requests
                .lock()
                .map_err(|_| "lock browser rerun requests")?
                .as_slice(),
            &[(
                source_task_uid,
                format!("browser-rerun:{source_task_uid}:one-click")
            )]
        );
        Ok(())
    }

    #[tokio::test]
    async fn github_rerun_dispatches_once_then_returns_the_correlated_higher_attempt()
    -> Result<(), String> {
        let owner = "usr_0123456789abcdef0123456789abcdef";
        let source_task_uid = Uuid::parse_str("11111111-1111-4111-8111-111111111111")
            .map_err(|error| error.to_string())?;
        let rerun_task_uid = Uuid::parse_str("22222222-2222-4222-8222-222222222222")
            .map_err(|error| error.to_string())?;
        let source = github_task(source_task_uid, owner, 1)?;
        let rerun = github_task(rerun_task_uid, owner, 2)?;
        let ledger = FakeLedger::default();
        ledger
            .rerun_sources
            .lock()
            .map_err(|_| "lock rerun sources")?
            .insert(source_task_uid, source);
        ledger
            .github_matches
            .lock()
            .map_err(|_| "lock github matches")?
            .extend([None, None, Some(rerun)]);
        let broker = FakeGithubRerunner::default();
        let (service, session_cookie, csrf) =
            signed_in_cookie_and_csrf(LocalFakeIdentity::User).await?;
        let app = protected_router_with_github_reruns(ledger.clone(), broker.clone(), service);
        let request = || {
            Request::builder()
                .method("POST")
                .uri(format!("/app/api/v1/runs/{source_task_uid}/rerun"))
                .header(header::COOKIE, &session_cookie)
                .header(header::ORIGIN, "http://127.0.0.1:33001")
                .header("sec-fetch-site", "same-origin")
                .header("x-steward-csrf", &csrf)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"idempotencyKey":"one-click"}"#))
                .map_err(|error| format!("build rerun request: {error}"))
        };

        let pending = app
            .clone()
            .oneshot(request()?)
            .await
            .map_err(|error| format!("execute pending rerun: {error}"))?;
        assert_eq!(pending.status(), StatusCode::ACCEPTED);
        let pending_body = to_bytes(pending.into_body(), 4096)
            .await
            .map_err(|error| format!("read pending rerun: {error}"))?;
        let pending_body: serde_json::Value = serde_json::from_slice(&pending_body)
            .map_err(|error| format!("decode pending rerun: {error}"))?;
        assert_eq!(pending_body["state"], "pending");
        assert_eq!(pending_body["retryAfterMs"], 1_000);

        let completed = app
            .oneshot(request()?)
            .await
            .map_err(|error| format!("execute completed rerun: {error}"))?;
        assert_eq!(completed.status(), StatusCode::OK);
        let completed_body = to_bytes(completed.into_body(), 4096)
            .await
            .map_err(|error| format!("read completed rerun: {error}"))?;
        let completed_body: serde_json::Value = serde_json::from_slice(&completed_body)
            .map_err(|error| format!("decode completed rerun: {error}"))?;
        assert_eq!(completed_body["taskUid"], rerun_task_uid.to_string());

        let requests = broker.requests.lock().map_err(|_| "lock rerun requests")?;
        assert_eq!(
            requests.len(),
            1,
            "polling must not dispatch a second provider rerun"
        );
        assert_eq!(requests[0].0, owner);
        assert_eq!(
            requests[0].1,
            GithubWorkflowRerunRequest {
                owner: "example-org".to_owned(),
                repository: "example-repo".to_owned(),
                run_id: 12_345,
                idempotency_key: format!("browser-rerun:{source_task_uid}:one-click"),
            }
        );
        assert_eq!(
            ledger
                .github_queries
                .lock()
                .map_err(|_| "lock github queries")?
                .as_slice(),
            [
                (
                    owner.to_owned(),
                    "example-org/example-repo".to_owned(),
                    "12345".to_owned(),
                    1
                ),
                (
                    owner.to_owned(),
                    "example-org/example-repo".to_owned(),
                    "12345".to_owned(),
                    1
                ),
                (
                    owner.to_owned(),
                    "example-org/example-repo".to_owned(),
                    "12345".to_owned(),
                    1
                ),
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn github_rerun_preserves_the_staged_orchestration_diagnostic() -> Result<(), String> {
        let owner = "usr_0123456789abcdef0123456789abcdef";
        let source_task_uid = Uuid::parse_str("11111111-1111-4111-8111-111111111111")
            .map_err(|error| error.to_string())?;
        let ledger = FakeLedger::default();
        ledger
            .rerun_sources
            .lock()
            .map_err(|_| "lock rerun sources")?
            .insert(source_task_uid, github_task(source_task_uid, owner, 1)?);
        let (service, session_cookie, csrf) =
            signed_in_cookie_and_csrf(LocalFakeIdentity::User).await?;
        let app = protected_router_with_github_reruns(
            ledger,
            FailingGithubRerunner(ConnectionBrokerError::OrchestrationNotActive),
            service,
        );

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/app/api/v1/runs/{source_task_uid}/rerun"))
                    .header(header::COOKIE, session_cookie)
                    .header(header::ORIGIN, "http://127.0.0.1:33001")
                    .header("sec-fetch-site", "same-origin")
                    .header("x-steward-csrf", csrf)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"idempotencyKey":"one-click"}"#))
                    .map_err(|error| format!("build staged rerun request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute staged rerun: {error}"))?;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), 1024)
            .await
            .map_err(|error| format!("read staged rerun response: {error}"))?;
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body)
                .map_err(|error| format!("decode staged rerun response: {error}"))?,
            serde_json::json!({ "error": "connections.orchestration_not_active" })
        );
        Ok(())
    }

    #[tokio::test]
    async fn administrators_can_read_failed_execution_stderr() -> Result<(), String> {
        let task_uid = Uuid::parse_str("33333333-3333-4333-8333-333333333333")
            .map_err(|error| error.to_string())?;
        let ledger = FakeLedger::default();
        let mut failed = run(task_uid, "usr_abcdefabcdefabcdefabcdefabcdefab");
        failed.phase = TaskPhase::Failed;
        ledger
            .records
            .lock()
            .map_err(|_| "lock records")?
            .push(failed);
        ledger.logs.lock().map_err(|_| "lock logs")?.insert(
            (task_uid, AgentRunLogStream::Stderr),
            b"agent failed\n".to_vec(),
        );

        let (user_service, user_cookie) = signed_in_cookie(LocalFakeIdentity::User).await?;
        let denied = protected_router(ledger.clone(), user_service)
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/admin/api/v1/all-runs/{task_uid}/logs/stderr?after=0"
                    ))
                    .header(header::COOKIE, user_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build denied stderr request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute denied stderr request: {error}"))?;
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);

        let (admin_service, admin_cookie) = signed_in_cookie(LocalFakeIdentity::Admin).await?;
        let response = protected_router(ledger, admin_service)
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/admin/api/v1/all-runs/{task_uid}/logs/stderr?after=0"
                    ))
                    .header(header::COOKIE, admin_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build admin stderr request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute admin stderr request: {error}"))?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024)
            .await
            .map_err(|error| format!("read failed stderr response: {error}"))?;
        let body: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("decode failed stderr response: {error}"))?;
        assert_eq!(body["stream"], "stderr");
        assert_eq!(body["content"], "agent failed\n");
        assert_eq!(body["complete"], true);
        Ok(())
    }

    fn output_tar_entries(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut archive = Vec::new();
        for (path, content) in entries {
            let header_offset = archive.len();
            archive.resize(header_offset + 512, 0);
            let header = &mut archive[header_offset..header_offset + 512];
            header[..path.len()].copy_from_slice(path.as_bytes());
            header[100..108].copy_from_slice(b"0000644\0");
            header[108..116].copy_from_slice(b"0000000\0");
            header[116..124].copy_from_slice(b"0000000\0");
            let size = format!("{:011o}\0", content.len());
            header[124..136].copy_from_slice(size.as_bytes());
            header[136..148].copy_from_slice(b"00000000000\0");
            header[148..156].fill(b' ');
            header[156] = b'0';
            header[257..263].copy_from_slice(b"ustar\0");
            header[263..265].copy_from_slice(b"00");
            let checksum: u64 = header.iter().map(|byte| u64::from(*byte)).sum();
            header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
            archive.extend_from_slice(content);
            archive.resize(header_offset + 512 + content.len().div_ceil(512) * 512, 0);
        }
        archive.resize(archive.len() + 1024, 0);
        archive
    }

    fn output_tar(path: &str, content: &[u8]) -> Vec<u8> {
        output_tar_entries(&[(path, content)])
    }

    #[test]
    fn output_archive_exposes_only_bounded_files_below_out() {
        let archive = output_tar("out/result.txt", b"hello\n");
        assert_eq!(
            output_archive_entries(&archive),
            Ok(vec![OutputArchiveEntry {
                path: "result.txt".to_owned(),
                offset: 512,
                size: 6,
            }])
        );
        assert!(output_archive_entries(&output_tar("secret.txt", b"no")).is_err());
        assert!(output_archive_entries(&output_tar("out/../secret.txt", b"no")).is_err());
    }

    #[test]
    fn output_archive_skips_only_reserved_execution_logs() {
        let archive = output_tar_entries(&[
            ("out/tool-calls.json", b"[]\n"),
            ("out/report.md", b"complete\n"),
            (".steward/diagnostics/stdout.log", b"agent output\n"),
            (".steward/diagnostics/stderr.log", b"agent warning\n"),
        ]);
        let entries = output_archive_entries(&archive).unwrap_or_default();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            vec!["tool-calls.json", "report.md"]
        );

        let unexpected = output_tar_entries(&[
            ("out/report.md", b"complete\n"),
            (".steward/diagnostics/trace.log", b"not reserved\n"),
        ]);
        assert!(output_archive_entries(&unexpected).is_err());
    }
}
