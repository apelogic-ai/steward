//! Browser-session-bound Agent Runs read APIs.
//!
//! The user route derives its exact owner scope from the authenticated browser session. It never
//! accepts a client-provided identity and therefore cannot be widened by the page. The separate
//! All Runs route requires a browser-admin session; the existing bearer administrator API remains
//! independent at `/admin/api/v1/runs`.

use std::collections::{BTreeMap, VecDeque};
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use steward_store::{
    AgentRunExecutionLog, AgentRunLogStream, AgentRunOutputArchive, AgentRunPage, AgentRunQuery,
    AgentRunRecord, AgentRunTimelineEvent, AgentRunTimelineKind, BrowserTaskVersionPublication,
    BrowserTaskVersionRecord, StoreError, TaskRecord, WorkflowRevisionRecord,
};
use steward_types::direct_package::{
    AgentRef, ContentDigest, DirectRequirements, DirectTaskDefinition, ExecutionLogMode,
    PackageClosure, PromptSourceKind, RelativePath, RuntimeSelection, TaskOrigin,
};
use steward_types::task_output_archive::{
    TASK_OUTPUT_ARCHIVE_CONTRACT, TaskOutputArchiveCompatibility, TaskOutputArchiveEntry,
    task_output_archive_entries,
};
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
pub const BROWSER_TASKS_API_VERSION: &str = "steward.browser-tasks/v1";

#[derive(Serialize, utoipa::ToSchema)]
struct RerunErrorResponse {
    error: &'static str,
}

#[derive(Clone)]
pub(crate) struct BrowserRunsState<L> {
    ledger: L,
    github_rerunner: Arc<dyn BrowserGithubRerunner>,
    browser_task_rerunner: Arc<dyn BrowserTaskRerunner>,
    event_streams: Arc<BrowserRunEventStreams>,
}

const MAX_BROWSER_RUN_EVENT_STREAMS_PER_USER: usize = 4;
const MAX_BROWSER_RUN_EVENT_HISTORY: usize = 128;
const MAX_BROWSER_RUN_EVENT_TASKS: usize = 256;

#[cfg(not(test))]
const BROWSER_RUN_EVENT_TERMINAL_GRACE: Duration = Duration::from_secs(30);
#[cfg(test)]
const BROWSER_RUN_EVENT_TERMINAL_GRACE: Duration = Duration::from_millis(50);

#[cfg(not(test))]
const BROWSER_RUN_EVENT_POLL_INTERVAL: Duration = Duration::from_secs(1);
#[cfg(test)]
const BROWSER_RUN_EVENT_POLL_INTERVAL: Duration = Duration::from_millis(10);
#[cfg(not(test))]
const BROWSER_RUN_EVENT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
#[cfg(test)]
const BROWSER_RUN_EVENT_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(5);

#[derive(Clone)]
struct BufferedBrowserRunEvent {
    id: u64,
    kind: &'static str,
    data: String,
    fingerprint: String,
    terminal: bool,
}

struct BrowserRunEventHistory {
    events: VecDeque<BufferedBrowserRunEvent>,
    last_access_id: u64,
    terminal_delivered_at: Option<Instant>,
}

struct BrowserRunEventStreamsState {
    active_by_user: BTreeMap<String, usize>,
    active_by_task: BTreeMap<Uuid, usize>,
    history_by_task: BTreeMap<Uuid, BrowserRunEventHistory>,
    next_event_id: u64,
    next_access_id: u64,
}

impl Default for BrowserRunEventStreamsState {
    fn default() -> Self {
        Self {
            active_by_user: BTreeMap::new(),
            active_by_task: BTreeMap::new(),
            history_by_task: BTreeMap::new(),
            next_event_id: 1,
            next_access_id: 1,
        }
    }
}

impl BrowserRunEventStreamsState {
    fn next_access_id(&mut self) -> Result<u64, ()> {
        let access_id = self.next_access_id;
        self.next_access_id = self.next_access_id.checked_add(1).ok_or(())?;
        Ok(access_id)
    }

    fn prune_expired_terminal_history(&mut self, now: Instant) {
        self.history_by_task.retain(|task_uid, history| {
            self.active_by_task.get(task_uid).copied().unwrap_or(0) > 0
                || history.terminal_delivered_at.is_none_or(|delivered_at| {
                    now.saturating_duration_since(delivered_at) < BROWSER_RUN_EVENT_TERMINAL_GRACE
                })
        });
    }

    fn make_room_for_task(&mut self, task_uid: Uuid) -> Result<(), ()> {
        if self.history_by_task.contains_key(&task_uid) {
            return Ok(());
        }
        while self.history_by_task.len() >= MAX_BROWSER_RUN_EVENT_TASKS {
            let Some(eviction) = self
                .history_by_task
                .iter()
                .filter(|(candidate, _)| {
                    self.active_by_task.get(candidate).copied().unwrap_or(0) == 0
                })
                .min_by_key(|(_, history)| history.last_access_id)
                .map(|(candidate, _)| *candidate)
            else {
                return Err(());
            };
            self.history_by_task.remove(&eviction);
        }
        Ok(())
    }
}

#[derive(Default)]
struct BrowserRunEventStreams {
    state: Mutex<BrowserRunEventStreamsState>,
}

struct BrowserRunEventStreamPermit {
    owner_user_id: String,
    task_uid: Uuid,
    streams: Arc<BrowserRunEventStreams>,
}

impl Drop for BrowserRunEventStreamPermit {
    fn drop(&mut self) {
        if let Ok(mut state) = self.streams.state.lock() {
            if let Some(active) = state.active_by_user.get_mut(&self.owner_user_id) {
                *active = active.saturating_sub(1);
                if *active == 0 {
                    state.active_by_user.remove(&self.owner_user_id);
                }
            }
            if let Some(active) = state.active_by_task.get_mut(&self.task_uid) {
                *active = active.saturating_sub(1);
                if *active == 0 {
                    state.active_by_task.remove(&self.task_uid);
                }
            }
            state.prune_expired_terminal_history(Instant::now());
        }
    }
}

impl BrowserRunEventStreams {
    fn acquire(
        self: &Arc<Self>,
        owner_user_id: &str,
        task_uid: Uuid,
    ) -> Result<BrowserRunEventStreamPermit, ()> {
        let mut state = self.state.lock().map_err(|_| ())?;
        state.prune_expired_terminal_history(Instant::now());
        let active_for_user = state
            .active_by_user
            .get(owner_user_id)
            .copied()
            .unwrap_or(0);
        if active_for_user >= MAX_BROWSER_RUN_EVENT_STREAMS_PER_USER {
            return Err(());
        }
        let active_for_task = state.active_by_task.get(&task_uid).copied().unwrap_or(0);
        let next_active_for_user = active_for_user.checked_add(1).ok_or(())?;
        let next_active_for_task = active_for_task.checked_add(1).ok_or(())?;
        state
            .active_by_user
            .insert(owner_user_id.to_owned(), next_active_for_user);
        state.active_by_task.insert(task_uid, next_active_for_task);
        Ok(BrowserRunEventStreamPermit {
            owner_user_id: owner_user_id.to_owned(),
            task_uid,
            streams: self.clone(),
        })
    }

    fn observe(&self, task_uid: Uuid, mut snapshot: BrowserRunEventSnapshot) -> Result<(), ()> {
        snapshot.event_id = 0;
        let fingerprint = serde_json::to_string(&snapshot).map_err(|_| ())?;
        let mut state = self.state.lock().map_err(|_| ())?;
        state.prune_expired_terminal_history(Instant::now());
        if state
            .history_by_task
            .get(&task_uid)
            .and_then(|history| history.events.back())
            .is_some_and(|event| event.fingerprint == fingerprint)
        {
            return Ok(());
        }
        state.make_room_for_task(task_uid)?;
        let event_id = state.next_event_id;
        state.next_event_id = state.next_event_id.checked_add(1).ok_or(())?;
        let access_id = state.next_access_id()?;
        snapshot.event_id = event_id;
        let data = serde_json::to_string(&snapshot).map_err(|_| ())?;
        let terminal = is_terminal_phase(snapshot.run.phase) && snapshot.run.finalized;
        let history =
            state
                .history_by_task
                .entry(task_uid)
                .or_insert_with(|| BrowserRunEventHistory {
                    events: VecDeque::new(),
                    last_access_id: access_id,
                    terminal_delivered_at: None,
                });
        history.last_access_id = access_id;
        history.terminal_delivered_at = None;
        let kind = if history.events.is_empty() {
            "snapshot"
        } else {
            "run"
        };
        history.events.push_back(BufferedBrowserRunEvent {
            id: event_id,
            kind,
            data,
            fingerprint,
            terminal,
        });
        while history.events.len() > MAX_BROWSER_RUN_EVENT_HISTORY {
            history.events.pop_front();
        }
        Ok(())
    }

    fn resume(
        self: &Arc<Self>,
        task_uid: Uuid,
        last_event_id: u64,
    ) -> Result<Vec<BufferedBrowserRunEvent>, ()> {
        let now = Instant::now();
        let (events, schedule_terminal_eviction) = {
            let mut state = self.state.lock().map_err(|_| ())?;
            state.prune_expired_terminal_history(now);
            let access_id = state.next_access_id()?;
            let Some(history) = state.history_by_task.get_mut(&task_uid) else {
                return Ok(Vec::new());
            };
            history.last_access_id = access_id;
            let events = if last_event_id == 0 {
                history
                    .events
                    .back()
                    .cloned()
                    .into_iter()
                    .map(|mut event| {
                        event.kind = "snapshot";
                        event
                    })
                    .collect::<Vec<_>>()
            } else if let Some(position) = history
                .events
                .iter()
                .position(|event| event.id == last_event_id)
            {
                history.events.iter().skip(position + 1).cloned().collect()
            } else {
                history
                    .events
                    .back()
                    .cloned()
                    .into_iter()
                    .map(|mut event| {
                        event.kind = "snapshot";
                        event
                    })
                    .collect()
            };
            let delivered_terminal = events.iter().any(|event| event.terminal);
            let schedule = delivered_terminal && history.terminal_delivered_at.is_none();
            if schedule {
                history.terminal_delivered_at = Some(now);
            }
            (events, schedule)
        };
        if schedule_terminal_eviction && let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let streams = self.clone();
            runtime.spawn(async move {
                tokio::time::sleep(BROWSER_RUN_EVENT_TERMINAL_GRACE).await;
                streams.prune_expired_terminal_history();
            });
        }
        Ok(events)
    }

    fn latest_is_terminal(&self, task_uid: Uuid) -> Result<bool, ()> {
        let state = self.state.lock().map_err(|_| ())?;
        Ok(state
            .history_by_task
            .get(&task_uid)
            .and_then(|history| history.events.back())
            .is_some_and(|event| event.terminal))
    }

    fn prune_expired_terminal_history(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.prune_expired_terminal_history(Instant::now());
        }
    }
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
    prompt_source: PromptSourceKind,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserRunPackageContentResponse {
    #[schema(value_type = String, format = "uuid")]
    task_uid: Uuid,
    files: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct BrowserTaskQuery {
    #[serde(default = "default_limit")]
    limit: u16,
    cursor: Option<Uuid>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SaveBrowserTaskRequest {
    #[schema(value_type = Option<String>, format = "uuid")]
    task_id: Option<Uuid>,
    path: RelativePath,
    #[schema(value_type = Object)]
    files: BTreeMap<String, String>,
    #[serde(default)]
    shared_roles: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserTaskVersionView {
    version: u64,
    content_digest: String,
    created_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserTaskListItem {
    #[schema(value_type = Option<String>, format = "uuid")]
    task_id: Option<Uuid>,
    content_digest: String,
    source: String,
    revision: String,
    path: String,
    name: String,
    version: u64,
    owned: bool,
    editable: bool,
    shared_roles: Vec<String>,
    updated_at: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserTasksResponse {
    api_version: &'static str,
    tasks: Vec<BrowserTaskListItem>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SaveBrowserTaskResponse {
    api_version: &'static str,
    task: BrowserTaskListItem,
}

#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserTaskView {
    #[schema(value_type = Option<String>, format = "uuid")]
    task_id: Option<Uuid>,
    content_digest: String,
    source: String,
    revision: String,
    path: String,
    name: String,
    version: u64,
    runtime: RuntimeSelection,
    requires: Option<DirectRequirements>,
    files: BTreeMap<String, String>,
    closure: Option<PackageClosure>,
    owned: bool,
    editable: bool,
    shared_roles: Vec<String>,
    versions: Vec<BrowserTaskVersionView>,
    runs: Vec<BrowserRunView>,
    #[schema(value_type = Option<String>, format = "uuid")]
    next_cursor: Option<Uuid>,
    #[schema(value_type = Option<String>, format = "uuid")]
    publication_task_uid: Option<Uuid>,
}

#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserTaskResponse {
    api_version: &'static str,
    task: BrowserTaskView,
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
        .route(
            "/app/api/v1/tasks",
            get(my_tasks::<L>).post(save_my_task::<L>),
        )
        .route("/app/api/v1/tasks/{content_digest}", get(my_task::<L>))
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
            event_streams: Arc::new(BrowserRunEventStreams::default()),
        })
}

#[utoipa::path(
    get,
    operation_id = "myTask",
    path = "/app/api/v1/tasks/{content_digest}",
    params(
        ("content_digest" = String, Path),
        ("limit" = Option<u16>, Query),
        ("cursor" = Option<String>, Query, format = "uuid")
    ),
    responses(
        (status = 200, body = BrowserTaskResponse),
        (status = 400, description = "Task digest or run query is invalid"),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 404, description = "Task is not visible in the user's scope"),
        (status = 503, description = "Task history is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn my_task<L>(
    session: Option<Extension<BrowserSessionContext>>,
    State(state): State<BrowserRunsState<L>>,
    Path(content_digest): Path<String>,
    Query(query): Query<BrowserTaskQuery>,
) -> Response
where
    L: AgentRunLedger,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if !valid_task_content_digest(&content_digest) {
        return browser_runs_error(StatusCode::BAD_REQUEST);
    }
    let owner_user_id = session.principal.canonical_user_id.as_str();
    let run_query = AgentRunQuery {
        limit: query.limit,
        cursor: query.cursor,
        phase: None,
        workflow: None,
        owner_user_id: Some(owner_user_id.to_owned()),
        runtime_uid: None,
        user_envelope_instance_id: None,
        task_uid: None,
        package_digest: Some(content_digest.clone()),
    };
    let page = match state.ledger.agent_runs(&run_query).await {
        Ok(page) => page,
        Err(StoreError::InvalidRunQuery | StoreError::InvalidRunCursor) => {
            return browser_runs_error(StatusCode::BAD_REQUEST);
        }
        Err(_) => return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
    };
    let publication_task_uid = page.records.iter().find_map(|record| {
        (record.phase == TaskPhase::Succeeded
            && record.finalized
            && record.task_origin == TaskOrigin::Browser
            && record
                .browser_task_evidence
                .as_ref()
                .is_some_and(|evidence| evidence.source == "inline"))
        .then_some(record.task_uid)
    });
    let runs = page
        .records
        .into_iter()
        .map(browser_run_view)
        .collect::<Vec<_>>();

    let saved = match state
        .ledger
        .browser_task_version_by_digest(
            owner_user_id,
            &session.principal.member_roles,
            &content_digest,
        )
        .await
    {
        Ok(record) => record,
        Err(_) => return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
    };
    let executed = match state
        .ledger
        .browser_task_by_digest(owner_user_id, &content_digest)
        .await
    {
        Ok(record) => record,
        Err(_) => return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
    }
    .and_then(|record| {
        let readable = record
            .browser_task_evidence
            .as_ref()
            .is_some_and(|evidence| {
                evidence.source == "inline" || executed_task_identity(&record).is_some()
            });
        if readable {
            Some(record)
        } else {
            eprintln!(
                "skipping unreadable browser Task run {} while resolving Task detail",
                record.task_uid
            );
            None
        }
    });
    let task = if let Some(record) = saved {
        let definition = match saved_task_definition(&record) {
            Ok(definition) => definition,
            Err(()) => return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
        };
        let package_path = match RelativePath::parse(record.package_path.clone()) {
            Ok(path) => path,
            Err(_) => return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
        };
        let Some(definition_source) = record.files.get(&record.package_path) else {
            return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE);
        };
        let (_, closure, computed_digest) = match crate::tasks::resolve_inline_package_closure(
            &package_path,
            &definition,
            definition_source.as_bytes(),
            &record.files,
        ) {
            Ok(resolved) => resolved,
            Err(_) => return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
        };
        if computed_digest.as_str() != record.content_digest {
            return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE);
        }
        let owned = record.owner_user_id == owner_user_id;
        let version_records = if owned {
            match state
                .ledger
                .browser_task_draft_versions(owner_user_id, record.task_id)
                .await
            {
                Ok(records) => records,
                Err(_) => return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
            }
        } else {
            vec![record.clone()]
        };
        let versions = match browser_task_version_views(version_records) {
            Ok(versions) => versions,
            Err(()) => return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
        };
        BrowserTaskView {
            task_id: Some(record.task_id),
            content_digest,
            source: "inline".to_owned(),
            revision: record.content_digest,
            path: record.package_path,
            name: definition.name.as_str().to_owned(),
            version: definition.version,
            runtime: definition.runtime,
            requires: definition.requires,
            files: record.files,
            closure: Some(closure),
            owned,
            editable: owned,
            shared_roles: record.shared_roles,
            versions,
            runs,
            next_cursor: page.next_cursor,
            publication_task_uid,
        }
    } else if let Some(record) = executed {
        let executed_identity = executed_task_identity(&record);
        let Some(evidence) = record.browser_task_evidence else {
            return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE);
        };
        let files = evidence.inline_files.clone().unwrap_or_default();
        let definition = if evidence.source == "inline" {
            let Some(definition_source) = files.get(evidence.path.as_str()) else {
                return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE);
            };
            match serde_json::from_str::<DirectTaskDefinition>(definition_source) {
                Ok(definition) if definition.validate().is_ok() => Some(definition),
                _ => return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
            }
        } else {
            None
        };
        let (name, version) = match definition.as_ref() {
            Some(definition) => (definition.name.as_str().to_owned(), definition.version),
            None => match executed_identity {
                Some(identity) => identity,
                None => return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
            },
        };
        let runtime = match definition.as_ref() {
            Some(definition) => definition.runtime.clone(),
            None => match AgentRef::parse(record.coding_agent_runtime.clone()) {
                Ok(agent_ref) => RuntimeSelection {
                    agent_ref,
                    model: None,
                },
                Err(_) => return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
            },
        };
        BrowserTaskView {
            task_id: None,
            content_digest,
            source: evidence.source,
            revision: evidence.revision,
            path: evidence.path.as_str().to_owned(),
            name,
            version,
            runtime,
            requires: definition.and_then(|definition| definition.requires),
            files,
            closure: evidence.closure,
            owned: true,
            editable: false,
            shared_roles: Vec::new(),
            versions: Vec::new(),
            runs,
            next_cursor: page.next_cursor,
            publication_task_uid: publication_task_uid.or_else(|| {
                (record.phase == TaskPhase::Succeeded && record.finalized)
                    .then_some(record.task_uid)
            }),
        }
    } else {
        let workflow_digest = content_digest
            .strip_prefix("steward:")
            .unwrap_or(&content_digest);
        let workflow = match state
            .ledger
            .workflow_revision_by_digest(workflow_digest)
            .await
        {
            Ok(Some(workflow)) => workflow,
            Ok(None) => return StatusCode::NOT_FOUND.into_response(),
            Err(_) => return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
        };
        let agent_ref = match AgentRef::parse(workflow.agent.clone()) {
            Ok(agent_ref) => agent_ref,
            Err(_) => return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
        };
        BrowserTaskView {
            task_id: None,
            content_digest: workflow.content_digest.clone(),
            source: format!("steward:registry/{}", workflow.name),
            revision: format!("steward:version:{}", workflow.version),
            path: "task-definition.json".to_owned(),
            name: workflow.name,
            version: match u64::try_from(workflow.version) {
                Ok(version) => version,
                Err(_) => return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
            },
            runtime: RuntimeSelection {
                agent_ref,
                model: None,
            },
            requires: None,
            files: BTreeMap::from([("prompt.md".to_owned(), workflow.prompt)]),
            closure: None,
            owned: false,
            editable: false,
            shared_roles: Vec::new(),
            versions: Vec::new(),
            runs,
            next_cursor: page.next_cursor,
            publication_task_uid: None,
        }
    };
    Json(BrowserTaskResponse {
        api_version: BROWSER_TASKS_API_VERSION,
        task,
    })
    .into_response()
}

fn valid_task_content_digest(value: &str) -> bool {
    ContentDigest::parse(value.to_owned()).is_ok()
        || value.strip_prefix("sha256:").is_some_and(|hex| {
            hex.len() == 64
                && hex
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
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
    operation_id = "myTasks",
    path = "/app/api/v1/tasks",
    responses(
        (status = 200, body = BrowserTasksResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 503, description = "Task library is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn my_tasks<L>(
    session: Option<Extension<BrowserSessionContext>>,
    State(state): State<BrowserRunsState<L>>,
) -> Response
where
    L: AgentRunLedger,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let owner_user_id = session.principal.canonical_user_id.as_str();
    let saved = match state
        .ledger
        .browser_task_versions(owner_user_id, &session.principal.member_roles)
        .await
    {
        Ok(saved) => saved,
        Err(_) => return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
    };
    let executed = match state.ledger.browser_tasks_for_owner(owner_user_id).await {
        Ok(executed) => executed,
        Err(_) => return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
    };
    let workflows = match state.ledger.list_latest_workflows().await {
        Ok(workflows) => workflows,
        Err(_) => return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
    };

    let mut tasks = BTreeMap::<String, BrowserTaskListItem>::new();
    for record in saved {
        let item = match saved_task_list_item(&record, owner_user_id) {
            Ok(item) => item,
            Err(()) => return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
        };
        tasks.insert(item.content_digest.clone(), item);
    }
    for record in executed {
        let Some(item) = executed_task_list_item(&record) else {
            eprintln!(
                "skipping unreadable browser Task run {} while listing Task library",
                record.task_uid
            );
            continue;
        };
        tasks.entry(item.content_digest.clone()).or_insert(item);
    }
    for workflow in workflows {
        let Some(item) = workflow_task_list_item(workflow) else {
            return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE);
        };
        tasks.entry(item.content_digest.clone()).or_insert(item);
    }
    let mut tasks = tasks.into_values().collect::<Vec<_>>();
    tasks.sort_by(|left, right| {
        right
            .updated_at
            .cmp(&left.updated_at)
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| right.version.cmp(&left.version))
    });
    Json(BrowserTasksResponse {
        api_version: BROWSER_TASKS_API_VERSION,
        tasks,
    })
    .into_response()
}

#[utoipa::path(
    post,
    operation_id = "saveMyTask",
    path = "/app/api/v1/tasks",
    request_body = SaveBrowserTaskRequest,
    responses(
        (status = 200, body = SaveBrowserTaskResponse),
        (status = 201, body = SaveBrowserTaskResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "CSRF proof is invalid or a sharing role is unauthorized"),
        (status = 404, description = "The owner-scoped Task draft does not exist"),
        (status = 409, description = "The Task version is not the next immutable version"),
        (status = 422, description = "The Task package is invalid"),
        (status = 503, description = "Task library is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn save_my_task<L>(
    session: Option<Extension<BrowserSessionContext>>,
    proof: Option<Extension<BrowserMutationProof>>,
    State(state): State<BrowserRunsState<L>>,
    Json(mut body): Json<SaveBrowserTaskRequest>,
) -> Response
where
    L: AgentRunLedger,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if proof.is_none() {
        return StatusCode::FORBIDDEN.into_response();
    }
    body.shared_roles.sort();
    if body.shared_roles.len() > 32
        || body
            .shared_roles
            .windows(2)
            .any(|roles| roles[0] == roles[1])
        || body
            .shared_roles
            .iter()
            .any(|role| !session.principal.member_roles.contains(role))
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(definition_source) = body.files.get(body.path.as_str()) else {
        return browser_runs_error(StatusCode::UNPROCESSABLE_ENTITY);
    };
    let definition = match serde_json::from_str::<DirectTaskDefinition>(definition_source) {
        Ok(definition) if definition.validate().is_ok() => definition,
        _ => return browser_runs_error(StatusCode::UNPROCESSABLE_ENTITY),
    };
    if body.task_id.is_none() && definition.version != 1 {
        return browser_runs_error(StatusCode::UNPROCESSABLE_ENTITY);
    }
    let (_, _, content_digest) = match crate::tasks::resolve_inline_package_closure(
        &body.path,
        &definition,
        definition_source.as_bytes(),
        &body.files,
    ) {
        Ok(resolved) => resolved,
        Err(_) => return browser_runs_error(StatusCode::UNPROCESSABLE_ENTITY),
    };
    let version = match i64::try_from(definition.version) {
        Ok(version) if version > 0 => version,
        _ => return browser_runs_error(StatusCode::UNPROCESSABLE_ENTITY),
    };
    let creating = body.task_id.is_none();
    let task_id = body.task_id.unwrap_or_else(Uuid::new_v4);
    let owner_user_id = session.principal.canonical_user_id.as_str();
    let record = match state
        .ledger
        .save_browser_task_version(BrowserTaskVersionPublication {
            task_id,
            owner_user_id,
            name: definition.name.as_str(),
            shared_roles: &body.shared_roles,
            version,
            content_digest: content_digest.as_str(),
            package_path: body.path.as_str(),
            files: &body.files,
        })
        .await
    {
        Ok(record) => record,
        Err(StoreError::TaskNotFound) => return StatusCode::NOT_FOUND.into_response(),
        Err(StoreError::TaskIdempotencyConflict) => {
            return browser_runs_error(StatusCode::CONFLICT);
        }
        Err(StoreError::InvalidTaskTransition) => {
            return browser_runs_error(StatusCode::UNPROCESSABLE_ENTITY);
        }
        Err(_) => return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
    };
    let task = match saved_task_list_item(&record, owner_user_id) {
        Ok(task) => task,
        Err(()) => return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
    };
    (
        if creating {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        Json(SaveBrowserTaskResponse {
            api_version: BROWSER_TASKS_API_VERSION,
            task,
        }),
    )
        .into_response()
}

fn saved_task_definition(record: &BrowserTaskVersionRecord) -> Result<DirectTaskDefinition, ()> {
    let source = record.files.get(&record.package_path).ok_or(())?;
    let definition = serde_json::from_str::<DirectTaskDefinition>(source).map_err(|_| ())?;
    definition.validate().map_err(|_| ())?;
    if definition.name.as_str() != record.name
        || i64::try_from(definition.version).ok() != Some(record.version)
    {
        return Err(());
    }
    Ok(definition)
}

fn saved_task_list_item(
    record: &BrowserTaskVersionRecord,
    viewer_user_id: &str,
) -> Result<BrowserTaskListItem, ()> {
    let definition = saved_task_definition(record)?;
    Ok(BrowserTaskListItem {
        task_id: Some(record.task_id),
        content_digest: record.content_digest.clone(),
        source: "inline".to_owned(),
        revision: record.content_digest.clone(),
        path: record.package_path.clone(),
        name: definition.name.as_str().to_owned(),
        version: definition.version,
        owned: record.owner_user_id == viewer_user_id,
        editable: record.owner_user_id == viewer_user_id,
        shared_roles: record.shared_roles.clone(),
        updated_at: Some(record.updated_at.clone()),
    })
}

fn browser_task_version_views(
    records: Vec<BrowserTaskVersionRecord>,
) -> Result<Vec<BrowserTaskVersionView>, ()> {
    records
        .into_iter()
        .map(|record| {
            Ok(BrowserTaskVersionView {
                version: u64::try_from(record.version).map_err(|_| ())?,
                content_digest: record.content_digest,
                created_at: record.created_at,
            })
        })
        .collect()
}

fn direct_workflow_identity(workflow: &str) -> Option<(String, u64)> {
    let (name, version) = workflow.strip_prefix("direct:")?.rsplit_once('@')?;
    let version = version.parse::<u64>().ok().filter(|version| *version > 0)?;
    Some((name.to_owned(), version))
}

fn executed_task_identity(record: &AgentRunRecord) -> Option<(String, u64)> {
    let evidence = record.browser_task_evidence.as_ref()?;
    if let Some(source_name) = evidence.source.strip_prefix("steward:registry/") {
        let name = record.workflow_name.as_deref()?;
        let version = u64::try_from(record.workflow_version?)
            .ok()
            .filter(|version| *version > 0)?;
        if source_name != name || evidence.revision != format!("steward:version:{version}") {
            return None;
        }
        return Some((name.to_owned(), version));
    }
    direct_workflow_identity(&record.workflow)
}

fn executed_task_list_item(record: &AgentRunRecord) -> Option<BrowserTaskListItem> {
    let evidence = record.browser_task_evidence.as_ref()?;
    let (name, version) = executed_task_identity(record)?;
    Some(BrowserTaskListItem {
        task_id: None,
        content_digest: evidence.closure_digest.as_str().to_owned(),
        source: evidence.source.clone(),
        revision: evidence.revision.clone(),
        path: evidence.path.as_str().to_owned(),
        name,
        version,
        owned: true,
        editable: false,
        shared_roles: Vec::new(),
        updated_at: Some(record.updated_at.clone()),
    })
}

fn workflow_task_list_item(workflow: WorkflowRevisionRecord) -> Option<BrowserTaskListItem> {
    Some(BrowserTaskListItem {
        task_id: None,
        content_digest: workflow.content_digest,
        source: format!("steward:registry/{}", workflow.name),
        revision: format!("steward:version:{}", workflow.version),
        path: "task-definition.json".to_owned(),
        name: workflow.name,
        version: u64::try_from(workflow.version)
            .ok()
            .filter(|version| *version > 0)?,
        owned: false,
        editable: false,
        shared_roles: Vec::new(),
        updated_at: Some(workflow.published_at),
    })
}

#[utoipa::path(
    get,
    path = "/app/api/v1/runs/{task_uid}/outputs",
    params(("task_uid" = String, Path, format = "uuid")),
    responses(
        (status = 200, body = BrowserRunOutputsResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 404, description = "Completed run output is unavailable"),
        (status = 409, description = "Run outputs are pending finalization"),
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
    match scoped_run(
        &state.ledger,
        task_uid,
        Some(session.principal.canonical_user_id.as_str().to_owned()),
    )
    .await
    {
        Ok(Some(record)) if !record.finalized => return outputs_pending(),
        Ok(Some(_)) => {}
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
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
        (status = 409, description = "Run outputs are pending finalization"),
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
    match scoped_run(
        &state.ledger,
        task_uid,
        Some(session.principal.canonical_user_id.as_str().to_owned()),
    )
    .await
    {
        Ok(Some(record)) if !record.finalized => return outputs_pending(),
        Ok(Some(_)) => {}
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
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
        archive.content[entry.offset..entry.offset + entry.size].to_vec(),
    )
        .into_response()
}

fn outputs_pending() -> Response {
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({ "error": "outputs_pending" })),
    )
        .into_response()
}

fn output_archive_entries(
    archive: &AgentRunOutputArchive,
) -> Result<Vec<TaskOutputArchiveEntry>, ()> {
    let compatibility = match archive.contract.as_deref() {
        Some(TASK_OUTPUT_ARCHIVE_CONTRACT) => TaskOutputArchiveCompatibility::Strict,
        None => TaskOutputArchiveCompatibility::HistoricalMixedDiagnostics,
        Some(_) => return Err(()),
    };
    task_output_archive_entries(&archive.content, compatibility).map_err(|_| ())
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
            event_streams: Arc::new(BrowserRunEventStreams::default()),
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
        package_digest: None,
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
        package_digest: None,
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
        (status = 429, description = "Per-user event-stream limit reached"),
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
    let owner_user_id = session.principal.canonical_user_id.as_str().to_owned();
    let initial = match browser_run_event_snapshot(&state.ledger, task_uid, &owner_user_id).await {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
    };
    let permit = match state.event_streams.acquire(&owner_user_id, task_uid) {
        Ok(permit) => permit,
        Err(()) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(serde_json::json!({ "error": "run_event_stream_limit" })),
            )
                .into_response();
        }
    };
    let last_event_id = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    if state.event_streams.observe(task_uid, initial).is_err() {
        return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE);
    }
    let initial_events = match state.event_streams.resume(task_uid, last_event_id) {
        Ok(events) => events,
        Err(()) => return browser_runs_error(StatusCode::SERVICE_UNAVAILABLE),
    };
    let ledger = state.ledger.clone();
    let event_streams = state.event_streams.clone();
    let stream = async_stream::stream! {
        let _permit = permit;
        let mut cursor = last_event_id;
        let mut terminal = event_streams.latest_is_terminal(task_uid).unwrap_or(false);
        for event in initial_events {
            cursor = event.id;
            terminal = event.terminal;
            yield Ok::<Event, Infallible>(browser_run_sse_event(event));
        }
        if !terminal {
            loop {
                tokio::time::sleep(BROWSER_RUN_EVENT_POLL_INTERVAL).await;
                let snapshot = match browser_run_event_snapshot(&ledger, task_uid, &owner_user_id).await {
                    Ok(Some(snapshot)) => snapshot,
                    Ok(None) | Err(_) => break,
                };
                if event_streams.observe(task_uid, snapshot).is_err() {
                    break;
                }
                let events = match event_streams.resume(task_uid, cursor) {
                    Ok(events) => events,
                    Err(()) => break,
                };
                for event in events {
                    cursor = event.id;
                    terminal = event.terminal;
                    yield Ok::<Event, Infallible>(browser_run_sse_event(event));
                }
                if terminal {
                    break;
                }
            }
        }
    };
    let mut response = Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(BROWSER_RUN_EVENT_HEARTBEAT_INTERVAL)
                .text("heartbeat"),
        )
        .into_response();
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

fn browser_run_sse_event(event: BufferedBrowserRunEvent) -> Event {
    Event::default()
        .id(event.id.to_string())
        .event(event.kind)
        .data(event.data)
}

async fn browser_run_event_snapshot<L>(
    ledger: &L,
    task_uid: Uuid,
    owner_user_id: &str,
) -> Result<Option<BrowserRunEventSnapshot>, StoreError>
where
    L: AgentRunLedger,
{
    let Some(record) = scoped_run(ledger, task_uid, Some(owner_user_id.to_owned())).await? else {
        return Ok(None);
    };
    let Some(events) = ledger.agent_run_timeline(task_uid).await? else {
        return Ok(None);
    };
    let events = events
        .into_iter()
        .rev()
        .map(browser_timeline_event)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(BrowserRunEventSnapshot {
        api_version: BROWSER_AGENT_RUNS_API_VERSION,
        event_id: 0,
        run: browser_run_view(record),
        timeline: BrowserRunTimelineResponse {
            api_version: BROWSER_AGENT_RUNS_API_VERSION,
            task_uid,
            events,
        },
    }))
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
                | ConnectionBrokerError::BridgeContractInvalid
                | ConnectionBrokerError::IdempotencyConflict
                | ConnectionBrokerError::ProxyPolicyDenied
                | ConnectionBrokerError::ProviderAuthorizationFailed
                | ConnectionBrokerError::TokenGrantFailed
                | ConnectionBrokerError::ProviderResponseInvalid
                | ConnectionBrokerError::BridgeResultTooLarge
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
        Err(BrowserTaskRerunError::InferenceKeyMissing) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "inference_key_missing",
                "failureReason": "Add an inference key under Connections > Inference / LLMs, then retry the run.",
            })),
        )
            .into_response(),
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
            package_digest: None,
        })
        .await?;
    Ok(page.records.into_iter().next())
}

fn browser_run_view(record: AgentRunRecord) -> BrowserRunView {
    let phase = if record.runtime_uid.is_some()
        && matches!(
            record.phase,
            TaskPhase::Submitted | TaskPhase::Parked | TaskPhase::Queued
        ) {
        TaskPhase::Running
    } else {
        record.phase
    };
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
        phase,
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
            prompt_source: evidence.prompt_source,
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
                    prompt_source: evidence.prompt_source,
                })
        })
        .or_else(|| {
            Some(BrowserRunPackageView {
                source: format!("steward:registry/{}", record.workflow_name.as_deref()?),
                revision: format!("steward:version:{}", record.workflow_version?),
                path: "task-definition.json".to_owned(),
                content_digest: record.workflow_digest.clone(),
                prompt_source: PromptSourceKind::Path,
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
        phase,
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

#[cfg(test)]
fn agent_run_content_digest(record: &AgentRunRecord) -> Option<&str> {
    record
        .browser_task_evidence
        .as_ref()
        .map(|evidence| evidence.closure_digest.as_str())
        .or_else(|| {
            record
                .direct_task_evidence
                .as_ref()
                .map(|evidence| evidence.closure_digest.as_str())
        })
        .or(record.workflow_digest.as_deref())
}

const fn is_terminal_phase(phase: TaskPhase) -> bool {
    matches!(
        phase,
        TaskPhase::Succeeded | TaskPhase::Failed | TaskPhase::Cancelled
    )
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
    use steward_store::{AgentRunSpend, AgentRunTimelineEvent, TaskRecord, WorkflowRevisionRecord};
    use steward_types::direct_package::{
        BrowserTaskEvidence, ContentDigest, PromptSourceKind, RelativePath,
    };
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
        outputs: Arc<Mutex<HashMap<Uuid, AgentRunOutputArchive>>>,
        rerun_sources: Arc<Mutex<HashMap<Uuid, TaskRecord>>>,
        github_matches: Arc<Mutex<VecDeque<Option<TaskRecord>>>>,
        github_queries: Arc<Mutex<Vec<GithubRerunQuery>>>,
        workflow_revisions: Arc<Mutex<Vec<WorkflowRevisionRecord>>>,
        task_versions: Arc<Mutex<Vec<BrowserTaskVersionRecord>>>,
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
                                && query.package_digest.as_deref().is_none_or(|digest| {
                                    agent_run_content_digest(record) == Some(digest)
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

        fn inline_browser_task_by_digest<'a>(
            &'a self,
            owner_user_id: &'a str,
            package_digest: &'a str,
        ) -> BoxFuture<'a, Result<Option<AgentRunRecord>, StoreError>> {
            Box::pin(async move {
                let mut records = self
                    .records
                    .lock()
                    .map_err(|_| StoreError::InvalidRunQuery)?
                    .iter()
                    .filter(|record| {
                        record.owner_user_id.as_deref() == Some(owner_user_id)
                            && record
                                .browser_task_evidence
                                .as_ref()
                                .is_some_and(|evidence| {
                                    evidence.source == "inline"
                                        && evidence.closure_digest.as_str() == package_digest
                                        && evidence.inline_files.is_some()
                                })
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                records.sort_by_key(|record| {
                    (
                        !(record.phase == TaskPhase::Succeeded && record.finalized),
                        std::cmp::Reverse(record.created_at.clone()),
                        std::cmp::Reverse(record.task_uid),
                    )
                });
                Ok(records.into_iter().next())
            })
        }

        fn workflow_revision_by_digest<'a>(
            &'a self,
            content_digest: &'a str,
        ) -> BoxFuture<'a, Result<Option<WorkflowRevisionRecord>, StoreError>> {
            Box::pin(async move {
                Ok(self
                    .workflow_revisions
                    .lock()
                    .map_err(|_| StoreError::InvalidWorkflow)?
                    .iter()
                    .find(|workflow| workflow.content_digest == content_digest)
                    .cloned())
            })
        }

        fn browser_tasks_for_owner<'a>(
            &'a self,
            owner_user_id: &'a str,
        ) -> BoxFuture<'a, Result<Vec<AgentRunRecord>, StoreError>> {
            Box::pin(async move {
                Ok(self
                    .records
                    .lock()
                    .map_err(|_| StoreError::InvalidRunQuery)?
                    .iter()
                    .filter(|record| {
                        record.owner_user_id.as_deref() == Some(owner_user_id)
                            && record.browser_task_evidence.is_some()
                    })
                    .cloned()
                    .collect())
            })
        }

        fn browser_task_by_digest<'a>(
            &'a self,
            owner_user_id: &'a str,
            package_digest: &'a str,
        ) -> BoxFuture<'a, Result<Option<AgentRunRecord>, StoreError>> {
            Box::pin(async move {
                Ok(self
                    .records
                    .lock()
                    .map_err(|_| StoreError::InvalidRunQuery)?
                    .iter()
                    .find(|record| {
                        record.owner_user_id.as_deref() == Some(owner_user_id)
                            && record
                                .browser_task_evidence
                                .as_ref()
                                .is_some_and(|evidence| {
                                    evidence.closure_digest.as_str() == package_digest
                                })
                    })
                    .cloned())
            })
        }

        fn browser_task_versions<'a>(
            &'a self,
            viewer_user_id: &'a str,
            viewer_roles: &'a [String],
        ) -> BoxFuture<'a, Result<Vec<BrowserTaskVersionRecord>, StoreError>> {
            Box::pin(async move {
                Ok(self
                    .task_versions
                    .lock()
                    .map_err(|_| StoreError::InvalidRunQuery)?
                    .iter()
                    .filter(|record| {
                        record.owner_user_id == viewer_user_id
                            || record
                                .shared_roles
                                .iter()
                                .any(|role| viewer_roles.contains(role))
                    })
                    .cloned()
                    .collect())
            })
        }

        fn browser_task_version_by_digest<'a>(
            &'a self,
            viewer_user_id: &'a str,
            viewer_roles: &'a [String],
            content_digest: &'a str,
        ) -> BoxFuture<'a, Result<Option<BrowserTaskVersionRecord>, StoreError>> {
            Box::pin(async move {
                Ok(self
                    .task_versions
                    .lock()
                    .map_err(|_| StoreError::InvalidRunQuery)?
                    .iter()
                    .find(|record| {
                        record.content_digest == content_digest
                            && (record.owner_user_id == viewer_user_id
                                || record
                                    .shared_roles
                                    .iter()
                                    .any(|role| viewer_roles.contains(role)))
                    })
                    .cloned())
            })
        }

        fn browser_task_draft_versions<'a>(
            &'a self,
            owner_user_id: &'a str,
            task_id: Uuid,
        ) -> BoxFuture<'a, Result<Vec<BrowserTaskVersionRecord>, StoreError>> {
            Box::pin(async move {
                Ok(self
                    .task_versions
                    .lock()
                    .map_err(|_| StoreError::InvalidRunQuery)?
                    .iter()
                    .filter(|record| {
                        record.task_id == task_id && record.owner_user_id == owner_user_id
                    })
                    .cloned()
                    .collect())
            })
        }

        fn save_browser_task_version<'a>(
            &'a self,
            publication: BrowserTaskVersionPublication<'a>,
        ) -> BoxFuture<'a, Result<BrowserTaskVersionRecord, StoreError>> {
            Box::pin(async move {
                let mut records = self
                    .task_versions
                    .lock()
                    .map_err(|_| StoreError::InvalidRunQuery)?;
                let current = records
                    .iter()
                    .filter(|record| record.task_id == publication.task_id)
                    .map(|record| record.version)
                    .max()
                    .unwrap_or(0);
                if publication.version != current + 1 {
                    return Err(StoreError::TaskIdempotencyConflict);
                }
                if current > 0
                    && records.iter().any(|record| {
                        record.task_id == publication.task_id
                            && (record.owner_user_id != publication.owner_user_id
                                || record.name != publication.name)
                    })
                {
                    return Err(StoreError::TaskNotFound);
                }
                let record = BrowserTaskVersionRecord {
                    task_id: publication.task_id,
                    owner_user_id: publication.owner_user_id.to_owned(),
                    name: publication.name.to_owned(),
                    shared_roles: publication.shared_roles.to_vec(),
                    version: publication.version,
                    content_digest: publication.content_digest.to_owned(),
                    package_path: publication.package_path.to_owned(),
                    files: publication.files.clone(),
                    created_at: format!("2026-01-01T00:00:0{}Z", publication.version),
                    updated_at: format!("2026-01-01T00:00:0{}Z", publication.version),
                };
                records.push(record.clone());
                Ok(record)
            })
        }

        fn list_latest_workflows(
            &self,
        ) -> BoxFuture<'_, Result<Vec<WorkflowRevisionRecord>, StoreError>> {
            Box::pin(async move {
                Ok(self
                    .workflow_revisions
                    .lock()
                    .map_err(|_| StoreError::InvalidWorkflow)?
                    .clone())
            })
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
        ) -> BoxFuture<'a, Result<Option<AgentRunOutputArchive>, StoreError>> {
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

    #[derive(Clone, Copy)]
    struct FailingBrowserTaskRerunner(BrowserTaskRerunError);

    impl BrowserTaskRerunner for FailingBrowserTaskRerunner {
        fn rerun<'a>(
            &'a self,
            _session: &'a BrowserSessionContext,
            _source: &'a TaskRecord,
            _idempotency_key: &'a str,
        ) -> BoxFuture<'a, Result<Uuid, BrowserTaskRerunError>> {
            Box::pin(async move { Err(self.0) })
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

    #[test]
    fn run_snapshot_promotes_a_bound_runtime_and_its_job_from_queued_to_running()
    -> Result<(), String> {
        let owner = "usr_0123456789abcdef0123456789abcdef";
        let task_uid = Uuid::parse_str("11111111-1111-4111-8111-111111111111")
            .map_err(|error| error.to_string())?;
        let mut record = run(task_uid, owner);
        record.phase = TaskPhase::Queued;
        record.runtime_uid = Some("runtime-bound".to_owned());

        let view = browser_run_view(record);
        assert_eq!(view.phase, TaskPhase::Running);
        assert_eq!(view.stages[1].id, BrowserRunStageId::ProvisionRuntime);
        assert_eq!(view.stages[1].state, BrowserRunStageState::Succeeded);
        assert_eq!(view.stages[2].id, BrowserRunStageId::AgentExecution);
        assert_eq!(view.stages[2].state, BrowserRunStageState::Running);
        Ok(())
    }

    fn run_event_snapshot(
        task_uid: Uuid,
        owner_user_id: &str,
        phase: TaskPhase,
        finalized: bool,
    ) -> BrowserRunEventSnapshot {
        let mut record = run(task_uid, owner_user_id);
        record.phase = phase;
        record.finalized = finalized;
        BrowserRunEventSnapshot {
            api_version: BROWSER_AGENT_RUNS_API_VERSION,
            event_id: 0,
            run: browser_run_view(record),
            timeline: BrowserRunTimelineResponse {
                api_version: BROWSER_AGENT_RUNS_API_VERSION,
                task_uid,
                events: Vec::new(),
            },
        }
    }

    #[tokio::test]
    async fn terminal_run_event_history_expires_after_delivery_and_stream_close()
    -> Result<(), String> {
        let owner = "usr_0123456789abcdef0123456789abcdef";
        let task_uid = Uuid::from_u128(1);
        let streams = Arc::new(BrowserRunEventStreams::default());
        let permit = streams
            .acquire(owner, task_uid)
            .map_err(|()| "acquire stream")?;
        streams
            .observe(
                task_uid,
                run_event_snapshot(task_uid, owner, TaskPhase::Succeeded, true),
            )
            .map_err(|()| "observe terminal event")?;
        let delivered = streams
            .resume(task_uid, 0)
            .map_err(|()| "deliver terminal event")?;
        assert_eq!(delivered.len(), 1);
        assert!(delivered[0].terminal);
        drop(permit);

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            !streams
                .state
                .lock()
                .map_err(|_| "lock stream state")?
                .history_by_task
                .contains_key(&task_uid),
            "terminal event history must be evicted after its resume grace"
        );
        Ok(())
    }

    #[tokio::test]
    async fn terminal_run_event_history_resumes_inside_the_grace_period() -> Result<(), String> {
        let owner = "usr_0123456789abcdef0123456789abcdef";
        let task_uid = Uuid::from_u128(2);
        let streams = Arc::new(BrowserRunEventStreams::default());
        let permit = streams
            .acquire(owner, task_uid)
            .map_err(|()| "acquire stream")?;
        streams
            .observe(
                task_uid,
                run_event_snapshot(task_uid, owner, TaskPhase::Running, false),
            )
            .map_err(|()| "observe running event")?;
        let running_event_id = streams
            .resume(task_uid, 0)
            .map_err(|()| "deliver running event")?
            .into_iter()
            .next()
            .ok_or_else(|| "running event missing".to_owned())?
            .id;
        streams
            .observe(
                task_uid,
                run_event_snapshot(task_uid, owner, TaskPhase::Succeeded, true),
            )
            .map_err(|()| "observe terminal event")?;
        let terminal = streams
            .resume(task_uid, running_event_id)
            .map_err(|()| "deliver terminal event")?;
        assert_eq!(terminal.len(), 1);
        assert!(terminal[0].terminal);
        drop(permit);

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let resumed = streams
            .resume(task_uid, running_event_id)
            .map_err(|()| "resume inside grace")?;
        assert_eq!(resumed.len(), 1);
        assert!(resumed[0].terminal);
        Ok(())
    }

    #[test]
    fn retained_run_event_tasks_never_exceed_the_global_bound() -> Result<(), String> {
        let owner = "usr_0123456789abcdef0123456789abcdef";
        let streams = BrowserRunEventStreams::default();
        for index in 1..=257_u128 {
            let task_uid = Uuid::from_u128(index);
            streams
                .observe(
                    task_uid,
                    run_event_snapshot(task_uid, owner, TaskPhase::Running, false),
                )
                .map_err(|()| "observe run event")?;
        }
        assert!(
            streams
                .state
                .lock()
                .map_err(|_| "lock stream state")?
                .history_by_task
                .len()
                <= 256,
            "run event history must retain no more than 256 tasks"
        );
        Ok(())
    }

    #[tokio::test]
    async fn run_events_hold_one_stream_resume_and_close_after_terminal() -> Result<(), String> {
        let owner = "usr_0123456789abcdef0123456789abcdef";
        let other_owner = "usr_abcdefabcdefabcdefabcdefabcdefab";
        let own_task = Uuid::parse_str("11111111-1111-4111-8111-111111111111")
            .map_err(|error| error.to_string())?;
        let other_task = Uuid::parse_str("22222222-2222-4222-8222-222222222222")
            .map_err(|error| error.to_string())?;
        let ledger = FakeLedger::default();
        let mut own_run = run(own_task, owner);
        own_run.phase = TaskPhase::Running;
        own_run.finalized = false;
        let mut other_run = run(other_task, other_owner);
        other_run.finalized = true;
        ledger
            .records
            .lock()
            .map_err(|_| "lock records")?
            .extend([own_run, other_run]);
        let (service, session_cookie) = signed_in_cookie(LocalFakeIdentity::User).await?;
        let app = protected_router(ledger.clone(), service);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/app/api/v1/runs/{own_task}/events"))
                    .header(header::COOKIE, &session_cookie)
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
        let read = tokio::spawn(to_bytes(response.into_body(), 64 * 1024));
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(
            !read.is_finished(),
            "a non-terminal run must keep one event stream open"
        );
        {
            let mut records = ledger.records.lock().map_err(|_| "lock records")?;
            let record = records
                .iter_mut()
                .find(|record| record.task_uid == own_task)
                .ok_or_else(|| "own run disappeared".to_owned())?;
            record.phase = TaskPhase::Succeeded;
            record.finalized = true;
            record.updated_at = "2026-08-17T00:01:00.000000Z".to_owned();
        }
        let body = tokio::time::timeout(std::time::Duration::from_secs(2), read)
            .await
            .map_err(|_| "terminal event stream did not close".to_owned())?
            .map_err(|error| format!("join run event reader: {error}"))?
            .map_err(|error| format!("read run event response: {error}"))?;
        let body = std::str::from_utf8(&body)
            .map_err(|error| format!("run event response was not UTF-8: {error}"))?;
        assert!(body.contains("event: snapshot"));
        assert!(body.contains("event: run"));
        assert!(body.contains(": heartbeat"));
        assert!(body.contains(&format!("\"taskUid\":\"{own_task}\"")));
        assert!(body.contains("\"phase\":\"running\""));
        assert!(body.contains("\"phase\":\"succeeded\""));
        let first_event_id = body
            .lines()
            .find_map(|line| line.strip_prefix("id: "))
            .ok_or_else(|| "snapshot omitted event id".to_owned())?
            .parse::<u64>()
            .map_err(|error| format!("parse event id: {error}"))?;

        let resumed = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/app/api/v1/runs/{own_task}/events"))
                    .header(header::COOKIE, &session_cookie)
                    .header("last-event-id", first_event_id.to_string())
                    .body(Body::empty())
                    .map_err(|error| format!("build resumed run event request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute resumed run event request: {error}"))?;
        assert_eq!(resumed.status(), StatusCode::OK);
        let resumed = to_bytes(resumed.into_body(), 64 * 1024)
            .await
            .map_err(|error| format!("read resumed run event response: {error}"))?;
        let resumed = std::str::from_utf8(&resumed)
            .map_err(|error| format!("resumed event response was not UTF-8: {error}"))?;
        assert!(resumed.contains("event: run"));
        assert!(!resumed.contains("event: snapshot"));

        let terminal_event_id = resumed
            .lines()
            .find_map(|line| line.strip_prefix("id: "))
            .ok_or_else(|| "resumed terminal event omitted event id".to_owned())?;
        let already_current = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/app/api/v1/runs/{own_task}/events"))
                    .header(header::COOKIE, &session_cookie)
                    .header("last-event-id", terminal_event_id)
                    .body(Body::empty())
                    .map_err(|error| format!("build current terminal event request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute current terminal event request: {error}"))?;
        assert_eq!(already_current.status(), StatusCode::OK);
        let already_current = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            to_bytes(already_current.into_body(), 4096),
        )
        .await
        .map_err(|_| "current terminal event stream did not close".to_owned())?
        .map_err(|error| format!("read current terminal event response: {error}"))?;
        assert!(already_current.is_empty());

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
    async fn run_events_enforce_a_per_user_concurrent_stream_cap() -> Result<(), String> {
        let owner = "usr_0123456789abcdef0123456789abcdef";
        let ledger = FakeLedger::default();
        for index in 1..=5 {
            let task_uid = Uuid::from_u128(index);
            let mut record = run(task_uid, owner);
            record.phase = TaskPhase::Running;
            record.finalized = false;
            ledger
                .records
                .lock()
                .map_err(|_| "lock records")?
                .push(record);
        }
        let (service, session_cookie) = signed_in_cookie(LocalFakeIdentity::User).await?;
        let app = protected_router(ledger, service);
        let mut held = Vec::new();
        for index in 1..=4 {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!(
                            "/app/api/v1/runs/{}/events",
                            Uuid::from_u128(index)
                        ))
                        .header(header::COOKIE, &session_cookie)
                        .body(Body::empty())
                        .map_err(|error| format!("build capped event request: {error}"))?,
                )
                .await
                .map_err(|error| format!("execute capped event request: {error}"))?;
            assert_eq!(response.status(), StatusCode::OK);
            held.push(response);
        }
        let rejected = app
            .oneshot(
                Request::builder()
                    .uri(format!("/app/api/v1/runs/{}/events", Uuid::from_u128(5)))
                    .header(header::COOKIE, session_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build over-cap event request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute over-cap event request: {error}"))?;
        assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS);
        drop(held);
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
        let pending_task = Uuid::parse_str("33333333-3333-4333-8333-333333333333")
            .map_err(|error| error.to_string())?;
        let mut own_run = run(own_task, owner);
        own_run.finalized = true;
        let mut other_run = run(other_task, other_owner);
        other_run.finalized = true;
        let mut pending_run = run(pending_task, owner);
        pending_run.phase = TaskPhase::Running;
        pending_run.finalized = false;
        ledger.records.lock().map_err(|_| "lock records")?.extend([
            own_run,
            other_run,
            pending_run,
        ]);
        ledger.outputs.lock().map_err(|_| "lock outputs")?.extend([
            (
                own_task,
                AgentRunOutputArchive {
                    content: output_tar("out/report.txt", b"complete\n"),
                    contract: Some(TASK_OUTPUT_ARCHIVE_CONTRACT.to_owned()),
                },
            ),
            (
                other_task,
                AgentRunOutputArchive {
                    content: output_tar("out/secret.txt", b"hidden\n"),
                    contract: Some(TASK_OUTPUT_ARCHIVE_CONTRACT.to_owned()),
                },
            ),
        ]);

        let (service, session_cookie) = signed_in_cookie(LocalFakeIdentity::User).await?;
        let app = protected_router(ledger.clone(), service);
        let pending = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/app/api/v1/runs/{pending_task}/outputs"))
                    .header(header::COOKIE, &session_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build pending output request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute pending output request: {error}"))?;
        assert_eq!(pending.status(), StatusCode::CONFLICT);
        let pending = to_bytes(pending.into_body(), 4096)
            .await
            .map_err(|error| format!("read pending output response: {error}"))?;
        let pending: serde_json::Value = serde_json::from_slice(&pending)
            .map_err(|error| format!("decode pending output response: {error}"))?;
        assert_eq!(pending["error"], "outputs_pending");

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
            prompt_source: PromptSourceKind::Path,
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
    async fn task_page_is_digest_scoped_and_includes_exact_owned_runs() -> Result<(), String> {
        let owner = "usr_0123456789abcdef0123456789abcdef";
        let other_owner = "usr_abcdefabcdefabcdefabcdefabcdefab";
        let succeeded_task = Uuid::parse_str("33333333-3333-4333-8333-333333333333")
            .map_err(|error| error.to_string())?;
        let failed_task = Uuid::parse_str("55555555-5555-4555-8555-555555555555")
            .map_err(|error| error.to_string())?;
        let hidden_task = Uuid::parse_str("44444444-4444-4444-8444-444444444444")
            .map_err(|error| error.to_string())?;
        let digest = format!("steward:sha256:{}", "d".repeat(64));
        let evidence = BrowserTaskEvidence {
            source: "inline".to_owned(),
            revision: digest.clone(),
            path: RelativePath::parse("task-definition.json")?,
            closure: None,
            closure_digest: ContentDigest::parse(digest.clone())?,
            inline_files: Some(BTreeMap::from([
                ("prompt.md".to_owned(), "Say hello.\n".to_owned()),
                (
                    "task-definition.json".to_owned(),
                    serde_json::json!({
                        "schemaVersion": "steward.task-definition/v2",
                        "name": "hello-task",
                        "version": 3,
                        "runtime": {"agentRef": "example-agent@1.0.0"},
                        "prompt": "prompt.md",
                        "outputs": [{"path": "out", "kind": "directory", "required": true}]
                    })
                    .to_string(),
                ),
            ])),
            diagnostics: Default::default(),
            prompt_source: PromptSourceKind::Path,
        };
        let mut succeeded = run(succeeded_task, owner);
        succeeded.task_origin = TaskOrigin::Browser;
        succeeded.finalized = true;
        succeeded.browser_task_evidence = Some(evidence.clone());
        let mut failed = run(failed_task, owner);
        failed.phase = TaskPhase::Failed;
        failed.task_origin = TaskOrigin::Browser;
        failed.browser_task_evidence = Some(evidence.clone());
        let mut hidden = run(hidden_task, other_owner);
        hidden.task_origin = TaskOrigin::Browser;
        hidden.browser_task_evidence = Some(evidence);
        let ledger = FakeLedger::default();
        ledger
            .records
            .lock()
            .map_err(|_| "lock records")?
            .extend([succeeded, failed, hidden]);

        let (service, session_cookie) = signed_in_cookie(LocalFakeIdentity::User).await?;
        let response = protected_router(ledger, service)
            .oneshot(
                Request::builder()
                    .uri(format!("/app/api/v1/tasks/{digest}"))
                    .header(header::COOKIE, session_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build Task request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute Task request: {error}"))?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 16 * 1024)
            .await
            .map_err(|error| format!("read Task response: {error}"))?;
        let body: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("decode Task response: {error}"))?;
        assert_eq!(body["task"]["contentDigest"], digest);
        assert_eq!(body["task"]["name"], "hello-task");
        assert_eq!(body["task"]["version"], 3);
        assert_eq!(body["task"]["runtime"]["agentRef"], "example-agent@1.0.0");
        assert_eq!(body["task"]["files"]["prompt.md"], "Say hello.\n");
        assert_eq!(
            body["task"]["publicationTaskUid"],
            succeeded_task.to_string()
        );
        assert_eq!(body["task"]["runs"].as_array().map(Vec::len), Some(2));
        assert!(body["task"]["runs"].as_array().is_some_and(|runs| {
            runs.iter()
                .all(|run| run["taskUid"] != hidden_task.to_string())
        }));
        Ok(())
    }

    #[tokio::test]
    async fn task_page_hides_another_users_inline_task_but_exposes_a_published_workflow()
    -> Result<(), String> {
        let other_owner = "usr_abcdefabcdefabcdefabcdefabcdefab";
        let hidden_digest = format!("steward:sha256:{}", "f".repeat(64));
        let hidden_task = Uuid::parse_str("44444444-4444-4444-8444-444444444444")
            .map_err(|error| error.to_string())?;
        let mut hidden = run(hidden_task, other_owner);
        hidden.task_origin = TaskOrigin::Browser;
        hidden.browser_task_evidence = Some(BrowserTaskEvidence {
            source: "inline".to_owned(),
            revision: hidden_digest.clone(),
            path: RelativePath::parse("task-definition.json")?,
            closure: None,
            closure_digest: ContentDigest::parse(hidden_digest.clone())?,
            inline_files: Some(BTreeMap::from([(
                "task-definition.json".to_owned(),
                "{}".to_owned(),
            )])),
            diagnostics: Default::default(),
            prompt_source: PromptSourceKind::Path,
        });
        let workflow_digest = format!("sha256:{}", "e".repeat(64));
        let workflow = WorkflowRevisionRecord {
            name: "release-summary".to_owned(),
            version: 2,
            display_name: "Release summary".to_owned(),
            agent: "example-agent@1.0.0".to_owned(),
            prompt: "Summarize the release.\n".to_owned(),
            content_digest: workflow_digest.clone(),
            published_by: "usr_admin0000000000000000000000000".to_owned(),
            published_at: "2026-01-02T03:04:05Z".to_owned(),
        };
        let registry_digest = format!("steward:{workflow_digest}");
        let registry_task = Uuid::parse_str("66666666-6666-4666-8666-666666666666")
            .map_err(|error| error.to_string())?;
        let mut registry_run = run(registry_task, "usr_0123456789abcdef0123456789abcdef");
        registry_run.task_origin = TaskOrigin::Browser;
        registry_run.workflow = "release-summary@2".to_owned();
        registry_run.workflow_name = Some("release-summary".to_owned());
        registry_run.workflow_version = Some(2);
        registry_run.workflow_digest = Some(workflow_digest.clone());
        registry_run.coding_agent_runtime = "example-agent@1.0.0".to_owned();
        registry_run.browser_task_evidence = Some(BrowserTaskEvidence {
            source: "steward:registry/release-summary".to_owned(),
            revision: "steward:version:2".to_owned(),
            path: RelativePath::parse("task-definition.json")?,
            closure: None,
            closure_digest: ContentDigest::parse(registry_digest.clone())?,
            inline_files: None,
            diagnostics: Default::default(),
            prompt_source: PromptSourceKind::Path,
        });
        let malformed_task = Uuid::parse_str("77777777-7777-4777-8777-777777777777")
            .map_err(|error| error.to_string())?;
        let mut malformed_run = run(malformed_task, "usr_0123456789abcdef0123456789abcdef");
        malformed_run.task_origin = TaskOrigin::Browser;
        malformed_run.browser_task_evidence = Some(BrowserTaskEvidence {
            source: "https://github.com/example-org/example-repo.git".to_owned(),
            revision: format!("git:sha1:{}", "a".repeat(40)),
            path: RelativePath::parse("task-definition.json")?,
            closure: None,
            closure_digest: ContentDigest::parse(format!("steward:sha256:{}", "d".repeat(64)))?,
            inline_files: None,
            diagnostics: Default::default(),
            prompt_source: PromptSourceKind::Path,
        });
        let ledger = FakeLedger::default();
        ledger.records.lock().map_err(|_| "lock records")?.extend([
            hidden,
            registry_run,
            malformed_run,
        ]);
        ledger
            .workflow_revisions
            .lock()
            .map_err(|_| "lock workflows")?
            .push(workflow);

        let (service, session_cookie) = signed_in_cookie(LocalFakeIdentity::User).await?;
        let app = protected_router(ledger, service);
        let hidden_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/app/api/v1/tasks/{hidden_digest}"))
                    .header(header::COOKIE, &session_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build hidden Task request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute hidden Task request: {error}"))?;
        assert_eq!(hidden_response.status(), StatusCode::NOT_FOUND);

        let workflow_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/app/api/v1/tasks/{workflow_digest}"))
                    .header(header::COOKIE, &session_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build Workflow Task request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute Workflow Task request: {error}"))?;
        assert_eq!(workflow_response.status(), StatusCode::OK);
        let body = to_bytes(workflow_response.into_body(), 16 * 1024)
            .await
            .map_err(|error| format!("read Workflow Task response: {error}"))?;
        let body: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("decode Workflow Task response: {error}"))?;
        assert_eq!(body["task"]["name"], "release-summary");
        assert_eq!(body["task"]["revision"], "steward:version:2");
        assert_eq!(
            body["task"]["files"]["prompt.md"],
            "Summarize the release.\n"
        );

        let registry_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/app/api/v1/tasks/{registry_digest}"))
                    .header(header::COOKIE, &session_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build registry Task request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute registry Task request: {error}"))?;
        assert_eq!(registry_response.status(), StatusCode::OK);

        let list_response = app
            .oneshot(
                Request::builder()
                    .uri("/app/api/v1/tasks")
                    .header(header::COOKIE, session_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build Task list request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute Task list request: {error}"))?;
        assert_eq!(list_response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(
            &to_bytes(list_response.into_body(), 16 * 1024)
                .await
                .map_err(|error| format!("read Task list response: {error}"))?,
        )
        .map_err(|error| format!("decode Task list response: {error}"))?;
        assert!(body["tasks"].as_array().is_some_and(|tasks| {
            tasks.iter().any(|task| {
                task["contentDigest"] == registry_digest
                    && task["name"] == "release-summary"
                    && task["version"] == 2
            })
        }));
        Ok(())
    }

    #[tokio::test]
    async fn task_library_saves_immutable_versions_and_preserves_each_digest_route()
    -> Result<(), String> {
        fn save_body(version: u64, prompt: &str, task_id: Option<String>) -> serde_json::Value {
            let definition = serde_json::json!({
                "schemaVersion": "steward.task-definition/v2",
                "name": "release-review",
                "version": version,
                "runtime": {"agentRef": "example-agent@1.0.0"},
                "promptText": prompt,
                "outputs": [{"path": "out", "kind": "directory", "required": true}]
            });
            serde_json::json!({
                "taskId": task_id,
                "path": "task-definition.json",
                "files": {"task-definition.json": definition.to_string()},
                "sharedRoles": []
            })
        }

        async fn save(
            app: &Router,
            cookie: &str,
            csrf: &str,
            body: serde_json::Value,
        ) -> Result<(StatusCode, serde_json::Value), String> {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/app/api/v1/tasks")
                        .header(header::COOKIE, cookie)
                        .header(header::ORIGIN, "http://127.0.0.1:33001")
                        .header("sec-fetch-site", "same-origin")
                        .header("x-steward-csrf", csrf)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(body.to_string()))
                        .map_err(|error| format!("build Task save request: {error}"))?,
                )
                .await
                .map_err(|error| format!("execute Task save request: {error}"))?;
            let status = response.status();
            let body = to_bytes(response.into_body(), 32 * 1024)
                .await
                .map_err(|error| format!("read Task save response: {error}"))?;
            let body = serde_json::from_slice(&body)
                .map_err(|error| format!("decode Task save response: {error}"))?;
            Ok((status, body))
        }

        let ledger = FakeLedger::default();
        let (service, session_cookie, csrf) =
            signed_in_cookie_and_csrf(LocalFakeIdentity::User).await?;
        let app = protected_router(ledger, service);
        let (status, v1) = save(
            &app,
            &session_cookie,
            &csrf,
            save_body(1, "Review release one.", None),
        )
        .await?;
        assert_eq!(status, StatusCode::CREATED);
        let task_id = v1["task"]["taskId"]
            .as_str()
            .ok_or_else(|| "v1 response omitted taskId".to_owned())?
            .to_owned();
        let v1_digest = v1["task"]["contentDigest"]
            .as_str()
            .ok_or_else(|| "v1 response omitted digest".to_owned())?
            .to_owned();

        let (status, v2) = save(
            &app,
            &session_cookie,
            &csrf,
            save_body(2, "Review release two.", Some(task_id)),
        )
        .await?;
        assert_eq!(status, StatusCode::OK);
        let v2_digest = v2["task"]["contentDigest"]
            .as_str()
            .ok_or_else(|| "v2 response omitted digest".to_owned())?
            .to_owned();
        assert_ne!(v1_digest, v2_digest);

        for (digest, version) in [(&v1_digest, 1), (&v2_digest, 2)] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/app/api/v1/tasks/{digest}"))
                        .header(header::COOKIE, &session_cookie)
                        .body(Body::empty())
                        .map_err(|error| format!("build Task detail request: {error}"))?,
                )
                .await
                .map_err(|error| format!("execute Task detail request: {error}"))?;
            assert_eq!(response.status(), StatusCode::OK);
            let body: serde_json::Value = serde_json::from_slice(
                &to_bytes(response.into_body(), 32 * 1024)
                    .await
                    .map_err(|error| format!("read Task detail response: {error}"))?,
            )
            .map_err(|error| format!("decode Task detail response: {error}"))?;
            assert_eq!(body["task"]["version"], version);
            assert_eq!(body["task"]["versions"].as_array().map(Vec::len), Some(2));
        }

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/app/api/v1/tasks")
                    .header(header::COOKIE, session_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build Task list request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute Task list request: {error}"))?;
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(
            &to_bytes(response.into_body(), 32 * 1024)
                .await
                .map_err(|error| format!("read Task list response: {error}"))?,
        )
        .map_err(|error| format!("decode Task list response: {error}"))?;
        assert_eq!(body["tasks"].as_array().map(Vec::len), Some(2));
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
            prompt_source: PromptSourceKind::Path,
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
        let response = protected_router_with_task_reruns(ledger.clone(), rerunner, service.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/app/api/v1/runs/{source_task_uid}/rerun"))
                    .header(header::COOKIE, &session_cookie)
                    .header(header::ORIGIN, "http://127.0.0.1:33001")
                    .header("sec-fetch-site", "same-origin")
                    .header("x-steward-csrf", &csrf)
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

        let missing = protected_router_with_task_reruns(
            ledger,
            Arc::new(FailingBrowserTaskRerunner(
                BrowserTaskRerunError::InferenceKeyMissing,
            )),
            service,
        )
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/app/api/v1/runs/{source_task_uid}/rerun"))
                .header(header::COOKIE, session_cookie)
                .header(header::ORIGIN, "http://127.0.0.1:33001")
                .header("sec-fetch-site", "same-origin")
                .header("x-steward-csrf", csrf)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"idempotencyKey":"missing-key"}"#))
                .map_err(|error| format!("build missing-key rerun request: {error}"))?,
        )
        .await
        .map_err(|error| format!("execute missing-key rerun request: {error}"))?;
        assert_eq!(missing.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body: serde_json::Value = serde_json::from_slice(
            &to_bytes(missing.into_body(), 4096)
                .await
                .map_err(|error| format!("read missing-key rerun response: {error}"))?,
        )
        .map_err(|error| format!("decode missing-key rerun response: {error}"))?;
        assert_eq!(body["error"], "inference_key_missing");
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
        let archive = AgentRunOutputArchive {
            content: output_tar("out/result.txt", b"hello\n"),
            contract: Some(TASK_OUTPUT_ARCHIVE_CONTRACT.to_owned()),
        };
        assert_eq!(
            output_archive_entries(&archive),
            Ok(vec![TaskOutputArchiveEntry {
                path: "result.txt".to_owned(),
                offset: 512,
                size: 6,
            }])
        );
        for content in [
            output_tar("secret.txt", b"no"),
            output_tar("out/../secret.txt", b"no"),
        ] {
            assert!(
                output_archive_entries(&AgentRunOutputArchive {
                    content,
                    contract: Some(TASK_OUTPUT_ARCHIVE_CONTRACT.to_owned()),
                })
                .is_err()
            );
        }
    }

    #[test]
    fn output_archive_rejects_new_mixed_archives_but_reads_historical_rows() {
        let content = output_tar_entries(&[
            ("out/tool-calls.json", b"[]\n"),
            ("out/report.md", b"complete\n"),
            (".steward/diagnostics/stdout.log", b"agent output\n"),
            (".steward/diagnostics/stderr.log", b"agent warning\n"),
        ]);
        assert!(
            output_archive_entries(&AgentRunOutputArchive {
                content: content.clone(),
                contract: Some(TASK_OUTPUT_ARCHIVE_CONTRACT.to_owned()),
            })
            .is_err()
        );
        let entries = output_archive_entries(&AgentRunOutputArchive {
            content,
            contract: None,
        })
        .unwrap_or_default();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            ["tool-calls.json", "report.md"]
        );

        assert!(
            output_archive_entries(&AgentRunOutputArchive {
                content: output_tar_entries(&[
                    ("out/report.md", b"complete\n"),
                    (".steward/diagnostics/trace.log", b"not reserved\n"),
                ]),
                contract: None,
            })
            .is_err()
        );
    }
}
