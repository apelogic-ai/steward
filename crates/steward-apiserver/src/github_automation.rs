//! Owner-scoped GitHub repository automation backed by governed Connections operations.
//!
//! The one exception is the default repository listing. With the source GitHub App
//! configured, it lists the operator-admitted source repositories directly.

use std::collections::BTreeMap;
use std::hash::Hash;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use futures::StreamExt as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use steward_ports::{GitRepositoryDescription, GitRepositoryReference};
use steward_store::AgentRunRecord;
use steward_types::direct_package::{ExecutionLogMode, TaskOrigin};
use steward_types::{CanonicalUserId, TaskPhase};
use uuid::Uuid;

use crate::browser_auth::{BrowserAuthService, BrowserSessionBinding, protect_browser_routes};
use crate::connections::{
    ConnectionBrokerError, ConnectionMutationProof, ConnectionSession, adapt_browser_context,
};
use crate::github_actions::{
    DirectPackageGithubActionsWorkflowContext, GithubActionsEnvelopeSelection, StewardRunRelease,
    StewardRunWorkflowInstallationMode, render_direct_package_github_actions_workflow,
    steward_run_supports_package_path_invocation,
};
use crate::governed_connections::{
    ConnectionOperationKind, GovernedConnectionsBroker, ProviderConnectionStatusSource,
    SplitConnectionsBroker, valid_repository_listing_entry,
};
use crate::tasks::TaskApiConfig;
use crate::{AgentRunLedger, BoxFuture};

pub const GITHUB_AUTOMATION_API_VERSION: &str = "steward.github-automation/v1";
const DEFAULT_PAGE_SIZE: u32 = 30;
const MAX_PAGE_SIZE: u32 = 100;
/// How long a fully resolved admitted repository listing is served from memory.
const ADMITTED_REPOSITORIES_TTL: Duration = Duration::from_secs(10 * 60);
/// How long before the App is asked again for only the repositories it did not resolve,
/// and before a failed full refresh of a usable listing is retried.
const ADMITTED_REPOSITORIES_PARTIAL_TTL: Duration = Duration::from_secs(30);
/// How long a cold-start failure is shared with later requests before a new attempt.
const ADMITTED_REPOSITORIES_FAILURE_TTL: Duration = Duration::from_secs(5);
/// Deadline for one resolution. Repositories still pending at the deadline count as
/// unresolved; every repository that finished before it is kept.
const ADMITTED_REPOSITORIES_RESOLUTION_TIMEOUT: Duration = Duration::from_secs(30);
/// Unresolved repositories named in one diagnostic line.
const ADMITTED_REPOSITORIES_REPORTED_FAILURES: usize = 5;
/// Longest a repository stays listed without a successful resolution. A definitive
/// rejection removes it at once.
const ADMITTED_REPOSITORIES_MAX_AGE: Duration = Duration::from_secs(20 * 60);
/// Longest a request waits for a first listing, even if a refresh never reports back.
const ADMITTED_REPOSITORIES_COLD_WAIT: Duration =
    ADMITTED_REPOSITORIES_RESOLUTION_TIMEOUT.saturating_add(Duration::from_secs(5));
/// Count of admitted repositories the source GitHub App could not resolve.
pub(crate) const UNRESOLVED_REPOSITORIES_HEADER: &str = "x-steward-unresolved-repositories";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GithubAutomationIdentity {
    pub(crate) idempotency_identity: String,
    pub(crate) idempotency_scope: Option<String>,
    pub(crate) publication_subject: Option<String>,
}

impl GithubAutomationIdentity {
    /// Creates an identity for a read whose result must not be reused by another request.
    pub fn fresh(operation: &str) -> Self {
        Self {
            idempotency_identity: format!("github-{operation}:{}", Uuid::new_v4()),
            idempotency_scope: None,
            publication_subject: None,
        }
    }

    /// Creates the content-bound identity used for governed GitHub mutations.
    pub fn write(
        operation: &str,
        client_key: &str,
        payload: &Value,
        subject: Option<&str>,
    ) -> Self {
        let subject = subject.map(|subject| operation_subject(operation, subject));
        let scope = match subject.as_deref() {
            Some(subject) => format!(
                "{subject}:client:sha256:{:x}",
                Sha256::digest(client_key.as_bytes())
            ),
            None => format!(
                "github-{operation}:client:sha256:{:x}",
                Sha256::digest(client_key.as_bytes())
            ),
        };
        let payload = payload.to_string();
        Self {
            idempotency_identity: format!(
                "{scope}:payload:sha256:{:x}",
                Sha256::digest(payload.as_bytes())
            ),
            idempotency_scope: Some(scope),
            publication_subject: (operation == "publish").then_some(subject).flatten(),
        }
    }
}

pub trait GithubAutomationBroker<B>: Clone + Send + Sync + 'static
where
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    fn execute<'a>(
        &'a self,
        session: &'a ConnectionSession<B>,
        operation: ConnectionOperationKind,
        request: Value,
        identity: &'a GithubAutomationIdentity,
    ) -> BoxFuture<'a, Result<Value, ConnectionBrokerError>>;

    fn evidence<'a>(
        &'a self,
        canonical_user_id: &'a CanonicalUserId,
        operation: ConnectionOperationKind,
        idempotency_identity_prefix: &'a str,
    ) -> BoxFuture<'a, Result<Option<Value>, ConnectionBrokerError>>;
}

impl<B> GithubAutomationBroker<B> for GovernedConnectionsBroker<B>
where
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    fn execute<'a>(
        &'a self,
        session: &'a ConnectionSession<B>,
        operation: ConnectionOperationKind,
        request: Value,
        identity: &'a GithubAutomationIdentity,
    ) -> BoxFuture<'a, Result<Value, ConnectionBrokerError>> {
        Box::pin(async move {
            self.run_automation_operation(
                session,
                operation,
                request,
                &identity.idempotency_identity,
                identity.idempotency_scope.as_deref(),
                identity.publication_subject.as_deref(),
            )
            .await
        })
    }

    fn evidence<'a>(
        &'a self,
        canonical_user_id: &'a CanonicalUserId,
        operation: ConnectionOperationKind,
        idempotency_identity_prefix: &'a str,
    ) -> BoxFuture<'a, Result<Option<Value>, ConnectionBrokerError>> {
        Box::pin(async move {
            self.stored_automation_result(canonical_user_id, operation, idempotency_identity_prefix)
                .await
        })
    }
}

impl<B, M, S> GithubAutomationBroker<B> for SplitConnectionsBroker<M, S>
where
    B: Clone + Eq + Hash + Send + Sync + 'static,
    M: GithubAutomationBroker<B>,
    S: ProviderConnectionStatusSource<B>,
{
    fn execute<'a>(
        &'a self,
        session: &'a ConnectionSession<B>,
        operation: ConnectionOperationKind,
        request: Value,
        identity: &'a GithubAutomationIdentity,
    ) -> BoxFuture<'a, Result<Value, ConnectionBrokerError>> {
        self.governed_mutations()
            .execute(session, operation, request, identity)
    }

    fn evidence<'a>(
        &'a self,
        canonical_user_id: &'a CanonicalUserId,
        operation: ConnectionOperationKind,
        idempotency_identity_prefix: &'a str,
    ) -> BoxFuture<'a, Result<Option<Value>, ConnectionBrokerError>> {
        self.governed_mutations().evidence(
            canonical_user_id,
            operation,
            idempotency_identity_prefix,
        )
    }
}

type AdmittedRepositoryClock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// In-process cache of the admitted source repositories.
///
/// A detached, single-flight refresher resolves repositories and writes the cache whatever
/// happens to the request that started it. Requests are served the current listing at once,
/// even while it is being revalidated; only a request with no usable listing waits.
#[derive(Clone)]
struct AdmittedRepositoryCache {
    /// The distinct admitted source repositories. Bindings are fixed for the process.
    key: Arc<Vec<GitRepositoryReference>>,
    state: Arc<std::sync::Mutex<AdmittedRepositoryState>>,
    refreshed: Arc<tokio::sync::watch::Sender<u64>>,
    clock: AdmittedRepositoryClock,
}

#[derive(Default)]
struct AdmittedRepositoryState {
    /// Each listed repository with the time it last resolved successfully.
    resolved: BTreeMap<GitRepositoryReference, (GithubRepositoryView, Instant)>,
    /// The listed repositories, sorted once per refresh.
    listing: Arc<Vec<GithubRepositoryView>>,
    /// Repositories that did not resolve in their latest attempt, listed or not.
    unresolved: Vec<GitRepositoryReference>,
    /// The hosting plane cannot describe repositories; use the governed listing.
    unsupported: bool,
    refreshing: bool,
    /// When every repository is resolved again.
    expires_at: Option<Instant>,
    /// When only the unresolved repositories are retried.
    retry_at: Option<Instant>,
    /// Until when a cold-start failure is shared instead of retried.
    failed_until: Option<Instant>,
}

/// What a refresh resolves: every admitted repository, or only the unresolved ones.
#[derive(Clone, Copy, Eq, PartialEq)]
enum AdmittedRefresh {
    Full,
    Unresolved,
}

impl AdmittedRepositoryState {
    fn refresh_due(&self, now: Instant) -> Option<AdmittedRefresh> {
        if self.resolved.is_empty() {
            return self
                .failed_until
                .is_none_or(|until| now >= until)
                .then_some(AdmittedRefresh::Full);
        }
        if self.expires_at.is_none_or(|expires_at| now >= expires_at) {
            return Some(AdmittedRefresh::Full);
        }
        (!self.unresolved.is_empty() && self.retry_at.is_none_or(|retry_at| now >= retry_at))
            .then_some(AdmittedRefresh::Unresolved)
    }

    fn listing(&self) -> AdmittedRepositories {
        if self.unsupported {
            return AdmittedRepositories::NotConfigured;
        }
        if self.listing.is_empty() {
            return AdmittedRepositories::Unavailable;
        }
        AdmittedRepositories::Listed {
            repositories: Arc::clone(&self.listing),
            unresolved: self.unresolved.len(),
        }
    }

    fn rebuild_listing(&mut self) {
        let mut repositories = self
            .resolved
            .values()
            .map(|(view, _)| view.clone())
            .collect::<Vec<_>>();
        repositories.sort_by(|left, right| {
            (
                left.owner.to_ascii_lowercase(),
                left.name.to_ascii_lowercase(),
            )
                .cmp(&(
                    right.owner.to_ascii_lowercase(),
                    right.name.to_ascii_lowercase(),
                ))
        });
        self.listing = Arc::new(repositories);
    }
}

impl AdmittedRepositoryCache {
    fn new(clock: AdmittedRepositoryClock, key: Vec<GitRepositoryReference>) -> Self {
        Self {
            key: Arc::new(key),
            state: Arc::new(std::sync::Mutex::new(AdmittedRepositoryState::default())),
            refreshed: Arc::new(tokio::sync::watch::channel(0).0),
            clock,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, AdmittedRepositoryState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Merges a refresh outcome; `None` means the refresh ended without one.
    ///
    /// A successful resolution updates an entry, a definitive rejection removes it, and any
    /// other failure keeps it until it is older than the maximum age. A kept entry that
    /// failed still counts as unresolved.
    fn complete_refresh(
        &self,
        refresh: AdmittedRefresh,
        targets: &[GitRepositoryReference],
        outcome: Option<ResolutionOutcome>,
    ) {
        let now = (self.clock)();
        let outcome = outcome.unwrap_or_default();
        {
            let mut state = self.lock();
            state.refreshing = false;
            if outcome.unsupported && refresh == AdmittedRefresh::Full && state.resolved.is_empty()
            {
                state.unsupported = true;
            } else {
                let fresh = outcome
                    .resolved
                    .keys()
                    .cloned()
                    .collect::<std::collections::BTreeSet<_>>();
                for (reference, view) in outcome.resolved {
                    state.resolved.insert(reference, (view, now));
                }
                for reference in &outcome.rejected {
                    state.resolved.remove(reference);
                }
                state.resolved.retain(|_, (_, resolved_at)| {
                    now.saturating_duration_since(*resolved_at) <= ADMITTED_REPOSITORIES_MAX_AGE
                });
                let targets = targets.iter().collect::<std::collections::BTreeSet<_>>();
                let unresolved = self
                    .key
                    .iter()
                    .filter(|reference| {
                        !state.resolved.contains_key(*reference)
                            || (targets.contains(reference) && !fresh.contains(*reference))
                    })
                    .cloned()
                    .collect();
                state.unresolved = unresolved;
                if state.resolved.is_empty() {
                    state.failed_until = Some(now + ADMITTED_REPOSITORIES_FAILURE_TTL);
                } else {
                    state.failed_until = None;
                    state.retry_at = Some(now + ADMITTED_REPOSITORIES_PARTIAL_TTL);
                    if refresh == AdmittedRefresh::Full {
                        state.expires_at = Some(
                            now + if fresh.is_empty() {
                                ADMITTED_REPOSITORIES_PARTIAL_TTL
                            } else {
                                ADMITTED_REPOSITORIES_TTL
                            },
                        );
                    }
                }
                state.rebuild_listing();
            }
        }
        self.refreshed.send_modify(|generation| {
            *generation = generation.wrapping_add(1);
        });
    }
}

/// Completes a refresh exactly once, including when its task is dropped before it runs.
struct AdmittedRefreshCompletion {
    pending: Option<(
        AdmittedRepositoryCache,
        AdmittedRefresh,
        Vec<GitRepositoryReference>,
    )>,
}

impl AdmittedRefreshCompletion {
    fn new(
        cache: AdmittedRepositoryCache,
        refresh: AdmittedRefresh,
        targets: Vec<GitRepositoryReference>,
    ) -> Self {
        Self {
            pending: Some((cache, refresh, targets)),
        }
    }

    fn targets(&self) -> Vec<GitRepositoryReference> {
        self.pending
            .as_ref()
            .map(|(_, _, targets)| targets.clone())
            .unwrap_or_default()
    }

    fn finish(mut self, outcome: ResolutionOutcome) {
        if let Some((cache, refresh, targets)) = self.pending.take() {
            cache.complete_refresh(refresh, &targets, Some(outcome));
        }
    }
}

impl Drop for AdmittedRefreshCompletion {
    fn drop(&mut self) {
        if let Some((cache, refresh, targets)) = self.pending.take() {
            cache.complete_refresh(refresh, &targets, None);
        }
    }
}

#[derive(Clone)]
pub struct GithubAutomationConfig {
    task_api: TaskApiConfig,
    steward_run_release: StewardRunRelease,
    workflow_installation_mode: StewardRunWorkflowInstallationMode,
    task_identity_discovery_enabled: bool,
    admitted_repositories: AdmittedRepositoryCache,
}

impl GithubAutomationConfig {
    pub fn new(
        task_api: TaskApiConfig,
        steward_run_release: StewardRunRelease,
        workflow_installation_mode: StewardRunWorkflowInstallationMode,
        task_identity_discovery_enabled: bool,
    ) -> Self {
        let admitted_key = task_api.admitted_source_repository_references();
        Self {
            task_api,
            steward_run_release,
            workflow_installation_mode,
            task_identity_discovery_enabled,
            admitted_repositories: AdmittedRepositoryCache::new(
                Arc::new(Instant::now),
                admitted_key,
            ),
        }
    }

    #[cfg(test)]
    fn with_admitted_repository_clock(mut self, clock: AdmittedRepositoryClock) -> Self {
        self.admitted_repositories =
            AdmittedRepositoryCache::new(clock, self.admitted_repositories.key.to_vec());
        self
    }

    #[cfg(test)]
    fn with_task_api(mut self, task_api: TaskApiConfig) -> Self {
        self.admitted_repositories = AdmittedRepositoryCache::new(
            Arc::clone(&self.admitted_repositories.clock),
            task_api.admitted_source_repository_references(),
        );
        self.task_api = task_api;
        self
    }
}

#[derive(Clone)]
pub(crate) struct GithubAutomationState<L, P> {
    ledger: L,
    broker: P,
    config: GithubAutomationConfig,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RepositoryQuery {
    /// Empty lists the admitted source repositories when the source GitHub App is
    /// configured, and otherwise the repositories owned by the authenticated GitHub user.
    #[serde(default)]
    query: String,
    #[serde(default = "first_page")]
    page: u32,
    #[serde(default = "default_page_size")]
    per_page: u32,
}

const fn first_page() -> u32 {
    1
}

const fn default_page_size() -> u32 {
    DEFAULT_PAGE_SIZE
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GithubRepositoryView {
    owner: String,
    owner_id: String,
    name: String,
    repository_id: String,
    default_branch: String,
    private: bool,
    url: String,
    ready: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    missing_prerequisite: Option<&'static str>,
}

/// Where a repository listing came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum GithubRepositoriesSource {
    /// Operator-admitted source repositories resolved through the source GitHub App.
    Admitted,
    /// Repositories visible to the user's governed GitHub connection.
    Connection,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GithubRepositoriesResponse {
    api_version: &'static str,
    /// GitHub login of the governed connection. Empty for an admitted listing, which
    /// does not consult the user's connection.
    login: String,
    repositories: Vec<GithubRepositoryView>,
    page: u32,
    has_next_page: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<GithubRepositoriesSource>,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RepositoryTargetRequest {
    owner: String,
    repository: String,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PublishTaskRequest {
    owner: String,
    repository: String,
    idempotency_key: String,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DispatchTaskRequest {
    owner: String,
    repository: String,
    #[serde(default)]
    inputs: BTreeMap<String, String>,
    idempotency_key: String,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RunStatusQuery {
    owner: String,
    repository: String,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorkflowDetectionResponse {
    api_version: &'static str,
    path: String,
    exists: bool,
    compatible: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    sha: Option<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PublishTaskResponse {
    api_version: &'static str,
    pull_request_url: String,
    pull_request_number: u64,
    branch: String,
    package_digest: String,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DispatchTaskResponse {
    api_version: &'static str,
    run_id: u64,
    url: String,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GithubTaskBundleResponse {
    api_version: &'static str,
    files: BTreeMap<String, String>,
    workflow_path: String,
    package_digest: String,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GithubAutomationEvidenceResponse {
    api_version: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    publication: Option<PublishTaskResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dispatch: Option<DispatchTaskResponse>,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GithubOnboardingEvidenceResponse {
    api_version: &'static str,
    publication_observed: bool,
    workflow_observed: bool,
    dispatch_observed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GithubJobView {
    id: u64,
    name: String,
    status: String,
    conclusion: Option<String>,
    url: String,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GithubRunStatusResponse {
    api_version: &'static str,
    run_id: u64,
    run_attempt: u32,
    phase: String,
    conclusion: Option<String>,
    url: String,
    jobs: Vec<GithubJobView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_log: Option<String>,
    #[schema(value_type = Option<String>, format = "uuid")]
    #[serde(skip_serializing_if = "Option::is_none")]
    linked_task_uid: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    linked_task_phase: Option<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GithubAutomationErrorResponse {
    api_version: &'static str,
    error: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    manual_files: Option<BTreeMap<String, String>>,
}

struct ExactRepositoryBundle {
    files: BTreeMap<String, String>,
    workflow_path: String,
    workflow_content: String,
    package_digest: String,
}

pub fn protected_router<L, P>(
    ledger: L,
    broker: P,
    config: GithubAutomationConfig,
    browser_auth: BrowserAuthService,
) -> Router
where
    L: AgentRunLedger,
    P: GithubAutomationBroker<BrowserSessionBinding>,
{
    let routes = Router::new()
        .route(
            "/app/api/v1/github/repositories",
            get(list_repositories::<L, P>),
        )
        .route(
            "/app/api/v1/runs/{task_uid}/github/workflow",
            post(detect_workflow::<L, P>),
        )
        .route(
            "/app/api/v1/runs/{task_uid}/github/bundle",
            get(github_task_bundle::<L, P>),
        )
        .route(
            "/app/api/v1/runs/{task_uid}/github/evidence",
            get(github_automation_evidence::<L, P>),
        )
        .route(
            "/app/api/v1/runs/{task_uid}/github/onboarding",
            get(github_onboarding_evidence::<L, P>),
        )
        .route(
            "/app/api/v1/runs/{task_uid}/github/publish",
            post(publish_task::<L, P>),
        )
        .route(
            "/app/api/v1/runs/{task_uid}/github/dispatch",
            post(dispatch_task::<L, P>),
        )
        .route(
            "/app/api/v1/runs/{task_uid}/github/runs/{run_id}",
            get(github_run_status::<L, P>),
        )
        .with_state(GithubAutomationState {
            ledger,
            broker,
            config,
        })
        .route_layer(middleware::from_fn(adapt_browser_context));
    protect_browser_routes(routes, browser_auth)
}

#[utoipa::path(
    get,
    path = "/app/api/v1/runs/{task_uid}/github/bundle",
    params(("task_uid" = String, Path, format = "uuid")),
    responses(
        (status = 200, body = GithubTaskBundleResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 404, description = "Run is unavailable"),
        (status = 409, description = "Run is not publishable"),
        (status = 503, body = GithubAutomationErrorResponse)
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn github_task_bundle<L, P>(
    session: Option<Extension<ConnectionSession<BrowserSessionBinding>>>,
    State(state): State<GithubAutomationState<L, P>>,
    Path(task_uid): Path<Uuid>,
) -> Response
where
    L: AgentRunLedger,
    P: GithubAutomationBroker<BrowserSessionBinding>,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    match exact_bundle(&state, &session, task_uid).await {
        Ok(bundle) => no_store_json(GithubTaskBundleResponse {
            api_version: GITHUB_AUTOMATION_API_VERSION,
            files: bundle.files,
            workflow_path: bundle.workflow_path,
            package_digest: bundle.package_digest,
        }),
        Err(response) => response,
    }
}

#[utoipa::path(
    get,
    path = "/app/api/v1/runs/{task_uid}/github/evidence",
    params(("task_uid" = String, Path, format = "uuid"), RunStatusQuery),
    responses(
        (status = 200, body = GithubAutomationEvidenceResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Repository is not admitted"),
        (status = 404, description = "Run or repository is unavailable"),
        (status = 409, description = "Run evidence is unavailable or invalid"),
        (status = 503, body = GithubAutomationErrorResponse)
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn github_automation_evidence<L, P>(
    session: Option<Extension<ConnectionSession<BrowserSessionBinding>>>,
    State(state): State<GithubAutomationState<L, P>>,
    Path(task_uid): Path<Uuid>,
    Query(query): Query<RunStatusQuery>,
) -> Response
where
    L: AgentRunLedger,
    P: GithubAutomationBroker<BrowserSessionBinding>,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    match resolve_repository(&state, &session, &query.owner, &query.repository).await {
        Ok(repository) if repository.ready => {}
        Ok(_) => return StatusCode::FORBIDDEN.into_response(),
        Err(response) => return response,
    }
    let run = match owned_successful_browser_run(&state.ledger, &session, task_uid).await {
        Ok(run) => run,
        Err(response) => return response,
    };
    let package_digest = match run.browser_task_evidence {
        Some(evidence) if evidence.validate().is_ok() => evidence.closure_digest,
        _ => return StatusCode::CONFLICT.into_response(),
    };
    let subject = automation_subject(task_uid, &query.owner, &query.repository);
    let publication = match state
        .broker
        .evidence(
            &session.subject.canonical_user_id,
            ConnectionOperationKind::Publish,
            &operation_subject("publish", &subject),
        )
        .await
    {
        Ok(Some(value)) => match publish_response(value, package_digest.as_str().to_owned()) {
            Ok(response) => Some(response),
            Err(()) => return unavailable(),
        },
        Ok(None) => None,
        Err(error) => return automation_error(error),
    };
    let dispatch = match state
        .broker
        .evidence(
            &session.subject.canonical_user_id,
            ConnectionOperationKind::Dispatch,
            &operation_subject("dispatch", &subject),
        )
        .await
    {
        Ok(Some(value)) => match dispatch_response(value) {
            Ok(response) => Some(response),
            Err(()) => return unavailable(),
        },
        Ok(None) => None,
        Err(error) => return automation_error(error),
    };
    no_store_json(GithubAutomationEvidenceResponse {
        api_version: GITHUB_AUTOMATION_API_VERSION,
        publication,
        dispatch,
    })
}

#[utoipa::path(
    get,
    path = "/app/api/v1/runs/{task_uid}/github/onboarding",
    params(("task_uid" = String, Path, format = "uuid")),
    responses(
        (status = 200, body = GithubOnboardingEvidenceResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 404, description = "Run is unavailable"),
        (status = 409, description = "Run evidence is unavailable or invalid"),
        (status = 503, body = GithubAutomationErrorResponse)
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn github_onboarding_evidence<L, P>(
    session: Option<Extension<ConnectionSession<BrowserSessionBinding>>>,
    State(state): State<GithubAutomationState<L, P>>,
    Path(task_uid): Path<Uuid>,
) -> Response
where
    L: AgentRunLedger,
    P: GithubAutomationBroker<BrowserSessionBinding>,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let run = match owned_successful_browser_run(&state.ledger, &session, task_uid).await {
        Ok(run) => run,
        Err(response) => return response,
    };
    let package_digest = match run.browser_task_evidence {
        Some(evidence) if evidence.validate().is_ok() => evidence.closure_digest,
        _ => return StatusCode::CONFLICT.into_response(),
    };
    let publication = match state
        .broker
        .evidence(
            &session.subject.canonical_user_id,
            ConnectionOperationKind::Publish,
            &task_operation_prefix("publish", task_uid),
        )
        .await
    {
        Ok(Some(value)) => match publish_response(value, package_digest.as_str().to_owned()) {
            Ok(_) => true,
            Err(()) => return unavailable(),
        },
        Ok(None) => false,
        Err(error) => return automation_error(error),
    };
    let workflow = match state
        .broker
        .evidence(
            &session.subject.canonical_user_id,
            ConnectionOperationKind::Workflow,
            &task_operation_prefix("workflow", task_uid),
        )
        .await
    {
        Ok(Some(value)) => match workflow_response(value) {
            Ok(response) => response.compatible,
            Err(()) => return unavailable(),
        },
        Ok(None) => false,
        Err(error) => return automation_error(error),
    };
    let dispatch = match state
        .broker
        .evidence(
            &session.subject.canonical_user_id,
            ConnectionOperationKind::Dispatch,
            &task_operation_prefix("dispatch", task_uid),
        )
        .await
    {
        Ok(Some(value)) => match dispatch_response(value) {
            Ok(_) => true,
            Err(()) => return unavailable(),
        },
        Ok(None) => false,
        Err(error) => return automation_error(error),
    };
    no_store_json(GithubOnboardingEvidenceResponse {
        api_version: GITHUB_AUTOMATION_API_VERSION,
        publication_observed: publication,
        workflow_observed: workflow,
        dispatch_observed: dispatch,
    })
}

#[utoipa::path(
    get,
    path = "/app/api/v1/github/repositories",
    params(RepositoryQuery),
    responses(
        (status = 200, body = GithubRepositoriesResponse, description = "Repositories. An admitted listing that the source GitHub App resolved only partly carries an x-steward-unresolved-repositories count header."),
        (status = 400, description = "Repository query is invalid"),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 503, body = GithubAutomationErrorResponse)
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn list_repositories<L, P>(
    session: Option<Extension<ConnectionSession<BrowserSessionBinding>>>,
    State(state): State<GithubAutomationState<L, P>>,
    Query(query): Query<RepositoryQuery>,
) -> Response
where
    L: AgentRunLedger,
    P: GithubAutomationBroker<BrowserSessionBinding>,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if query.page == 0
        || query.per_page == 0
        || query.per_page > MAX_PAGE_SIZE
        || query.query.len() > 200
        || query.query.chars().any(char::is_control)
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if query.query.is_empty() {
        match admitted_repositories(&state.config).await {
            AdmittedRepositories::NotConfigured => {}
            AdmittedRepositories::Unavailable => return source_app_unavailable(),
            AdmittedRepositories::Listed {
                repositories,
                unresolved,
            } => return admitted_page(repositories, unresolved, query.page, query.per_page),
        }
    }
    let identity = fresh_operation_identity("repositories");
    let result = match state
        .broker
        .execute(
            &session,
            ConnectionOperationKind::Repositories,
            json!({
                "query": query.query,
                "page": query.page,
                "perPage": query.per_page,
            }),
            &identity,
        )
        .await
    {
        Ok(result) => result,
        Err(error) => return automation_error(error),
    };
    match repository_response(&state.config.task_api, result) {
        Ok(response) => no_store_json(response),
        Err(()) => unavailable(),
    }
}

#[utoipa::path(
    post,
    path = "/app/api/v1/runs/{task_uid}/github/workflow",
    params(("task_uid" = String, Path, format = "uuid"), ("X-Steward-CSRF" = String, Header)),
    request_body = RepositoryTargetRequest,
    responses(
        (status = 200, body = WorkflowDetectionResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Mutation proof is invalid"),
        (status = 404, description = "Run or repository is unavailable"),
        (status = 409, description = "Run is not publishable"),
        (status = 503, body = GithubAutomationErrorResponse)
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn detect_workflow<L, P>(
    session: Option<Extension<ConnectionSession<BrowserSessionBinding>>>,
    proof: Option<Extension<ConnectionMutationProof>>,
    State(state): State<GithubAutomationState<L, P>>,
    Path(task_uid): Path<Uuid>,
    Json(request): Json<RepositoryTargetRequest>,
) -> Response
where
    L: AgentRunLedger,
    P: GithubAutomationBroker<BrowserSessionBinding>,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if proof.is_none() {
        return StatusCode::FORBIDDEN.into_response();
    }
    let repository =
        match resolve_repository(&state, &session, &request.owner, &request.repository).await {
            Ok(repository) => repository,
            Err(response) => return response,
        };
    let bundle = match exact_bundle(&state, &session, task_uid).await {
        Ok(bundle) => bundle,
        Err(response) => return response,
    };
    let subject = automation_subject(task_uid, &repository.owner, &repository.name);
    let identity = scoped_read_operation_identity("workflow", &subject);
    let result = match state
        .broker
        .execute(
            &session,
            ConnectionOperationKind::Workflow,
            json!({
                "owner": repository.owner,
                "repo": repository.name,
                "path": bundle.workflow_path,
                "ref": repository.default_branch,
                "expectedContent": bundle.workflow_content,
            }),
            &identity,
        )
        .await
    {
        Ok(result) => result,
        Err(error) => return automation_error(error),
    };
    match workflow_response(result) {
        Ok(response) => no_store_json(response),
        Err(()) => unavailable(),
    }
}

#[utoipa::path(
    post,
    path = "/app/api/v1/runs/{task_uid}/github/publish",
    params(("task_uid" = String, Path, format = "uuid"), ("X-Steward-CSRF" = String, Header)),
    request_body = PublishTaskRequest,
    responses(
        (status = 200, body = PublishTaskResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Mutation proof is invalid or repository is not admitted"),
        (status = 404, description = "Run or repository is unavailable"),
        (status = 409, description = "Run is not publishable"),
        (status = 422, description = "Publication request is invalid"),
        (status = 503, body = GithubAutomationErrorResponse)
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn publish_task<L, P>(
    session: Option<Extension<ConnectionSession<BrowserSessionBinding>>>,
    proof: Option<Extension<ConnectionMutationProof>>,
    State(state): State<GithubAutomationState<L, P>>,
    Path(task_uid): Path<Uuid>,
    Json(request): Json<PublishTaskRequest>,
) -> Response
where
    L: AgentRunLedger,
    P: GithubAutomationBroker<BrowserSessionBinding>,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if proof.is_none() {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !valid_idempotency_key(&request.idempotency_key) {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    }
    let repository =
        match resolve_repository(&state, &session, &request.owner, &request.repository).await {
            Ok(repository) if repository.ready => repository,
            Ok(_) => return StatusCode::FORBIDDEN.into_response(),
            Err(response) => return response,
        };
    let bundle = match exact_bundle(&state, &session, task_uid).await {
        Ok(bundle) => bundle,
        Err(response) => return response,
    };
    let branch = format!("steward/task-{}", task_uid.simple());
    let files = bundle
        .files
        .iter()
        .map(|(path, content)| json!({"path": path, "content": content}))
        .collect::<Vec<_>>();
    let operation_request = json!({
        "owner": repository.owner,
        "repo": repository.name,
        "baseBranch": repository.default_branch,
        "branch": branch,
        "title": "chore: add Steward governed task",
        "body": format!("Publishes the exact governed package tested by Steward Task `{task_uid}`."),
        "files": files,
    });
    let publication_subject = automation_subject(task_uid, &repository.owner, &repository.name);
    let identity = write_operation_identity(
        "publish",
        &request.idempotency_key,
        &operation_request,
        Some(&publication_subject),
    );
    let result = match state
        .broker
        .execute(
            &session,
            ConnectionOperationKind::Publish,
            operation_request,
            &identity,
        )
        .await
    {
        Ok(result) => result,
        Err(error) => return automation_error(error),
    };
    match publish_response(result, bundle.package_digest) {
        Ok(response) => no_store_json(response),
        Err(()) => unavailable(),
    }
}

#[utoipa::path(
    post,
    path = "/app/api/v1/runs/{task_uid}/github/dispatch",
    params(("task_uid" = String, Path, format = "uuid"), ("X-Steward-CSRF" = String, Header)),
    request_body = DispatchTaskRequest,
    responses(
        (status = 200, body = DispatchTaskResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Mutation proof is invalid or repository is not admitted"),
        (status = 404, description = "Run, repository, or workflow is unavailable"),
        (status = 409, description = "Published workflow does not match the tested task"),
        (status = 422, description = "Dispatch inputs are invalid"),
        (status = 503, body = GithubAutomationErrorResponse)
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn dispatch_task<L, P>(
    session: Option<Extension<ConnectionSession<BrowserSessionBinding>>>,
    proof: Option<Extension<ConnectionMutationProof>>,
    State(state): State<GithubAutomationState<L, P>>,
    Path(task_uid): Path<Uuid>,
    Json(request): Json<DispatchTaskRequest>,
) -> Response
where
    L: AgentRunLedger,
    P: GithubAutomationBroker<BrowserSessionBinding>,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if proof.is_none() {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !valid_idempotency_key(&request.idempotency_key)
        || request.inputs.len() > 20
        || request.inputs.iter().any(|(name, value)| {
            !valid_input_name(name) || value.len() > 8 * 1024 || value.chars().any(char::is_control)
        })
    {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    }
    let repository =
        match resolve_repository(&state, &session, &request.owner, &request.repository).await {
            Ok(repository) if repository.ready => repository,
            Ok(_) => return StatusCode::FORBIDDEN.into_response(),
            Err(response) => return response,
        };
    let bundle = match exact_bundle(&state, &session, task_uid).await {
        Ok(bundle) => bundle,
        Err(response) => return response,
    };
    let operation_request = json!({
        "owner": repository.owner,
        "repo": repository.name,
        "workflowId": bundle.workflow_path,
        "ref": repository.default_branch,
        "inputs": request.inputs,
        "expectedContent": bundle.workflow_content,
    });
    let identity = write_operation_identity(
        "dispatch",
        &request.idempotency_key,
        &operation_request,
        Some(&automation_subject(
            task_uid,
            &repository.owner,
            &repository.name,
        )),
    );
    let result = match state
        .broker
        .execute(
            &session,
            ConnectionOperationKind::Dispatch,
            operation_request,
            &identity,
        )
        .await
    {
        Ok(result) => result,
        Err(error) => return automation_error(error),
    };
    match dispatch_response(result) {
        Ok(response) => no_store_json(response),
        Err(()) => unavailable(),
    }
}

#[utoipa::path(
    get,
    path = "/app/api/v1/runs/{task_uid}/github/runs/{run_id}",
    params(("task_uid" = String, Path, format = "uuid"), ("run_id" = u64, Path), RunStatusQuery),
    responses(
        (status = 200, body = GithubRunStatusResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 404, description = "Run or repository is unavailable"),
        (status = 503, body = GithubAutomationErrorResponse)
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn github_run_status<L, P>(
    session: Option<Extension<ConnectionSession<BrowserSessionBinding>>>,
    State(state): State<GithubAutomationState<L, P>>,
    Path((task_uid, run_id)): Path<(Uuid, u64)>,
    Query(query): Query<RunStatusQuery>,
) -> Response
where
    L: AgentRunLedger,
    P: GithubAutomationBroker<BrowserSessionBinding>,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if owned_successful_browser_run(&state.ledger, &session, task_uid)
        .await
        .is_err()
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let repository =
        match resolve_repository(&state, &session, &query.owner, &query.repository).await {
            Ok(repository) => repository,
            Err(response) => return response,
        };
    let identity = fresh_operation_identity("run-status");
    let result = match state
        .broker
        .execute(
            &session,
            ConnectionOperationKind::RunStatus,
            json!({"owner": repository.owner, "repo": repository.name, "runId": run_id}),
            &identity,
        )
        .await
    {
        Ok(result) => result,
        Err(error) => return automation_error(error),
    };
    let run_attempt = result
        .get("runAttempt")
        .and_then(Value::as_u64)
        .and_then(|attempt| u32::try_from(attempt).ok())
        .filter(|attempt| *attempt > 0);
    let repository_name = format!("{}/{}", repository.owner, repository.name);
    let linked = state
        .ledger
        .github_rerun_task(
            session.subject.canonical_user_id.as_str(),
            &repository_name,
            &run_id.to_string(),
            run_attempt.unwrap_or(1).saturating_sub(1),
        )
        .await
        .ok()
        .flatten();
    match run_status_response(result, linked.map(|task| (task.task_uid, task.phase))) {
        Ok(response) => no_store_json(response),
        Err(()) => unavailable(),
    }
}

async fn resolve_repository<L, P>(
    state: &GithubAutomationState<L, P>,
    session: &ConnectionSession<BrowserSessionBinding>,
    owner: &str,
    repository: &str,
) -> Result<GithubRepositoryView, Response>
where
    L: AgentRunLedger,
    P: GithubAutomationBroker<BrowserSessionBinding>,
{
    if !valid_repository_component(owner, 39) || !valid_repository_component(repository, 100) {
        return Err(StatusCode::NOT_FOUND.into_response());
    }
    let query = format!("repo:{owner}/{repository}");
    let identity = fresh_operation_identity("repository");
    let result = state
        .broker
        .execute(
            session,
            ConnectionOperationKind::Repositories,
            json!({"query": query, "page": 1, "perPage": 10}),
            &identity,
        )
        .await
        .map_err(automation_error)?;
    let response =
        repository_response(&state.config.task_api, result).map_err(|()| unavailable())?;
    response
        .repositories
        .into_iter()
        .find(|candidate| candidate.owner == owner && candidate.name == repository)
        .ok_or_else(|| StatusCode::NOT_FOUND.into_response())
}

async fn exact_bundle<L, P>(
    state: &GithubAutomationState<L, P>,
    session: &ConnectionSession<BrowserSessionBinding>,
    task_uid: Uuid,
) -> Result<ExactRepositoryBundle, Response>
where
    L: AgentRunLedger,
    P: GithubAutomationBroker<BrowserSessionBinding>,
{
    let run = owned_successful_browser_run(&state.ledger, session, task_uid).await?;
    let evidence = run
        .browser_task_evidence
        .ok_or_else(|| StatusCode::CONFLICT.into_response())?;
    if evidence.source != "inline" || evidence.validate().is_err() {
        return Err(StatusCode::CONFLICT.into_response());
    }
    let mut files = evidence
        .inline_files
        .ok_or_else(|| StatusCode::CONFLICT.into_response())?;
    if files.len() != 1 || !files.contains_key(evidence.path.as_str()) {
        return Err(StatusCode::CONFLICT.into_response());
    }
    if !steward_run_supports_package_path_invocation(&state.config.steward_run_release) {
        return Err(automation_problem(
            "steward_run_release_unsupported",
            Some(files),
        ));
    }
    let envelope = GithubActionsEnvelopeSelection {
        id: run
            .user_envelope_instance_id
            .ok_or_else(|| StatusCode::CONFLICT.into_response())?,
        revision: u64::try_from(
            run.user_envelope_revision
                .ok_or_else(|| StatusCode::CONFLICT.into_response())?,
        )
        .map_err(|_| StatusCode::CONFLICT.into_response())?,
        digest: run
            .user_envelope_digest
            .ok_or_else(|| StatusCode::CONFLICT.into_response())?,
    };
    let generated =
        render_direct_package_github_actions_workflow(&DirectPackageGithubActionsWorkflowContext {
            envelope,
            invocation_path: None,
            package_path: Some(evidence.path.as_str().to_owned()),
            execution_log: ExecutionLogMode::Full,
            reviewed_release: state.config.steward_run_release.clone(),
            workflow_installation_mode: state.config.workflow_installation_mode,
            task_identity_discovery_enabled: state.config.task_identity_discovery_enabled,
        })
        .map_err(|_| StatusCode::CONFLICT.into_response())?;
    if files
        .insert(generated.suggested_path.clone(), generated.yaml.clone())
        .is_some()
        || files.len() != 2
    {
        return Err(StatusCode::CONFLICT.into_response());
    }
    Ok(ExactRepositoryBundle {
        files,
        workflow_path: generated.suggested_path,
        workflow_content: generated.yaml,
        package_digest: evidence.closure_digest.as_str().to_owned(),
    })
}

async fn owned_successful_browser_run<L>(
    ledger: &L,
    session: &ConnectionSession<BrowserSessionBinding>,
    task_uid: Uuid,
) -> Result<AgentRunRecord, Response>
where
    L: AgentRunLedger,
{
    match ledger.agent_run(task_uid).await {
        Ok(Some(run))
            if run.owner_user_id.as_deref() == Some(session.subject.canonical_user_id.as_str())
                && run.phase == TaskPhase::Succeeded
                && run.finalized
                && run.task_origin == TaskOrigin::Browser =>
        {
            Ok(run)
        }
        Ok(Some(_) | None) => Err(StatusCode::NOT_FOUND.into_response()),
        Err(_) => Err(unavailable()),
    }
}

fn repository_response(
    config: &TaskApiConfig,
    value: Value,
) -> Result<GithubRepositoriesResponse, ()> {
    let object = value.as_object().ok_or(())?;
    let login = object.get("login").and_then(Value::as_str).ok_or(())?;
    let page = object
        .get("page")
        .and_then(Value::as_u64)
        .and_then(|page| u32::try_from(page).ok())
        .ok_or(())?;
    let has_next_page = object
        .get("hasNextPage")
        .and_then(Value::as_bool)
        .ok_or(())?;
    let repositories = object
        .get("repositories")
        .and_then(Value::as_array)
        .ok_or(())?
        .iter()
        .map(|repository| repository_view(config, repository))
        .collect::<Result<Vec<_>, ()>>()?;
    Ok(GithubRepositoriesResponse {
        api_version: GITHUB_AUTOMATION_API_VERSION,
        login: login.to_owned(),
        repositories,
        page,
        has_next_page,
        source: Some(GithubRepositoriesSource::Connection),
    })
}

enum AdmittedRepositories {
    /// The source GitHub App or the source bindings are absent: use the governed listing.
    NotConfigured,
    /// The App resolved no admitted repository.
    Unavailable,
    Listed {
        repositories: Arc<Vec<GithubRepositoryView>>,
        unresolved: usize,
    },
}

async fn admitted_repositories(config: &GithubAutomationConfig) -> AdmittedRepositories {
    let cache = &config.admitted_repositories;
    if cache.key.is_empty() {
        return AdmittedRepositories::NotConfigured;
    }
    let mut refreshed = cache.refreshed.subscribe();
    let mut started = None;
    {
        let mut state = cache.lock();
        if state.unsupported {
            return AdmittedRepositories::NotConfigured;
        }
        if !state.refreshing
            && let Some(refresh) = state.refresh_due((cache.clock)())
        {
            let targets = match refresh {
                AdmittedRefresh::Full => cache.key.to_vec(),
                AdmittedRefresh::Unresolved => state.unresolved.clone(),
            };
            state.refreshing = true;
            // The guard exists before the task does, so the refresh completes even if the
            // task is dropped before it first runs.
            started = Some(AdmittedRefreshCompletion::new(
                cache.clone(),
                refresh,
                targets,
            ));
        }
        if started.is_none() && (!state.listing.is_empty() || !state.refreshing) {
            return state.listing();
        }
    }
    let usable = {
        if let Some(completion) = started {
            spawn_admitted_refresh(completion, config.task_api.clone());
        }
        let state = cache.lock();
        (!state.listing.is_empty() || !state.refreshing).then(|| state.listing())
    };
    if let Some(listing) = usable {
        return listing;
    }
    // No usable listing yet: wait, within a bound, for the in-flight refresh. Cancelling
    // this wait does not cancel the refresh, which still fills the cache.
    tokio::time::timeout(ADMITTED_REPOSITORIES_COLD_WAIT, async {
        loop {
            if refreshed.changed().await.is_err() {
                return AdmittedRepositories::Unavailable;
            }
            let state = cache.lock();
            if !state.refreshing {
                return state.listing();
            }
        }
    })
    .await
    .unwrap_or(AdmittedRepositories::Unavailable)
}

fn spawn_admitted_refresh(completion: AdmittedRefreshCompletion, task_api: TaskApiConfig) {
    tokio::spawn(async move {
        let targets = completion.targets();
        let outcome = resolve_admitted(&task_api, &targets).await;
        completion.finish(outcome);
    });
}

#[derive(Default)]
struct ResolutionOutcome {
    resolved: BTreeMap<GitRepositoryReference, GithubRepositoryView>,
    /// Repositories the App definitively rejected, or whose metadata failed validation.
    rejected: std::collections::BTreeSet<GitRepositoryReference>,
    /// Every requested repository was answered with `Unsupported`.
    unsupported: bool,
}

/// Resolves references through the source GitHub App, keeping each result as it completes
/// until one deadline.
async fn resolve_admitted(
    config: &TaskApiConfig,
    references: &[GitRepositoryReference],
) -> ResolutionOutcome {
    let deadline = tokio::time::Instant::now() + ADMITTED_REPOSITORIES_RESOLUTION_TIMEOUT;
    let mut resolved = BTreeMap::new();
    let mut rejected = std::collections::BTreeSet::new();
    let mut answered = vec![false; references.len()];
    let mut failures = Vec::new();
    let mut unsupported = 0_usize;
    if let Some(mut described) = config.describe_admitted_source_repositories(references) {
        while let Ok(Some((index, result))) =
            tokio::time::timeout_at(deadline, described.next()).await
        {
            let Some(reference) = references.get(index) else {
                continue;
            };
            if std::mem::replace(&mut answered[index], true) {
                continue;
            }
            let failure = match result {
                Ok(description) => match admitted_view(config, reference, &description) {
                    Some(view) => {
                        resolved.insert(reference.clone(), view);
                        continue;
                    }
                    None => {
                        rejected.insert(reference.clone());
                        "repository metadata failed listing validation".to_owned()
                    }
                },
                Err(steward_ports::PortError::Unsupported { operation }) => {
                    unsupported += 1;
                    operation.to_owned()
                }
                Err(steward_ports::PortError::Rejected { reason }) => {
                    rejected.insert(reference.clone());
                    reason
                }
                Err(error) => port_error_reason(&error).to_owned(),
            };
            failures.push((reference, failure));
        }
    }
    for (reference, answered) in references.iter().zip(&answered) {
        if !answered {
            failures.push((reference, "resolution timed out".to_owned()));
        }
    }
    let unsupported = !references.is_empty() && unsupported == references.len();
    if !failures.is_empty() && !unsupported {
        let examples = failures
            .iter()
            .take(ADMITTED_REPOSITORIES_REPORTED_FAILURES)
            .map(|(reference, reason)| {
                format!(
                    "ownerId={} repositoryId={} reason={reason}",
                    reference.repository_owner_id.as_str(),
                    reference.repository_id.as_str()
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        eprintln!(
            "admitted repository listing: {} of {} source repositories unresolved: {examples}",
            failures.len(),
            references.len()
        );
    }
    ResolutionOutcome {
        resolved,
        rejected,
        unsupported,
    }
}

fn port_error_reason(error: &steward_ports::PortError) -> &str {
    match error {
        steward_ports::PortError::Rejected { reason }
        | steward_ports::PortError::Failed { reason } => reason,
        steward_ports::PortError::Unsupported { operation } => operation,
        _ => "source GitHub App resolution failed",
    }
}

/// Applies the governed listing's per-field validation to one App-resolved repository.
fn admitted_view(
    config: &TaskApiConfig,
    reference: &GitRepositoryReference,
    description: &GitRepositoryDescription,
) -> Option<GithubRepositoryView> {
    if description.repository_owner_id != reference.repository_owner_id
        || description.repository_id != reference.repository_id
    {
        return None;
    }
    let value = json!({
        "owner": description.owner,
        "ownerId": description.repository_owner_id.as_str(),
        "name": description.name,
        "repositoryId": description.repository_id.as_str(),
        "defaultBranch": description.default_branch,
        "private": description.private,
        "url": description.web_url,
    });
    if !valid_repository_listing_entry(&value) {
        return None;
    }
    repository_view(config, &value)
        .ok()
        .filter(|view| view.ready)
}

fn admitted_page(
    repositories: Arc<Vec<GithubRepositoryView>>,
    unresolved: usize,
    page: u32,
    per_page: u32,
) -> Response {
    let per_page = per_page as usize;
    let start = (page as usize - 1).saturating_mul(per_page);
    let has_next_page = start.saturating_add(per_page) < repositories.len();
    let repositories = repositories
        .iter()
        .skip(start)
        .take(per_page)
        .cloned()
        .collect();
    let mut response = no_store_json(GithubRepositoriesResponse {
        api_version: GITHUB_AUTOMATION_API_VERSION,
        login: String::new(),
        repositories,
        page,
        has_next_page,
        source: Some(GithubRepositoriesSource::Admitted),
    });
    if unresolved > 0 {
        response.headers_mut().insert(
            UNRESOLVED_REPOSITORIES_HEADER,
            header::HeaderValue::from(unresolved),
        );
    }
    response
}

fn source_app_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::CACHE_CONTROL, "no-store")],
        Json(GithubAutomationErrorResponse {
            api_version: GITHUB_AUTOMATION_API_VERSION,
            error: "github_automation_unavailable",
            reason: Some("source_app_unavailable"),
            manual_files: None,
        }),
    )
        .into_response()
}

fn repository_view(config: &TaskApiConfig, value: &Value) -> Result<GithubRepositoryView, ()> {
    let object = value.as_object().ok_or(())?;
    let owner = string_field(object, "owner")?;
    let owner_id = string_field(object, "ownerId")?;
    let name = string_field(object, "name")?;
    let repository_id = string_field(object, "repositoryId")?;
    let ready = config.browser_source_repository_ids_are_authorized(&owner_id, &repository_id);
    Ok(GithubRepositoryView {
        owner,
        owner_id,
        name,
        repository_id,
        default_branch: string_field(object, "defaultBranch")?,
        private: object.get("private").and_then(Value::as_bool).ok_or(())?,
        url: string_field(object, "url")?,
        ready,
        missing_prerequisite: (!ready).then_some("source_repository_not_admitted"),
    })
}

fn workflow_response(value: Value) -> Result<WorkflowDetectionResponse, ()> {
    let object = value.as_object().ok_or(())?;
    Ok(WorkflowDetectionResponse {
        api_version: GITHUB_AUTOMATION_API_VERSION,
        path: string_field(object, "path")?,
        exists: object.get("exists").and_then(Value::as_bool).ok_or(())?,
        compatible: object
            .get("compatible")
            .and_then(Value::as_bool)
            .ok_or(())?,
        sha: object.get("sha").and_then(Value::as_str).map(str::to_owned),
    })
}

fn publish_response(value: Value, package_digest: String) -> Result<PublishTaskResponse, ()> {
    let object = value.as_object().ok_or(())?;
    Ok(PublishTaskResponse {
        api_version: GITHUB_AUTOMATION_API_VERSION,
        pull_request_url: string_field(object, "pullRequestUrl")?,
        pull_request_number: object
            .get("pullRequestNumber")
            .and_then(Value::as_u64)
            .ok_or(())?,
        branch: string_field(object, "branch")?,
        package_digest,
    })
}

fn dispatch_response(value: Value) -> Result<DispatchTaskResponse, ()> {
    let object = value.as_object().ok_or(())?;
    Ok(DispatchTaskResponse {
        api_version: GITHUB_AUTOMATION_API_VERSION,
        run_id: object.get("runId").and_then(Value::as_u64).ok_or(())?,
        url: string_field(object, "url")?,
    })
}

fn run_status_response(
    value: Value,
    linked: Option<(Uuid, TaskPhase)>,
) -> Result<GithubRunStatusResponse, ()> {
    let object = value.as_object().ok_or(())?;
    let jobs = object
        .get("jobs")
        .and_then(Value::as_array)
        .ok_or(())?
        .iter()
        .map(|job| {
            let job = job.as_object().ok_or(())?;
            Ok(GithubJobView {
                id: job.get("id").and_then(Value::as_u64).ok_or(())?,
                name: string_field(job, "name")?,
                status: string_field(job, "status")?,
                conclusion: job
                    .get("conclusion")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                url: string_field(job, "url")?,
            })
        })
        .collect::<Result<Vec<_>, ()>>()?;
    Ok(GithubRunStatusResponse {
        api_version: GITHUB_AUTOMATION_API_VERSION,
        run_id: object.get("runId").and_then(Value::as_u64).ok_or(())?,
        run_attempt: object
            .get("runAttempt")
            .and_then(Value::as_u64)
            .and_then(|attempt| u32::try_from(attempt).ok())
            .filter(|attempt| *attempt > 0)
            .ok_or(())?,
        phase: string_field(object, "phase")?,
        conclusion: object
            .get("conclusion")
            .and_then(Value::as_str)
            .map(str::to_owned),
        url: string_field(object, "url")?,
        jobs,
        failure_log: object
            .get("failureLog")
            .and_then(Value::as_str)
            .map(str::to_owned),
        linked_task_uid: linked.as_ref().map(|(task_uid, _)| *task_uid),
        linked_task_phase: linked.map(|(_, phase)| task_phase_name(phase).to_owned()),
    })
}

const fn task_phase_name(phase: TaskPhase) -> &'static str {
    match phase {
        TaskPhase::Submitted => "submitted",
        TaskPhase::Parked => "parked",
        TaskPhase::Queued => "queued",
        TaskPhase::Running => "running",
        TaskPhase::Succeeded => "succeeded",
        TaskPhase::Failed => "failed",
        TaskPhase::Cancelled => "cancelled",
    }
}

fn string_field(object: &serde_json::Map<String, Value>, field: &str) -> Result<String, ()> {
    object
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or(())
}

fn valid_repository_component(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && !value.starts_with(['.', '-'])
        && !value.ends_with('.')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn valid_input_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

fn valid_idempotency_key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
}

fn fresh_operation_identity(operation: &str) -> GithubAutomationIdentity {
    GithubAutomationIdentity::fresh(operation)
}

fn scoped_read_operation_identity(operation: &str, subject: &str) -> GithubAutomationIdentity {
    GithubAutomationIdentity {
        idempotency_identity: format!(
            "{}:read:{}",
            operation_subject(operation, subject),
            Uuid::new_v4()
        ),
        idempotency_scope: None,
        publication_subject: None,
    }
}

fn write_operation_identity(
    operation: &str,
    client_key: &str,
    payload: &Value,
    subject: Option<&str>,
) -> GithubAutomationIdentity {
    GithubAutomationIdentity::write(operation, client_key, payload, subject)
}

fn automation_subject(task_uid: Uuid, owner: &str, repository: &str) -> String {
    format!("{task_uid}:{owner}/{repository}")
}

fn operation_subject(operation: &str, subject: &str) -> String {
    let (task_uid, repository) = subject.split_once(':').unwrap_or((subject, subject));
    format!(
        "{}:subject:sha256:{:x}",
        task_operation_prefix(operation, task_uid),
        Sha256::digest(repository.as_bytes())
    )
}

fn task_operation_prefix(operation: &str, task_uid: impl std::fmt::Display) -> String {
    format!("github-{operation}:task:{task_uid}")
}

fn no_store_json<T: Serialize>(value: T) -> Response {
    (
        StatusCode::OK,
        [(header::CACHE_CONTROL, "no-store")],
        Json(value),
    )
        .into_response()
}

fn automation_error(error: ConnectionBrokerError) -> Response {
    let (status, reason) = match error {
        ConnectionBrokerError::OAuthFlowPending => (StatusCode::CONFLICT, None),
        ConnectionBrokerError::IdempotencyConflict => (StatusCode::UNPROCESSABLE_ENTITY, None),
        ConnectionBrokerError::RuntimeAuthenticationFailed
        | ConnectionBrokerError::ProxyPolicyDenied
        | ConnectionBrokerError::ProviderAuthorizationFailed => (StatusCode::FORBIDDEN, None),
        ConnectionBrokerError::BridgeContractInvalid => {
            (StatusCode::SERVICE_UNAVAILABLE, Some("bridge_contract"))
        }
        ConnectionBrokerError::ProviderResponseInvalid => (
            StatusCode::SERVICE_UNAVAILABLE,
            Some("bridge_response_contract"),
        ),
        ConnectionBrokerError::BridgeResultTooLarge => (
            StatusCode::SERVICE_UNAVAILABLE,
            Some("bridge_result_too_large"),
        ),
        _ => (StatusCode::SERVICE_UNAVAILABLE, None),
    };
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(GithubAutomationErrorResponse {
            api_version: GITHUB_AUTOMATION_API_VERSION,
            error: "github_automation_unavailable",
            reason,
            manual_files: None,
        }),
    )
        .into_response()
}

fn automation_problem(
    error: &'static str,
    manual_files: Option<BTreeMap<String, String>>,
) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::CACHE_CONTROL, "no-store")],
        Json(GithubAutomationErrorResponse {
            api_version: GITHUB_AUTOMATION_API_VERSION,
            error,
            reason: None,
            manual_files,
        }),
    )
        .into_response()
}

fn unavailable() -> Response {
    automation_error(ConnectionBrokerError::Unavailable)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use steward_adapter_mcp_gw::{GithubBridgeOperation, GithubBridgeRequest};
    use steward_ports::PortError;
    use steward_store::{
        AgentRunExecutionLog, AgentRunLogStream, AgentRunPage, AgentRunQuery, AgentRunRecord,
        AgentRunTimelineEvent, StoreError,
    };
    use steward_types::direct_package::{
        BrowserTaskEvidence, ClosureEntry, ClosureEntryKind, ContentDigest, PackageClosure,
        PromptSourceKind, RelativePath, StableProviderId, TaskOrigin, canonical_json_bytes,
    };
    use steward_types::{
        AgentRuntimeSpec, AgentType, Budget, Duration as TaskDuration, Email, ModelRef, Principal,
        RuntimeOwnership, TaskPhase,
    };
    use tower::ServiceExt;

    use super::*;
    use crate::browser_auth::{
        LocalFakeIdentity, browser_auth_router, local_fake_browser_auth_service,
    };

    const OWNER_USER_ID: &str = "usr_0123456789abcdef0123456789abcdef";
    const ORIGIN: &str = "http://127.0.0.1:33001";

    #[derive(Clone, Default)]
    struct FakeLedger {
        records: Arc<Mutex<Vec<AgentRunRecord>>>,
    }

    impl AgentRunLedger for FakeLedger {
        fn agent_runs<'a>(
            &'a self,
            _query: &'a AgentRunQuery,
        ) -> BoxFuture<'a, Result<AgentRunPage, StoreError>> {
            Box::pin(async { Err(StoreError::InvalidRunQuery) })
        }

        fn agent_run(
            &self,
            task_uid: Uuid,
        ) -> BoxFuture<'_, Result<Option<AgentRunRecord>, StoreError>> {
            Box::pin(async move {
                Ok(self
                    .records
                    .lock()
                    .map_err(|_| StoreError::InvalidRunQuery)?
                    .iter()
                    .find(|record| record.task_uid == task_uid)
                    .cloned())
            })
        }

        fn agent_run_phase_facets<'a>(
            &'a self,
            _query: &'a AgentRunQuery,
        ) -> BoxFuture<'a, Result<BTreeMap<String, u64>, StoreError>> {
            Box::pin(async { Err(StoreError::InvalidRunQuery) })
        }

        fn cancel_agent_run<'a>(
            &'a self,
            _task_uid: Uuid,
            _owner_user_id: &'a str,
        ) -> BoxFuture<'a, Result<Option<AgentRunRecord>, StoreError>> {
            Box::pin(async { Err(StoreError::InvalidTaskTransition) })
        }

        fn agent_run_timeline(
            &self,
            _task_uid: Uuid,
        ) -> BoxFuture<'_, Result<Option<Vec<AgentRunTimelineEvent>>, StoreError>> {
            Box::pin(async { Ok(None) })
        }

        fn agent_run_execution_log<'a>(
            &'a self,
            _task_uid: Uuid,
            _owner_user_id: Option<&'a str>,
            _stream: AgentRunLogStream,
        ) -> BoxFuture<'a, Result<Option<AgentRunExecutionLog>, StoreError>> {
            Box::pin(async { Ok(None) })
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct BrokerCall {
        operation: ConnectionOperationKind,
        request: Value,
        idempotency_identity: String,
    }

    #[derive(Clone, Default)]
    struct FakeBroker {
        calls: Arc<Mutex<Vec<BrokerCall>>>,
        no_repository_hits: bool,
    }

    impl GithubAutomationBroker<BrowserSessionBinding> for FakeBroker {
        fn execute<'a>(
            &'a self,
            _session: &'a ConnectionSession<BrowserSessionBinding>,
            operation: ConnectionOperationKind,
            request: Value,
            identity: &'a GithubAutomationIdentity,
        ) -> BoxFuture<'a, Result<Value, ConnectionBrokerError>> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .map_err(|_| ConnectionBrokerError::Unavailable)?
                    .push(BrokerCall {
                        operation,
                        request,
                        idempotency_identity: identity.idempotency_identity.clone(),
                    });
                match operation {
                    ConnectionOperationKind::Repositories if self.no_repository_hits => Ok(json!({
                        "login": "alice",
                        "repositories": [],
                        "page": 1,
                        "hasNextPage": false
                    })),
                    ConnectionOperationKind::Repositories => Ok(json!({
                        "login": "alice",
                        "repositories": [
                            {
                                "owner": "example-org",
                                "ownerId": "100",
                                "name": "agentic-ops",
                                "repositoryId": "200",
                                "defaultBranch": "main",
                                "private": true,
                                "url": "https://github.com/example-org/agentic-ops"
                            },
                            {
                                "owner": "example-org",
                                "ownerId": "100",
                                "name": "not-admitted",
                                "repositoryId": "201",
                                "defaultBranch": "main",
                                "private": true,
                                "url": "https://github.com/example-org/not-admitted"
                            }
                        ],
                        "page": 1,
                        "hasNextPage": false
                    })),
                    ConnectionOperationKind::Workflow => Ok(json!({
                        "path": ".github/workflows/hypershell-hello.yml",
                        "exists": true,
                        "compatible": true,
                        "sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    })),
                    ConnectionOperationKind::Publish => Ok(json!({
                        "pullRequestUrl": "https://github.com/example-org/agentic-ops/pull/42",
                        "pullRequestNumber": 42,
                        "branch": "steward/task-11111111111141118111111111111111"
                    })),
                    ConnectionOperationKind::Dispatch => Ok(json!({
                        "runId": 12345,
                        "url": "https://github.com/example-org/agentic-ops/actions/runs/12345"
                    })),
                    ConnectionOperationKind::RunStatus => Ok(json!({
                        "runId": 12345,
                        "runAttempt": 1,
                        "phase": "completed",
                        "conclusion": "success",
                        "url": "https://github.com/example-org/agentic-ops/actions/runs/12345",
                        "jobs": []
                    })),
                    _ => Err(ConnectionBrokerError::Unavailable),
                }
            })
        }

        fn evidence<'a>(
            &'a self,
            _canonical_user_id: &'a CanonicalUserId,
            operation: ConnectionOperationKind,
            idempotency_identity_prefix: &'a str,
        ) -> BoxFuture<'a, Result<Option<Value>, ConnectionBrokerError>> {
            Box::pin(async move {
                let calls = self
                    .calls
                    .lock()
                    .map_err(|_| ConnectionBrokerError::Unavailable)?;
                if !calls.iter().any(|call| {
                    call.operation == operation
                        && call
                            .idempotency_identity
                            .starts_with(idempotency_identity_prefix)
                }) {
                    return Ok(None);
                }
                match operation {
                    ConnectionOperationKind::Workflow => Ok(Some(json!({
                        "path": ".github/workflows/hypershell-hello.yml",
                        "exists": true,
                        "compatible": true,
                        "sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    }))),
                    ConnectionOperationKind::Publish => Ok(Some(json!({
                        "pullRequestUrl": "https://github.com/example-org/agentic-ops/pull/42",
                        "pullRequestNumber": 42,
                        "branch": "steward/task-11111111111141118111111111111111"
                    }))),
                    ConnectionOperationKind::Dispatch => Ok(Some(json!({
                        "runId": 12345,
                        "url": "https://github.com/example-org/agentic-ops/actions/runs/12345"
                    }))),
                    _ => Ok(None),
                }
            })
        }
    }

    fn browser_run(task_uid: Uuid, owner_user_id: &str) -> Result<AgentRunRecord, String> {
        let content = r#"{"schemaVersion":"steward.task-definition/v2","promptText":"Say hello."}"#;
        let entry_point = RelativePath::parse(".steward/tasks/hello/task-definition.json")?;
        let file_digest = ContentDigest::parse(format!(
            "steward:sha256:{:x}",
            Sha256::digest(content.as_bytes())
        ))?;
        let closure = PackageClosure {
            contract_version: "steward.package-closure/v1".to_owned(),
            entry_point: entry_point.clone(),
            entries: vec![ClosureEntry {
                kind: ClosureEntryKind::TaskDefinition,
                path: entry_point.clone(),
                digest: file_digest,
                size_bytes: u64::try_from(content.len()).map_err(|error| error.to_string())?,
            }],
        };
        let closure_digest = ContentDigest::parse(format!(
            "steward:sha256:{:x}",
            Sha256::digest(canonical_json_bytes(&closure)?)
        ))?;
        let evidence = BrowserTaskEvidence {
            source: "inline".to_owned(),
            revision: closure_digest.as_str().to_owned(),
            path: entry_point.clone(),
            closure: Some(closure),
            closure_digest,
            inline_files: Some(BTreeMap::from([(
                entry_point.as_str().to_owned(),
                content.to_owned(),
            )])),
            diagnostics: Default::default(),
            workspace: None,
            prompt_source: PromptSourceKind::Inline,
        };
        evidence.validate()?;
        Ok(AgentRunRecord {
            task_uid,
            submitter_service: "browser".to_owned(),
            acting_user: Some("alice@example.com".to_owned()),
            owner: "alice@example.com".to_owned(),
            owner_user_id: Some(owner_user_id.to_owned()),
            owner_display_email: Some("alice@example.com".to_owned()),
            workflow: "browser-task@1".to_owned(),
            workflow_name: None,
            workflow_version: None,
            workflow_digest: None,
            user_envelope_instance_id: Some("envelope-instance-1".to_owned()),
            user_envelope_revision: Some(4),
            user_envelope_digest: Some(format!("sha256:{}", "b".repeat(64))),
            coding_agent_runtime: "agent-v1".to_owned(),
            runtime_uid: Some(format!("runtime-{task_uid}")),
            runtime_ownership: RuntimeOwnership::Provisioned,
            phase: TaskPhase::Succeeded,
            runtime_spec: AgentRuntimeSpec {
                principal: Principal::Service {
                    name: "browser".to_owned(),
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
                ttl: TaskDuration("24h".to_owned()),
                runner: steward_types::RunnerRequirements::default(),
                bindings: None,
            },
            envelope_revision: Some(1),
            finalize_requested: true,
            finalized: true,
            failure_reason: None,
            created_at: "2026-08-17T00:00:00.000000Z".to_owned(),
            updated_at: "2026-08-17T00:01:00.000000Z".to_owned(),
            spend: None,
            history_partial: false,
            direct_task_evidence: None,
            task_origin: TaskOrigin::Browser,
            browser_task_evidence: Some(evidence),
        })
    }

    fn config() -> Result<GithubAutomationConfig, String> {
        let bindings = json!({
            "contractVersion": "steward.source-repository-bindings/v1",
            "bindings": [{
                "caller": {"ownerId": "300", "repositoryId": "400"},
                "source": {"ownerId": "100", "repositoryId": "200"}
            }]
        })
        .to_string();
        Ok(GithubAutomationConfig::new(
            TaskApiConfig::default().with_source_repository_bindings_json(Some(&bindings))?,
            StewardRunRelease {
                manifest_schema_version: 3,
                version: "0.8.0".to_owned(),
                workflow_repository: "example-org/steward-run".to_owned(),
                workflow_commit: "a".repeat(40),
                action_commit: "b".repeat(40),
                governed_job_container_image: None,
            },
            StewardRunWorkflowInstallationMode::Remote,
            true,
        ))
    }

    #[tokio::test]
    async fn unsupported_steward_run_release_returns_specific_reason_and_manual_package()
    -> Result<(), String> {
        let task_uid = Uuid::parse_str("11111111-1111-4111-8111-111111111111")
            .map_err(|error| error.to_string())?;
        let ledger = FakeLedger::default();
        ledger
            .records
            .lock()
            .map_err(|_| "lock records")?
            .push(browser_run(task_uid, OWNER_USER_ID)?);
        let mut unsupported = config()?;
        unsupported.steward_run_release.version = "0.7.9".to_owned();
        let (auth, session_cookie, _) = signed_in_cookie_and_csrf().await?;
        let app = protected_router(ledger, FakeBroker::default(), unsupported, auth);

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/app/api/v1/runs/{task_uid}/github/bundle"))
                    .header(header::COOKIE, session_cookie)
                    .body(Body::empty())
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), 64 * 1024)
            .await
            .map_err(|error| error.to_string())?;
        let body: Value = serde_json::from_slice(&body).map_err(|error| error.to_string())?;
        assert_eq!(body["error"], "steward_run_release_unsupported");
        assert!(
            body["manualFiles"][".steward/tasks/hello/task-definition.json"]
                .as_str()
                .is_some()
        );
        Ok(())
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

    async fn signed_in_cookie_and_csrf() -> Result<(BrowserAuthService, String, String), String> {
        let service = local_fake_browser_auth_service(ORIGIN, LocalFakeIdentity::User)?;
        let login = browser_auth_router(service.clone())
            .oneshot(
                Request::builder()
                    .uri("/admin/auth/login")
                    .body(Body::empty())
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
        let flow_cookie = cookie(&login, "steward-local-oidc-flow")?;
        let authorize = login
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or("login omitted redirect")?;
        let authorized = browser_auth_router(service.clone())
            .oneshot(
                Request::builder()
                    .uri(authorize)
                    .body(Body::empty())
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
        let callback = authorized
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or("authorize omitted callback")?;
        let callback = browser_auth_router(service.clone())
            .oneshot(
                Request::builder()
                    .uri(callback)
                    .header(header::COOKIE, flow_cookie)
                    .body(Body::empty())
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
        let session_cookie = cookie(&callback, "steward-local-session")?;
        let session = browser_auth_router(service.clone())
            .oneshot(
                Request::builder()
                    .uri("/admin/api/v1/session")
                    .header(header::COOKIE, &session_cookie)
                    .body(Body::empty())
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
        let body = to_bytes(session.into_body(), 64 * 1024)
            .await
            .map_err(|error| error.to_string())?;
        let value: Value = serde_json::from_slice(&body).map_err(|error| error.to_string())?;
        let csrf = value["csrf"]
            .as_str()
            .ok_or("session omitted csrf")?
            .to_owned();
        Ok((service, session_cookie, csrf))
    }

    fn mutation_request(
        path: String,
        cookie: &str,
        csrf: &str,
        body: Value,
    ) -> Result<Request<Body>, String> {
        Request::builder()
            .method("POST")
            .uri(path)
            .header(header::COOKIE, cookie)
            .header(header::ORIGIN, ORIGIN)
            .header("sec-fetch-site", "same-origin")
            .header("x-steward-csrf", csrf)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .map_err(|error| error.to_string())
    }

    #[test]
    fn request_scalars_are_narrowly_bounded() {
        assert!(valid_repository_component("example-org", 39));
        assert!(!valid_repository_component("../other", 39));
        assert!(valid_input_name("task-inputs"));
        assert!(!valid_input_name("TASK INPUTS"));
        assert!(valid_idempotency_key("publish-123"));
        assert!(!valid_idempotency_key("publish/123"));
    }

    #[tokio::test]
    async fn bridge_contract_failure_has_a_bounded_automation_reason() -> Result<(), String> {
        let response = automation_error(ConnectionBrokerError::BridgeContractInvalid);
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), 1024)
            .await
            .map_err(|error| format!("read automation failure body: {error}"))?;
        assert_eq!(
            serde_json::from_slice::<Value>(&body)
                .map_err(|error| format!("parse automation failure body: {error}"))?,
            json!({
                "apiVersion": GITHUB_AUTOMATION_API_VERSION,
                "error": "github_automation_unavailable",
                "reason": "bridge_contract"
            })
        );
        Ok(())
    }

    #[tokio::test]
    async fn bridge_result_bound_has_its_own_automation_reason() -> Result<(), String> {
        let response = automation_error(ConnectionBrokerError::BridgeResultTooLarge);
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), 1024)
            .await
            .map_err(|error| format!("read automation failure body: {error}"))?;
        assert_eq!(
            serde_json::from_slice::<Value>(&body)
                .map_err(|error| format!("parse automation failure body: {error}"))?["reason"],
            "bridge_result_too_large",
            "Steward's own result bound is not a provider response contract failure"
        );
        Ok(())
    }

    #[tokio::test]
    async fn repository_lookup_without_search_hits_is_not_found() -> Result<(), String> {
        let task_uid = Uuid::parse_str("11111111-1111-4111-8111-111111111111")
            .map_err(|error| error.to_string())?;
        let ledger = FakeLedger::default();
        ledger
            .records
            .lock()
            .map_err(|_| "lock records")?
            .push(browser_run(task_uid, OWNER_USER_ID)?);
        let broker = FakeBroker {
            no_repository_hits: true,
            ..FakeBroker::default()
        };
        let (auth, session_cookie, csrf) = signed_in_cookie_and_csrf().await?;
        let app = protected_router(ledger, broker.clone(), config()?, auth);
        let response = app
            .oneshot(mutation_request(
                format!("/app/api/v1/runs/{task_uid}/github/workflow"),
                &session_cookie,
                &csrf,
                json!({"owner": "example-org", "repository": "example-repo"}),
            )?)
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let lookup = broker
            .calls
            .lock()
            .map_err(|_| "lock broker calls")?
            .iter()
            .find(|call| call.operation == ConnectionOperationKind::Repositories)
            .map(|call| call.request.clone())
            .ok_or("repository lookup was not captured")?;
        assert_eq!(lookup["query"], "repo:example-org/example-repo");
        Ok(())
    }

    #[tokio::test]
    async fn bridge_response_contract_failure_has_a_bounded_automation_reason() -> Result<(), String>
    {
        let response = automation_error(ConnectionBrokerError::ProviderResponseInvalid);
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), 1024)
            .await
            .map_err(|error| format!("read automation failure body: {error}"))?;
        assert_eq!(
            serde_json::from_slice::<Value>(&body)
                .map_err(|error| format!("parse automation failure body: {error}"))?,
            json!({
                "apiVersion": GITHUB_AUTOMATION_API_VERSION,
                "error": "github_automation_unavailable",
                "reason": "bridge_response_contract"
            })
        );
        Ok(())
    }

    #[test]
    fn write_idempotency_binds_one_client_key_to_one_semantic_payload() {
        let first_read = fresh_operation_identity("repositories");
        let second_read = fresh_operation_identity("repositories");
        assert_ne!(
            first_read.idempotency_identity,
            second_read.idempotency_identity
        );
        assert_eq!(first_read.idempotency_scope, None);
        assert_eq!(second_read.idempotency_scope, None);

        let first = write_operation_identity(
            "dispatch",
            "client-key",
            &json!({"inputs": {"message": "first"}}),
            None,
        );
        let retry = write_operation_identity(
            "dispatch",
            "client-key",
            &json!({"inputs": {"message": "first"}}),
            None,
        );
        let conflict = write_operation_identity(
            "dispatch",
            "client-key",
            &json!({"inputs": {"message": "other"}}),
            None,
        );
        assert_eq!(first, retry);
        assert_eq!(first.idempotency_scope, conflict.idempotency_scope);
        assert_ne!(first.idempotency_identity, conflict.idempotency_identity);

        let subject = "11111111-1111-4111-8111-111111111111:example-org/agentic-ops";
        let scoped = write_operation_identity(
            "dispatch",
            "client-key",
            &json!({"inputs": {}}),
            Some(subject),
        );
        assert!(scoped.idempotency_identity.starts_with(&format!(
            "{}:client:",
            operation_subject("dispatch", subject)
        )));
        assert_eq!(scoped.publication_subject, None);
    }

    #[tokio::test]
    async fn publication_is_owner_scoped_exact_and_idempotent_for_the_task_and_repository()
    -> Result<(), String> {
        let task_uid = Uuid::parse_str("11111111-1111-4111-8111-111111111111")
            .map_err(|error| error.to_string())?;
        let hidden_task = Uuid::parse_str("22222222-2222-4222-8222-222222222222")
            .map_err(|error| error.to_string())?;
        let source = browser_run(task_uid, OWNER_USER_ID)?;
        let expected_digest = source
            .browser_task_evidence
            .as_ref()
            .ok_or("missing browser evidence")?
            .closure_digest
            .as_str()
            .to_owned();
        let ledger = FakeLedger::default();
        ledger.records.lock().map_err(|_| "lock records")?.extend([
            source,
            browser_run(hidden_task, "usr_abcdefabcdefabcdefabcdefabcdefab")?,
        ]);
        let broker = FakeBroker::default();
        let (auth, session_cookie, csrf) = signed_in_cookie_and_csrf().await?;
        let app = protected_router(ledger, broker.clone(), config()?, auth);

        let bundle = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/app/api/v1/runs/{task_uid}/github/bundle"))
                    .header(header::COOKIE, &session_cookie)
                    .body(Body::empty())
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(bundle.status(), StatusCode::OK);
        let bundle = to_bytes(bundle.into_body(), 64 * 1024)
            .await
            .map_err(|error| error.to_string())?;
        let bundle: Value = serde_json::from_slice(&bundle).map_err(|error| error.to_string())?;
        assert_eq!(
            bundle["workflowPath"],
            ".github/workflows/hypershell-hello.yml"
        );
        assert_eq!(bundle["packageDigest"], expected_digest);
        assert!(
            bundle["files"][".github/workflows/hypershell-hello.yml"]
                .as_str()
                .is_some_and(|workflow| workflow
                    .contains("package-path: .steward/tasks/hello/task-definition.json"))
        );

        let repositories = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/app/api/v1/github/repositories?perPage=100")
                    .header(header::COOKIE, &session_cookie)
                    .body(Body::empty())
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(repositories.status(), StatusCode::OK);
        let repositories = to_bytes(repositories.into_body(), 64 * 1024)
            .await
            .map_err(|error| error.to_string())?;
        let repositories: Value =
            serde_json::from_slice(&repositories).map_err(|error| error.to_string())?;
        assert_eq!(repositories["repositories"][0]["ready"], true);
        assert_eq!(repositories["repositories"][1]["ready"], false);
        assert_eq!(
            repositories["repositories"][1]["missingPrerequisite"],
            "source_repository_not_admitted"
        );
        let default_repository_request = broker
            .calls
            .lock()
            .map_err(|_| "lock broker calls")?
            .iter()
            .find(|call| call.operation == ConnectionOperationKind::Repositories)
            .map(|call| call.request.clone())
            .ok_or("default repository request was not captured")?;
        assert_eq!(default_repository_request["query"], "");
        GithubBridgeRequest::parse(
            GithubBridgeOperation::Repositories,
            &serde_json::to_vec(&default_repository_request)
                .map_err(|error| format!("encode repository request: {error}"))?,
        )
        .map_err(|error| {
            format!("apiserver repository payload violates bridge contract: {error:?}")
        })?;

        for retry_key in ["first", "second"] {
            let response = app
                .clone()
                .oneshot(mutation_request(
                    format!("/app/api/v1/runs/{task_uid}/github/publish"),
                    &session_cookie,
                    &csrf,
                    json!({
                        "owner": "example-org",
                        "repository": "agentic-ops",
                        "idempotencyKey": retry_key
                    }),
                )?)
                .await
                .map_err(|error| error.to_string())?;
            assert_eq!(response.status(), StatusCode::OK);
            let body = to_bytes(response.into_body(), 64 * 1024)
                .await
                .map_err(|error| error.to_string())?;
            let body: Value = serde_json::from_slice(&body).map_err(|error| error.to_string())?;
            assert_eq!(body["packageDigest"], expected_digest);
        }

        let evidence = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/app/api/v1/runs/{task_uid}/github/evidence?owner=example-org&repository=agentic-ops"
                    ))
                    .header(header::COOKIE, &session_cookie)
                    .body(Body::empty())
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(evidence.status(), StatusCode::OK);
        let evidence = to_bytes(evidence.into_body(), 64 * 1024)
            .await
            .map_err(|error| error.to_string())?;
        let evidence: Value =
            serde_json::from_slice(&evidence).map_err(|error| error.to_string())?;
        assert_eq!(evidence["publication"]["pullRequestNumber"], 42);
        assert_eq!(evidence["publication"]["packageDigest"], expected_digest);
        assert!(evidence.get("dispatch").is_none());

        let dispatch = app
            .clone()
            .oneshot(mutation_request(
                format!("/app/api/v1/runs/{task_uid}/github/dispatch"),
                &session_cookie,
                &csrf,
                json!({
                    "owner": "example-org",
                    "repository": "agentic-ops",
                    "inputs": {"task-inputs": "{}"},
                    "idempotencyKey": "dispatch"
                }),
            )?)
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(dispatch.status(), StatusCode::OK);

        let evidence = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/app/api/v1/runs/{task_uid}/github/evidence?owner=example-org&repository=agentic-ops"
                    ))
                    .header(header::COOKIE, &session_cookie)
                    .body(Body::empty())
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
        let evidence = to_bytes(evidence.into_body(), 64 * 1024)
            .await
            .map_err(|error| error.to_string())?;
        let evidence: Value =
            serde_json::from_slice(&evidence).map_err(|error| error.to_string())?;
        assert_eq!(evidence["dispatch"]["runId"], 12345);

        let workflow = app
            .clone()
            .oneshot(mutation_request(
                format!("/app/api/v1/runs/{task_uid}/github/workflow"),
                &session_cookie,
                &csrf,
                json!({
                    "owner": "example-org",
                    "repository": "agentic-ops"
                }),
            )?)
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(workflow.status(), StatusCode::OK);

        let call_count = broker.calls.lock().map_err(|_| "lock broker calls")?.len();
        let onboarding = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/app/api/v1/runs/{task_uid}/github/onboarding"))
                    .header(header::COOKIE, &session_cookie)
                    .body(Body::empty())
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(onboarding.status(), StatusCode::OK);
        let onboarding = to_bytes(onboarding.into_body(), 64 * 1024)
            .await
            .map_err(|error| error.to_string())?;
        let onboarding: Value =
            serde_json::from_slice(&onboarding).map_err(|error| error.to_string())?;
        assert_eq!(onboarding["publicationObserved"], true);
        assert_eq!(onboarding["workflowObserved"], true);
        assert_eq!(onboarding["dispatchObserved"], true);
        assert_eq!(
            broker.calls.lock().map_err(|_| "lock broker calls")?.len(),
            call_count,
            "persisted onboarding evidence must not execute a governed GitHub operation"
        );

        {
            let calls = broker.calls.lock().map_err(|_| "lock broker calls")?;
            let publications = calls
                .iter()
                .filter(|call| call.operation == ConnectionOperationKind::Publish)
                .collect::<Vec<_>>();
            assert_eq!(publications.len(), 2);
            assert_ne!(
                publications[0].idempotency_identity, publications[1].idempotency_identity,
                "distinct client keys must retain distinct operation identities"
            );
            assert_eq!(
                publications[0]
                    .idempotency_identity
                    .split(":client:")
                    .next(),
                publications[1]
                    .idempotency_identity
                    .split(":client:")
                    .next(),
                "all retries of one Task and repository must share the publication subject"
            );
            let files = publications[0].request["files"]
                .as_array()
                .ok_or("publish request omitted files")?;
            assert_eq!(files.len(), 2);
            assert!(
                files
                    .iter()
                    .any(|file| file["path"] == ".steward/tasks/hello/task-definition.json")
            );
            assert!(
                files
                    .iter()
                    .any(|file| file["path"] == ".github/workflows/hypershell-hello.yml")
            );
            assert_eq!(publications[0].request["baseBranch"], "main");
            assert_ne!(publications[0].request["branch"], "main");
        }

        let hidden = app
            .oneshot(mutation_request(
                format!("/app/api/v1/runs/{hidden_task}/github/publish"),
                &session_cookie,
                &csrf,
                json!({
                    "owner": "example-org",
                    "repository": "agentic-ops",
                    "idempotencyKey": "hidden"
                }),
            )?)
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(hidden.status(), StatusCode::NOT_FOUND);
        Ok(())
    }

    type FakeResult = Result<GitRepositoryDescription, PortError>;

    #[derive(Clone, Default)]
    struct FakeSourceApp {
        calls: Arc<Mutex<Vec<Vec<GitRepositoryReference>>>>,
        results: Arc<Mutex<BTreeMap<String, FakeResult>>>,
        /// When set, each call waits for one permit before answering.
        gate: Arc<Mutex<Option<Arc<tokio::sync::Semaphore>>>>,
        /// Repository IDs whose resolution never completes.
        stalled: Arc<Mutex<std::collections::BTreeSet<String>>>,
    }

    impl FakeSourceApp {
        fn resolving(descriptions: Vec<GitRepositoryDescription>) -> Self {
            let app = Self::default();
            if let Ok(mut results) = app.results.lock() {
                for description in descriptions {
                    results.insert(
                        description.repository_id.as_str().to_owned(),
                        Ok(description),
                    );
                }
            }
            app
        }

        fn set(&self, repository_id: &str, result: FakeResult) -> Result<(), String> {
            self.results
                .lock()
                .map_err(|_| "lock source App results")?
                .insert(repository_id.to_owned(), result);
            Ok(())
        }

        fn fail(&self, repository_id: &str) -> Result<(), String> {
            self.set(
                repository_id,
                Err(PortError::Failed {
                    reason: "fixture source App failure".to_owned(),
                }),
            )
        }

        fn close_gate(&self) -> Result<Arc<tokio::sync::Semaphore>, String> {
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            *self.gate.lock().map_err(|_| "lock source App gate")? = Some(Arc::clone(&gate));
            Ok(gate)
        }

        fn call_count(&self) -> Result<usize, String> {
            Ok(self
                .calls
                .lock()
                .map_err(|_| "lock source App calls")?
                .len())
        }

        fn call(&self, index: usize) -> Result<Vec<String>, String> {
            Ok(self
                .calls
                .lock()
                .map_err(|_| "lock source App calls")?
                .get(index)
                .ok_or("source App call is absent")?
                .iter()
                .map(|reference| reference.repository_id.as_str().to_owned())
                .collect())
        }
    }

    impl steward_ports::GitHostingPlane for FakeSourceApp {
        fn describe_repositories<'a>(
            &'a self,
            repositories: &'a [GitRepositoryReference],
        ) -> impl futures::Stream<Item = (usize, FakeResult)> + Send + 'a {
            futures::stream::once(async move {
                if let Ok(mut calls) = self.calls.lock() {
                    calls.push(repositories.to_vec());
                }
                let gate = self.gate.lock().ok().and_then(|gate| gate.clone());
                if let Some(gate) = gate
                    && let Ok(permit) = gate.acquire().await
                {
                    permit.forget();
                }
                let results = self
                    .results
                    .lock()
                    .map(|results| results.clone())
                    .unwrap_or_default();
                let stalled = self
                    .stalled
                    .lock()
                    .map(|stalled| stalled.clone())
                    .unwrap_or_default();
                let answers = repositories
                    .iter()
                    .enumerate()
                    .map(|(index, reference)| {
                        let id = reference.repository_id.as_str();
                        let result = results.get(id).cloned().unwrap_or(Err(PortError::Rejected {
                            reason: "fixture repository is unknown".to_owned(),
                        }));
                        let stall = stalled.contains(id);
                        async move {
                            if stall {
                                std::future::pending::<()>().await;
                            }
                            (index, result)
                        }
                    })
                    .collect::<Vec<_>>();
                futures::stream::iter(answers).buffer_unordered(64)
            })
            .flatten()
        }

        async fn resolve_repository(
            &self,
            _repository: &steward_types::direct_package::RepositoryUrl,
        ) -> Result<steward_ports::GitRepositoryIdentity, PortError> {
            Err(PortError::Unsupported {
                operation: "resolve_repository",
            })
        }

        async fn read_file(
            &self,
            _request: &steward_ports::GitFileRequest,
        ) -> Result<steward_ports::GitFile, PortError> {
            Err(PortError::Unsupported {
                operation: "read_file",
            })
        }

        async fn resolve_revision(
            &self,
            _request: &steward_ports::GitRevisionRequest,
        ) -> Result<steward_types::direct_package::ExactGitCommit, PortError> {
            Err(PortError::Unsupported {
                operation: "resolve_revision",
            })
        }
    }

    /// A hosting plane that keeps the port's default, unsupported repository description.
    struct DescriptionlessPlane;

    impl steward_ports::GitHostingPlane for DescriptionlessPlane {
        async fn resolve_repository(
            &self,
            _repository: &steward_types::direct_package::RepositoryUrl,
        ) -> Result<steward_ports::GitRepositoryIdentity, PortError> {
            Err(PortError::Unsupported {
                operation: "resolve_repository",
            })
        }

        async fn read_file(
            &self,
            _request: &steward_ports::GitFileRequest,
        ) -> Result<steward_ports::GitFile, PortError> {
            Err(PortError::Unsupported {
                operation: "read_file",
            })
        }

        async fn resolve_revision(
            &self,
            _request: &steward_ports::GitRevisionRequest,
        ) -> Result<steward_types::direct_package::ExactGitCommit, PortError> {
            Err(PortError::Unsupported {
                operation: "resolve_revision",
            })
        }
    }

    /// Waits until no admitted-repository refresh is in flight.
    async fn refresh_idle(cache: &AdmittedRepositoryCache) -> Result<(), String> {
        for _ in 0..10_000 {
            if !cache.lock().refreshing {
                return Ok(());
            }
            tokio::task::yield_now().await;
        }
        Err("admitted repository refresh did not finish".to_owned())
    }

    /// Waits until the source App has received `count` calls.
    async fn app_calls(source_app: &FakeSourceApp, count: usize) -> Result<(), String> {
        for _ in 0..10_000 {
            if source_app.call_count()? >= count {
                return Ok(());
            }
            tokio::task::yield_now().await;
        }
        Err(format!("source App did not receive {count} calls"))
    }

    fn set_clock(
        offset: &Mutex<std::time::Duration>,
        value: std::time::Duration,
    ) -> Result<(), String> {
        *offset.lock().map_err(|_| "lock clock")? = value;
        Ok(())
    }

    fn listed_names(body: &Value) -> Vec<String> {
        body["repositories"]
            .as_array()
            .map(|repositories| {
                repositories
                    .iter()
                    .filter_map(|repository| repository["name"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn description(
        owner: &str,
        owner_id: &str,
        name: &str,
        repository_id: &str,
    ) -> Result<GitRepositoryDescription, String> {
        Ok(GitRepositoryDescription {
            owner: owner.to_owned(),
            repository_owner_id: StableProviderId::parse(owner_id)?,
            name: name.to_owned(),
            repository_id: StableProviderId::parse(repository_id)?,
            default_branch: "main".to_owned(),
            private: true,
            web_url: format!("https://github.com/{owner}/{name}"),
        })
    }

    /// Source bindings (ownerId, repositoryId) admitted for two callers each, so the
    /// listing must deduplicate them.
    fn admitted_config(
        app: FakeSourceApp,
        sources: &[(&str, &str)],
        clock: AdmittedRepositoryClock,
    ) -> Result<GithubAutomationConfig, String> {
        let bindings = sources
            .iter()
            .flat_map(|(owner_id, repository_id)| {
                ["400", "401"].map(|caller| {
                    json!({
                        "caller": {"ownerId": "300", "repositoryId": caller},
                        "source": {"ownerId": owner_id, "repositoryId": repository_id}
                    })
                })
            })
            .collect::<Vec<_>>();
        let bindings = json!({
            "contractVersion": "steward.source-repository-bindings/v1",
            "bindings": bindings
        })
        .to_string();
        let config = config()?;
        let config = config.with_task_api(
            TaskApiConfig::default()
                .with_source_repository_bindings_json(Some(&bindings))?
                .with_git_hosting_plane(app),
        );
        Ok(config.with_admitted_repository_clock(clock))
    }

    fn fixed_clock() -> (AdmittedRepositoryClock, Arc<Mutex<std::time::Duration>>) {
        let base = Instant::now();
        let offset = Arc::new(Mutex::new(std::time::Duration::ZERO));
        let shared = Arc::clone(&offset);
        let clock: AdmittedRepositoryClock =
            Arc::new(move || base + shared.lock().map(|offset| *offset).unwrap_or_default());
        (clock, offset)
    }

    async fn list(
        app: &Router,
        session_cookie: &str,
        query: &str,
    ) -> Result<(StatusCode, Option<String>, Value), String> {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/app/api/v1/github/repositories{query}"))
                    .header(header::COOKIE, session_cookie)
                    .body(Body::empty())
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status();
        let unresolved = response
            .headers()
            .get(UNRESOLVED_REPOSITORIES_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = to_bytes(response.into_body(), 256 * 1024)
            .await
            .map_err(|error| error.to_string())?;
        let body = serde_json::from_slice(&body).unwrap_or(Value::Null);
        Ok((status, unresolved, body))
    }

    fn governed_calls(broker: &FakeBroker) -> Result<usize, String> {
        Ok(broker.calls.lock().map_err(|_| "lock broker calls")?.len())
    }

    async fn admitted_app(
        source_app: &FakeSourceApp,
        sources: &[(&str, &str)],
    ) -> Result<
        (
            Router,
            String,
            AdmittedRepositoryCache,
            Arc<Mutex<std::time::Duration>>,
            FakeBroker,
        ),
        String,
    > {
        let (clock, offset) = fixed_clock();
        let config = admitted_config(source_app.clone(), sources, clock)?;
        let cache = config.admitted_repositories.clone();
        let broker = FakeBroker::default();
        let (auth, session_cookie, _) = signed_in_cookie_and_csrf().await?;
        let app = protected_router(FakeLedger::default(), broker.clone(), config, auth);
        Ok((app, session_cookie, cache, offset, broker))
    }

    #[tokio::test]
    async fn blank_listing_returns_admitted_repositories_without_a_governed_operation()
    -> Result<(), String> {
        let source_app = FakeSourceApp::resolving(vec![
            description("example-org", "100", "zeta-service", "200")?,
            description("acme", "500", "service-a", "600")?,
        ]);
        let (app, cookie, _, _, broker) =
            admitted_app(&source_app, &[("100", "200"), ("500", "600")]).await?;

        let (status, unresolved, body) = list(&app, &cookie, "?perPage=100").await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(unresolved, None);
        assert_eq!(body["source"], "admitted");
        assert_eq!(body["login"], "");
        assert_eq!(body["page"], 1);
        assert_eq!(body["hasNextPage"], false);
        assert_eq!(
            body["repositories"],
            json!([
                {
                    "owner": "acme",
                    "ownerId": "500",
                    "name": "service-a",
                    "repositoryId": "600",
                    "defaultBranch": "main",
                    "private": true,
                    "url": "https://github.com/acme/service-a",
                    "ready": true
                },
                {
                    "owner": "example-org",
                    "ownerId": "100",
                    "name": "zeta-service",
                    "repositoryId": "200",
                    "defaultBranch": "main",
                    "private": true,
                    "url": "https://github.com/example-org/zeta-service",
                    "ready": true
                }
            ])
        );
        assert_eq!(governed_calls(&broker)?, 0, "no governed operation runs");
        assert_eq!(
            source_app.call(0)?,
            ["200", "600"],
            "each distinct admitted source is resolved once"
        );
        assert_eq!(source_app.call_count()?, 1);
        Ok(())
    }

    #[tokio::test]
    async fn expired_listing_is_served_stale_while_one_refresh_runs() -> Result<(), String> {
        let source_app =
            FakeSourceApp::resolving(vec![description("example-org", "100", "service-a", "200")?]);
        let (app, cookie, cache, offset, _) = admitted_app(&source_app, &[("100", "200")]).await?;

        for _ in 0..3 {
            let (status, _, body) = list(&app, &cookie, "").await?;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(listed_names(&body), ["service-a"]);
        }
        assert_eq!(
            source_app.call_count()?,
            1,
            "a warm cache makes no App call"
        );
        set_clock(
            &offset,
            ADMITTED_REPOSITORIES_TTL - std::time::Duration::from_secs(1),
        )?;
        list(&app, &cookie, "").await?;
        assert_eq!(source_app.call_count()?, 1, "the entry is still fresh");

        let gate = source_app.close_gate()?;
        source_app.set(
            "200",
            Ok(description("example-org", "100", "service-renamed", "200")?),
        )?;
        set_clock(&offset, ADMITTED_REPOSITORIES_TTL)?;
        for _ in 0..2 {
            let (status, _, body) =
                tokio::time::timeout(std::time::Duration::from_secs(5), list(&app, &cookie, ""))
                    .await
                    .map_err(|_| "a stale listing must not wait for the refresh")??;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(listed_names(&body), ["service-a"], "served stale");
        }
        app_calls(&source_app, 2).await?;
        list(&app, &cookie, "").await?;
        assert_eq!(
            source_app.call_count()?,
            2,
            "exactly one refresh is in flight"
        );

        gate.add_permits(1);
        refresh_idle(&cache).await?;
        let (_, _, body) = list(&app, &cookie, "").await?;
        assert_eq!(
            listed_names(&body),
            ["service-renamed"],
            "the refresh landed"
        );
        assert_eq!(source_app.call_count()?, 2);
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_cold_request_still_fills_the_cache() -> Result<(), String> {
        let source_app =
            FakeSourceApp::resolving(vec![description("example-org", "100", "service-a", "200")?]);
        let gate = source_app.close_gate()?;
        let (app, cookie, cache, _, _) = admitted_app(&source_app, &[("100", "200")]).await?;

        let cancelled = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            list(&app, &cookie, ""),
        )
        .await;
        assert!(
            cancelled.is_err(),
            "the cold request waits and is then cancelled"
        );
        assert_eq!(source_app.call_count()?, 1);

        gate.add_permits(1);
        refresh_idle(&cache).await?;
        let (status, _, body) = list(&app, &cookie, "").await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(listed_names(&body), ["service-a"]);
        assert_eq!(
            source_app.call_count()?,
            1,
            "the detached refresh filled the cache"
        );
        Ok(())
    }

    #[tokio::test]
    async fn partial_resolution_lists_resolved_repositories_with_a_bounded_count()
    -> Result<(), String> {
        let mut oversized = description("example-org", "100", "oversized", "203")?;
        oversized.name = "a".repeat(101);
        let mut foreign_url = description("example-org", "100", "foreign", "204")?;
        foreign_url.web_url = "https://elsewhere.example.com/example-org/foreign".to_owned();
        let mismatched = description("example-org", "999", "mismatched", "205")?;
        let source_app = FakeSourceApp::resolving(vec![
            description("example-org", "100", "service-a", "200")?,
            oversized,
            foreign_url,
            mismatched,
        ]);
        source_app.fail("201")?;
        let (app, cookie, cache, offset, broker) = admitted_app(
            &source_app,
            &[
                ("100", "200"),
                ("100", "201"),
                ("100", "202"),
                ("100", "203"),
                ("100", "204"),
                ("100", "205"),
            ],
        )
        .await?;

        let (status, unresolved, body) = list(&app, &cookie, "").await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["source"], "admitted");
        assert_eq!(listed_names(&body), ["service-a"]);
        assert_eq!(
            unresolved.as_deref(),
            Some("5"),
            "failed, unknown, oversized, foreign-URL and mismatched entries are unresolved"
        );
        assert_eq!(governed_calls(&broker)?, 0);

        set_clock(
            &offset,
            ADMITTED_REPOSITORIES_PARTIAL_TTL - std::time::Duration::from_secs(1),
        )?;
        list(&app, &cookie, "").await?;
        assert_eq!(
            source_app.call_count()?,
            1,
            "the partial listing is still served"
        );

        // The retry is in flight while a request is cancelled; nothing unresolved is lost.
        let gate = source_app.close_gate()?;
        source_app.set(
            "201",
            Ok(description("example-org", "100", "recovered", "201")?),
        )?;
        set_clock(&offset, ADMITTED_REPOSITORIES_PARTIAL_TTL)?;
        let (status, unresolved, _) = list(&app, &cookie, "").await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            unresolved.as_deref(),
            Some("5"),
            "the unresolved set is kept during a retry"
        );
        assert_eq!(cache.lock().unresolved.len(), 5);
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(10),
            list(&app, &cookie, ""),
        )
        .await;
        app_calls(&source_app, 2).await?;
        assert_eq!(
            cache.lock().unresolved.len(),
            5,
            "an in-flight retry keeps them"
        );
        assert_eq!(source_app.call(1)?, ["201", "202", "203", "204", "205"]);

        gate.add_permits(1);
        refresh_idle(&cache).await?;
        let (_, unresolved, body) = list(&app, &cookie, "").await?;
        assert_eq!(unresolved.as_deref(), Some("4"));
        assert_eq!(listed_names(&body), ["recovered", "service-a"]);
        assert_eq!(
            source_app.call_count()?,
            2,
            "only the unresolved ones were retried"
        );
        Ok(())
    }

    #[tokio::test]
    async fn failed_refresh_keeps_the_previous_listing_and_counts_it_unresolved()
    -> Result<(), String> {
        let source_app = FakeSourceApp::resolving(vec![
            description("example-org", "100", "repo-a", "200")?,
            description("example-org", "100", "repo-b", "201")?,
        ]);
        let (app, cookie, cache, offset, _) =
            admitted_app(&source_app, &[("100", "200"), ("100", "201")]).await?;
        let (_, _, body) = list(&app, &cookie, "").await?;
        assert_eq!(listed_names(&body), ["repo-a", "repo-b"]);

        source_app.fail("200")?;
        source_app.fail("201")?;
        set_clock(&offset, ADMITTED_REPOSITORIES_TTL)?;
        list(&app, &cookie, "").await?;
        app_calls(&source_app, 2).await?;
        refresh_idle(&cache).await?;
        let (status, unresolved, body) = list(&app, &cookie, "").await?;
        assert_eq!(
            status,
            StatusCode::OK,
            "a failed refresh never becomes a 503"
        );
        assert_eq!(listed_names(&body), ["repo-a", "repo-b"]);
        assert_eq!(
            unresolved.as_deref(),
            Some("2"),
            "retained entries that failed count as unresolved"
        );

        source_app.set(
            "200",
            Ok(description("example-org", "100", "repo-a-renamed", "200")?),
        )?;
        set_clock(
            &offset,
            ADMITTED_REPOSITORIES_TTL + ADMITTED_REPOSITORIES_PARTIAL_TTL,
        )?;
        list(&app, &cookie, "").await?;
        app_calls(&source_app, 3).await?;
        refresh_idle(&cache).await?;
        let (_, unresolved, body) = list(&app, &cookie, "").await?;
        assert_eq!(
            listed_names(&body),
            ["repo-a-renamed", "repo-b"],
            "a later refresh merges into the retained listing"
        );
        assert_eq!(unresolved.as_deref(), Some("1"));
        Ok(())
    }

    #[tokio::test]
    async fn retained_entries_expire_after_the_maximum_age() -> Result<(), String> {
        let source_app = FakeSourceApp::resolving(vec![
            description("example-org", "100", "repo-a", "200")?,
            description("example-org", "100", "repo-b", "201")?,
        ]);
        let (app, cookie, cache, offset, _) =
            admitted_app(&source_app, &[("100", "200"), ("100", "201")]).await?;
        list(&app, &cookie, "").await?;

        source_app.fail("201")?;
        set_clock(&offset, ADMITTED_REPOSITORIES_TTL)?;
        list(&app, &cookie, "").await?;
        app_calls(&source_app, 2).await?;
        refresh_idle(&cache).await?;
        let (_, unresolved, body) = list(&app, &cookie, "").await?;
        assert_eq!(
            listed_names(&body),
            ["repo-a", "repo-b"],
            "repo-b is retained"
        );
        assert_eq!(unresolved.as_deref(), Some("1"));

        set_clock(
            &offset,
            ADMITTED_REPOSITORIES_MAX_AGE + std::time::Duration::from_secs(1),
        )?;
        list(&app, &cookie, "").await?;
        app_calls(&source_app, 3).await?;
        refresh_idle(&cache).await?;
        let (status, unresolved, body) = list(&app, &cookie, "").await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            listed_names(&body),
            ["repo-a"],
            "an entry failing past the maximum age is no longer listed"
        );
        assert_eq!(unresolved.as_deref(), Some("1"));
        Ok(())
    }

    #[tokio::test]
    async fn definitive_rejection_evicts_a_listed_repository() -> Result<(), String> {
        let source_app = FakeSourceApp::resolving(vec![
            description("example-org", "100", "repo-a", "200")?,
            description("example-org", "100", "repo-b", "201")?,
        ]);
        let (app, cookie, cache, offset, _) =
            admitted_app(&source_app, &[("100", "200"), ("100", "201")]).await?;
        list(&app, &cookie, "").await?;

        source_app.set(
            "201",
            Err(PortError::Rejected {
                reason: "GitHub App is not installed for the repository owner".to_owned(),
            }),
        )?;
        set_clock(&offset, ADMITTED_REPOSITORIES_TTL)?;
        list(&app, &cookie, "").await?;
        app_calls(&source_app, 2).await?;
        refresh_idle(&cache).await?;
        let (_, unresolved, body) = list(&app, &cookie, "").await?;
        assert_eq!(
            listed_names(&body),
            ["repo-a"],
            "a rejected repository is evicted"
        );
        assert_eq!(unresolved.as_deref(), Some("1"));
        Ok(())
    }

    #[tokio::test]
    async fn unsupported_retry_never_discards_a_resolved_listing() -> Result<(), String> {
        let source_app =
            FakeSourceApp::resolving(vec![description("example-org", "100", "repo-a", "200")?]);
        source_app.fail("201")?;
        let (app, cookie, cache, offset, broker) =
            admitted_app(&source_app, &[("100", "200"), ("100", "201")]).await?;
        list(&app, &cookie, "").await?;

        source_app.set(
            "201",
            Err(PortError::Unsupported {
                operation: "describe_repositories",
            }),
        )?;
        set_clock(&offset, ADMITTED_REPOSITORIES_PARTIAL_TTL)?;
        list(&app, &cookie, "").await?;
        app_calls(&source_app, 2).await?;
        refresh_idle(&cache).await?;
        assert_eq!(source_app.call(1)?, ["201"], "an unresolved-only retry");
        let (status, unresolved, body) = list(&app, &cookie, "").await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["source"], "admitted");
        assert_eq!(listed_names(&body), ["repo-a"]);
        assert_eq!(unresolved.as_deref(), Some("1"));
        assert_eq!(governed_calls(&broker)?, 0);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn cold_wait_is_bounded_when_a_refresh_never_reports() -> Result<(), String> {
        let source_app =
            FakeSourceApp::resolving(vec![description("example-org", "100", "repo-a", "200")?]);
        let (app, cookie, cache, _, _) = admitted_app(&source_app, &[("100", "200")]).await?;
        // A refresh that is marked in flight but never completes.
        cache.lock().refreshing = true;

        let (status, _, body) = list(&app, &cookie, "").await?;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["reason"], "source_app_unavailable");
        assert_eq!(source_app.call_count()?, 0);
        Ok(())
    }

    #[tokio::test]
    async fn refresh_dropped_before_running_still_completes() -> Result<(), String> {
        let source_app =
            FakeSourceApp::resolving(vec![description("example-org", "100", "repo-a", "200")?]);
        let (clock, _) = fixed_clock();
        let config = admitted_config(source_app, &[("100", "200")], clock)?;
        let cache = config.admitted_repositories.clone();
        cache.lock().refreshing = true;
        let refreshed = cache.refreshed.subscribe();

        drop(AdmittedRefreshCompletion::new(
            cache.clone(),
            AdmittedRefresh::Full,
            cache.key.to_vec(),
        ));
        let state = cache.lock();
        assert!(!state.refreshing, "the guard cleared the in-flight flag");
        assert!(
            state.failed_until.is_some(),
            "a cold drop is a shared failure"
        );
        drop(state);
        assert!(
            refreshed.has_changed().map_err(|error| error.to_string())?,
            "waiters are woken"
        );
        Ok(())
    }

    #[tokio::test]
    async fn cold_total_failure_is_a_bounded_source_app_reason() -> Result<(), String> {
        let source_app = FakeSourceApp::default();
        source_app.fail("200")?;
        let (app, cookie, _, offset, broker) = admitted_app(&source_app, &[("100", "200")]).await?;

        let expected = json!({
            "apiVersion": GITHUB_AUTOMATION_API_VERSION,
            "error": "github_automation_unavailable",
            "reason": "source_app_unavailable"
        });
        for _ in 0..2 {
            let (status, _, body) = list(&app, &cookie, "").await?;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(body, expected);
        }
        assert_eq!(
            source_app.call_count()?,
            1,
            "a cold failure is shared briefly"
        );

        set_clock(&offset, ADMITTED_REPOSITORIES_FAILURE_TTL)?;
        let (status, _, _) = list(&app, &cookie, "").await?;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(source_app.call_count()?, 2, "and retried after its TTL");
        assert_eq!(governed_calls(&broker)?, 0);
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_cold_listings_share_one_resolution() -> Result<(), String> {
        let source_app = FakeSourceApp::default();
        source_app.fail("200")?;
        let gate = source_app.close_gate()?;
        let (app, cookie, _, _, _) = admitted_app(&source_app, &[("100", "200")]).await?;

        let release = async {
            for _ in 0..100 {
                tokio::task::yield_now().await;
            }
            gate.add_permits(1);
        };
        let (first, second, third, ()) = tokio::join!(
            list(&app, &cookie, ""),
            list(&app, &cookie, ""),
            list(&app, &cookie, ""),
            release,
        );
        for result in [first?, second?, third?] {
            assert_eq!(result.0, StatusCode::SERVICE_UNAVAILABLE);
        }
        assert_eq!(source_app.call_count()?, 1);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_keeps_every_repository_that_finished() -> Result<(), String> {
        let source_app = FakeSourceApp::resolving(
            (200..205)
                .map(|id| description("example-org", "100", &format!("repo-{id}"), &id.to_string()))
                .collect::<Result<Vec<_>, _>>()?,
        );
        source_app
            .stalled
            .lock()
            .map_err(|_| "lock stalled")?
            .insert("202".to_owned());
        let (app, cookie, _, _, _) = admitted_app(
            &source_app,
            &[
                ("100", "200"),
                ("100", "201"),
                ("100", "202"),
                ("100", "203"),
                ("100", "204"),
            ],
        )
        .await?;

        let (status, unresolved, body) = list(&app, &cookie, "").await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            listed_names(&body),
            ["repo-200", "repo-201", "repo-203", "repo-204"],
            "one slow repository does not discard the others"
        );
        assert_eq!(unresolved.as_deref(), Some("1"));
        Ok(())
    }

    #[tokio::test]
    async fn plane_without_repository_description_keeps_the_governed_listing() -> Result<(), String>
    {
        let bindings = json!({
            "contractVersion": "steward.source-repository-bindings/v1",
            "bindings": [{
                "caller": {"ownerId": "300", "repositoryId": "400"},
                "source": {"ownerId": "100", "repositoryId": "200"}
            }]
        })
        .to_string();
        let mut config = config()?;
        config = config.with_task_api(
            TaskApiConfig::default()
                .with_source_repository_bindings_json(Some(&bindings))?
                .with_git_hosting_plane(DescriptionlessPlane),
        );
        let broker = FakeBroker::default();
        let (auth, cookie, _) = signed_in_cookie_and_csrf().await?;
        let app = protected_router(FakeLedger::default(), broker.clone(), config, auth);

        for expected_calls in 1..=2 {
            let (status, _, body) = list(&app, &cookie, "").await?;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["source"], "connection");
            assert_eq!(body["login"], "alice");
            assert_eq!(governed_calls(&broker)?, expected_calls);
        }
        Ok(())
    }

    #[tokio::test]
    async fn explicit_query_keeps_the_governed_listing() -> Result<(), String> {
        let source_app =
            FakeSourceApp::resolving(vec![description("example-org", "100", "service-a", "200")?]);
        let (app, cookie, _, _, broker) = admitted_app(&source_app, &[("100", "200")]).await?;

        let (status, _, body) = list(&app, &cookie, "?query=org%3Aexample-org").await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["source"], "connection");
        assert_eq!(body["login"], "alice");
        assert_eq!(source_app.call_count()?, 0);
        let calls = broker.calls.lock().map_err(|_| "lock broker calls")?;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].request["query"], "org:example-org");
        Ok(())
    }

    #[tokio::test]
    async fn blank_listing_without_github_source_keeps_the_governed_listing() -> Result<(), String>
    {
        let broker = FakeBroker::default();
        let (auth, cookie, _) = signed_in_cookie_and_csrf().await?;
        let app = protected_router(FakeLedger::default(), broker.clone(), config()?, auth);

        let (status, unresolved, body) = list(&app, &cookie, "").await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(unresolved, None);
        assert_eq!(body["source"], "connection");
        assert_eq!(body["login"], "alice");
        assert_eq!(body["repositories"][1]["ready"], false);
        assert_eq!(governed_calls(&broker)?, 1);
        Ok(())
    }

    #[tokio::test]
    async fn source_app_without_bindings_keeps_the_governed_listing() -> Result<(), String> {
        let source_app = FakeSourceApp::default();
        let mut config = config()?;
        config = config
            .with_task_api(TaskApiConfig::default().with_git_hosting_plane(source_app.clone()));
        let broker = FakeBroker::default();
        let (auth, cookie, _) = signed_in_cookie_and_csrf().await?;
        let app = protected_router(FakeLedger::default(), broker.clone(), config, auth);

        let (status, _, body) = list(&app, &cookie, "").await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["source"], "connection");
        assert_eq!(source_app.call_count()?, 0);
        assert_eq!(governed_calls(&broker)?, 1);
        Ok(())
    }

    #[tokio::test]
    async fn admitted_listing_is_paged_and_keeps_request_bounds() -> Result<(), String> {
        let source_app = FakeSourceApp::resolving(vec![
            description("example-org", "100", "repo-a", "200")?,
            description("example-org", "100", "repo-b", "201")?,
            description("example-org", "100", "repo-c", "202")?,
        ]);
        let (app, cookie, _, _, _) = admitted_app(
            &source_app,
            &[("100", "200"), ("100", "201"), ("100", "202")],
        )
        .await?;

        let (_, _, first) = list(&app, &cookie, "?page=1&perPage=2").await?;
        assert_eq!(listed_names(&first), ["repo-a", "repo-b"]);
        assert_eq!(first["hasNextPage"], true);
        let (_, _, second) = list(&app, &cookie, "?page=2&perPage=2").await?;
        assert_eq!(second["page"], 2);
        assert_eq!(listed_names(&second), ["repo-c"]);
        assert_eq!(second["hasNextPage"], false);
        let (_, _, beyond) = list(&app, &cookie, "?page=9&perPage=2").await?;
        assert_eq!(beyond["repositories"], json!([]));
        assert_eq!(beyond["hasNextPage"], false);
        for invalid in ["?perPage=101", "?perPage=0", "?page=0"] {
            let (status, _, _) = list(&app, &cookie, invalid).await?;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{invalid}");
        }
        assert_eq!(source_app.call_count()?, 1);
        Ok(())
    }
}
