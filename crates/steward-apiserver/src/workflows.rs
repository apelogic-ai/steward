//! Immutable, versioned Workflow contracts.

use std::collections::BTreeSet;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use steward_store::PgStore;
use steward_store::{StoreError, WorkflowPublication, WorkflowRevisionRecord};

use crate::browser_auth::{
    BrowserAdminAuthority, BrowserAuthService, BrowserMutationProof, protect_browser_admin_routes,
};
use crate::{ApiError, BoxFuture};

const BROWSER_WORKFLOW_API_VERSION: &str = "steward.workflows/v1";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowReference {
    pub name: String,
    pub version: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkflowReferenceError {
    Invalid,
}

impl WorkflowReference {
    pub fn parse(value: &str) -> Result<Self, WorkflowReferenceError> {
        let (name, version) = value
            .rsplit_once('@')
            .filter(|(name, version)| !name.contains('@') && !version.is_empty())
            .ok_or(WorkflowReferenceError::Invalid)?;
        if !valid_workflow_name(name) {
            return Err(WorkflowReferenceError::Invalid);
        }
        let version = version
            .parse::<i64>()
            .ok()
            .filter(|version| *version > 0)
            .ok_or(WorkflowReferenceError::Invalid)?;
        Ok(Self {
            name: name.to_owned(),
            version,
        })
    }
}

fn valid_workflow_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    matches!(bytes.first(), Some(byte) if byte.is_ascii_lowercase())
        && matches!(bytes.last(), Some(byte) if byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublishWorkflowRequest {
    pub name: String,
    pub display_name: String,
    pub agent: String,
    pub prompt: String,
}

impl PublishWorkflowRequest {
    pub(crate) fn validate(
        &self,
        allowed_agents: &BTreeSet<String>,
    ) -> Result<(), PublishWorkflowError> {
        if !valid_workflow_name(&self.name)
            || self.display_name.trim().is_empty()
            || !allowed_agents.contains(&self.agent)
            || self.prompt.trim().is_empty()
        {
            return Err(PublishWorkflowError::Invalid);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PublishWorkflowError {
    Invalid,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublishWorkflowVersionRequest {
    pub display_name: String,
    pub agent: String,
    pub prompt: String,
}

impl PublishWorkflowVersionRequest {
    fn as_initial(&self, name: String) -> PublishWorkflowRequest {
        PublishWorkflowRequest {
            name,
            display_name: self.display_name.clone(),
            agent: self.agent.clone(),
            prompt: self.prompt.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRevisionView {
    pub name: String,
    pub version: i64,
    pub display_name: String,
    pub agent: String,
    pub prompt: String,
    pub content_digest: String,
    pub published_by: String,
    pub published_at: String,
}

impl From<WorkflowRevisionRecord> for WorkflowRevisionView {
    fn from(record: WorkflowRevisionRecord) -> Self {
        Self {
            name: record.name,
            version: record.version,
            display_name: record.display_name,
            agent: record.agent,
            prompt: record.prompt,
            content_digest: record.content_digest,
            published_by: record.published_by,
            published_at: record.published_at,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRevisionResponse {
    pub api_version: &'static str,
    pub workflow: WorkflowRevisionView,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowListResponse {
    pub api_version: &'static str,
    /// Exact logical agent references from the deployment-owned execution catalog.
    pub agents: Vec<String>,
    pub workflows: Vec<WorkflowRevisionView>,
}

pub trait WorkflowRepository: Clone + Send + Sync + 'static {
    fn list_latest_workflows(
        &self,
    ) -> BoxFuture<'_, Result<Vec<WorkflowRevisionRecord>, StoreError>>;

    fn workflow_revision<'a>(
        &'a self,
        name: &'a str,
        version: i64,
    ) -> BoxFuture<'a, Result<Option<WorkflowRevisionRecord>, StoreError>>;

    fn publish_initial_workflow<'a>(
        &'a self,
        publication: WorkflowPublication<'a>,
    ) -> BoxFuture<'a, Result<WorkflowRevisionRecord, StoreError>>;

    fn publish_next_workflow<'a>(
        &'a self,
        publication: WorkflowPublication<'a>,
    ) -> BoxFuture<'a, Result<WorkflowRevisionRecord, StoreError>>;
}

impl WorkflowRepository for PgStore {
    fn list_latest_workflows(
        &self,
    ) -> BoxFuture<'_, Result<Vec<WorkflowRevisionRecord>, StoreError>> {
        Box::pin(async move { PgStore::list_latest_workflows(self).await })
    }

    fn workflow_revision<'a>(
        &'a self,
        name: &'a str,
        version: i64,
    ) -> BoxFuture<'a, Result<Option<WorkflowRevisionRecord>, StoreError>> {
        Box::pin(async move { PgStore::workflow_revision(self, name, version).await })
    }

    fn publish_initial_workflow<'a>(
        &'a self,
        publication: WorkflowPublication<'a>,
    ) -> BoxFuture<'a, Result<WorkflowRevisionRecord, StoreError>> {
        Box::pin(async move { PgStore::publish_initial_workflow(self, publication).await })
    }

    fn publish_next_workflow<'a>(
        &'a self,
        publication: WorkflowPublication<'a>,
    ) -> BoxFuture<'a, Result<WorkflowRevisionRecord, StoreError>> {
        Box::pin(async move { PgStore::publish_next_workflow(self, publication).await })
    }
}

#[derive(Clone)]
pub(crate) struct WorkflowApiState<L> {
    repository: L,
    agents: BTreeSet<String>,
}

fn inner_admin_router<L>(repository: L, agents: Vec<String>) -> Router
where
    L: WorkflowRepository,
{
    Router::new()
        .route(
            "/admin/api/v1/workflows",
            get(list_workflows::<L>).post(publish_initial_workflow::<L>),
        )
        .route(
            "/admin/api/v1/workflows/{name}/versions/{version}",
            get(get_workflow_revision::<L>),
        )
        .route(
            "/admin/api/v1/workflows/{name}/versions",
            post(publish_next_workflow::<L>),
        )
        .with_state(WorkflowApiState {
            repository,
            agents: agents.into_iter().collect(),
        })
}

pub fn protected_admin_router<L>(repository: L, browser_auth: BrowserAuthService) -> Router
where
    L: WorkflowRepository,
{
    protected_admin_router_with_agents(repository, browser_auth, Vec::new())
}

pub fn protected_admin_router_with_agents<L>(
    repository: L,
    browser_auth: BrowserAuthService,
    agents: Vec<String>,
) -> Router
where
    L: WorkflowRepository,
{
    protect_browser_admin_routes(inner_admin_router(repository, agents), browser_auth)
}

#[utoipa::path(
    get,
    operation_id = "listAdminWorkflows",
    path = "/admin/api/v1/workflows",
    responses(
        (status = 200, body = WorkflowListResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role is required"),
        (status = 503, description = "Workflow store is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn list_workflows<L>(
    Extension(_authority): Extension<BrowserAdminAuthority>,
    State(state): State<WorkflowApiState<L>>,
) -> Response
where
    L: WorkflowRepository,
{
    match state.repository.list_latest_workflows().await {
        Ok(workflows) => Json(WorkflowListResponse {
            api_version: BROWSER_WORKFLOW_API_VERSION,
            agents: state.agents.iter().cloned().collect(),
            workflows: workflows.into_iter().map(Into::into).collect(),
        })
        .into_response(),
        Err(error) => ApiError::Store(error).into_response(),
    }
}

#[utoipa::path(
    get,
    operation_id = "getAdminWorkflowVersion",
    path = "/admin/api/v1/workflows/{name}/versions/{version}",
    params(("name" = String, Path), ("version" = i64, Path)),
    responses(
        (status = 200, body = WorkflowRevisionResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role is required"),
        (status = 404, description = "Workflow revision was not found"),
        (status = 503, description = "Workflow store is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn get_workflow_revision<L>(
    Extension(_authority): Extension<BrowserAdminAuthority>,
    State(state): State<WorkflowApiState<L>>,
    Path((name, version)): Path<(String, i64)>,
) -> Response
where
    L: WorkflowRepository,
{
    if !valid_workflow_name(&name) || version <= 0 {
        return StatusCode::NOT_FOUND.into_response();
    }
    match state.repository.workflow_revision(&name, version).await {
        Ok(Some(workflow)) => Json(WorkflowRevisionResponse {
            api_version: BROWSER_WORKFLOW_API_VERSION,
            workflow: workflow.into(),
        })
        .into_response(),
        Ok(None) | Err(StoreError::WorkflowNotFound) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => ApiError::Store(error).into_response(),
    }
}

#[utoipa::path(
    post,
    operation_id = "publishAdminWorkflow",
    path = "/admin/api/v1/workflows",
    params(("X-Steward-CSRF" = String, Header)),
    request_body = PublishWorkflowRequest,
    responses(
        (status = 201, body = WorkflowRevisionResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role or mutation proof is invalid"),
        (status = 409, description = "Workflow name already exists"),
        (status = 422, description = "Workflow content is invalid"),
        (status = 503, description = "Workflow store is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn publish_initial_workflow<L>(
    Extension(authority): Extension<BrowserAdminAuthority>,
    Extension(_proof): Extension<BrowserMutationProof>,
    State(state): State<WorkflowApiState<L>>,
    Json(request): Json<PublishWorkflowRequest>,
) -> Response
where
    L: WorkflowRepository,
{
    publish_workflow(state, authority, request, false).await
}

#[utoipa::path(
    post,
    operation_id = "publishAdminWorkflowVersion",
    path = "/admin/api/v1/workflows/{name}/versions",
    params(("name" = String, Path), ("X-Steward-CSRF" = String, Header)),
    request_body = PublishWorkflowVersionRequest,
    responses(
        (status = 201, body = WorkflowRevisionResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 403, description = "Administrator role or mutation proof is invalid"),
        (status = 404, description = "Workflow was not found"),
        (status = 422, description = "Workflow content is invalid"),
        (status = 503, description = "Workflow store is unavailable")
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn publish_next_workflow<L>(
    Extension(authority): Extension<BrowserAdminAuthority>,
    Extension(_proof): Extension<BrowserMutationProof>,
    State(state): State<WorkflowApiState<L>>,
    Path(name): Path<String>,
    Json(request): Json<PublishWorkflowVersionRequest>,
) -> Response
where
    L: WorkflowRepository,
{
    publish_workflow(state, authority, request.as_initial(name), true).await
}

async fn publish_workflow<L>(
    state: WorkflowApiState<L>,
    authority: BrowserAdminAuthority,
    request: PublishWorkflowRequest,
    next: bool,
) -> Response
where
    L: WorkflowRepository,
{
    if state.agents.is_empty() {
        return ApiError::TaskRuntimeContractUnavailable(
            "no coding agents are configured for Workflow publication".to_owned(),
        )
        .into_response();
    }
    if request.validate(&state.agents).is_err() {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    }
    let digest = workflow_content_digest(&request.agent, &request.prompt);
    let publication = WorkflowPublication {
        name: &request.name,
        display_name: &request.display_name,
        agent: &request.agent,
        prompt: &request.prompt,
        content_digest: &digest,
        published_by: authority.principal().canonical_user_id.as_str(),
    };
    let published = if next {
        state.repository.publish_next_workflow(publication).await
    } else {
        state.repository.publish_initial_workflow(publication).await
    };
    match published {
        Ok(workflow) => (
            StatusCode::CREATED,
            Json(WorkflowRevisionResponse {
                api_version: BROWSER_WORKFLOW_API_VERSION,
                workflow: workflow.into(),
            }),
        )
            .into_response(),
        Err(StoreError::WorkflowAlreadyExists) => StatusCode::CONFLICT.into_response(),
        Err(StoreError::WorkflowNotFound) => StatusCode::NOT_FOUND.into_response(),
        Err(StoreError::InvalidWorkflow) => StatusCode::UNPROCESSABLE_ENTITY.into_response(),
        Err(error) => ApiError::Store(error).into_response(),
    }
}

fn workflow_content_digest(agent: &str, prompt: &str) -> String {
    let mut hasher = Sha256::new();
    for value in [agent, prompt] {
        hasher.update(value.len().to_be_bytes());
        hasher.update(value.as_bytes());
    }
    let digest = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("sha256:{digest}")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    use super::{
        PublishWorkflowError, PublishWorkflowRequest, WorkflowReference, WorkflowReferenceError,
        WorkflowRepository, protected_admin_router_with_agents,
    };
    use crate::BoxFuture;
    use crate::browser_auth::{
        BrowserAuthService, LocalFakeIdentity, browser_auth_router, local_fake_browser_auth_service,
    };
    use steward_store::{StoreError, WorkflowPublication, WorkflowRevisionRecord};

    const TEST_AGENT: &str = "example-agent@1.0.0";
    const TEST_AGENT_TWO: &str = "example-agent@2.0.0";

    #[test]
    fn malformed_or_unversioned_workflow_references_fail_closed() {
        for value in [
            "repository-review",
            "repository-review@",
            "@1",
            "repository-review@0",
            "repository-review@-1",
            "repository-review@latest",
            "repository-review@one",
            "repository-review@1@2",
            "Repository-review@1",
            "repository_review@1",
            "repository review@1",
        ] {
            assert_eq!(
                WorkflowReference::parse(value),
                Err(WorkflowReferenceError::Invalid),
                "malformed Workflow reference {value:?} must be rejected"
            );
        }
    }

    #[test]
    fn exact_versioned_workflow_reference_is_accepted() {
        assert_eq!(
            WorkflowReference::parse("repository-review@17"),
            Ok(WorkflowReference {
                name: "repository-review".to_owned(),
                version: 17,
            })
        );
    }

    #[test]
    fn workflow_publication_rejects_authority_and_execution_escape_fields() -> Result<(), String> {
        for forbidden in [
            ("models", serde_json::json!(["openai/gpt-5.4"])),
            (
                "tools",
                serde_json::json!(["github:repository:get_file_contents"]),
            ),
            (
                "budget",
                serde_json::json!({"monthlyLimit": "1", "currency": "USD"}),
            ),
            ("ttl", serde_json::json!("15m")),
            ("runner", serde_json::json!({"platforms": ["linux"]})),
            ("repository", serde_json::json!("example-org/repository")),
            ("revision", serde_json::json!("deadbeef")),
            ("path", serde_json::json!("src/lib.rs")),
            ("namespace", serde_json::json!("default")),
            ("command", serde_json::json!(["sh", "-c", "id"])),
            ("nodes", serde_json::json!([])),
        ] {
            let mut value = serde_json::json!({
                "name": "repository-review",
                "displayName": "Repository review",
                "agent": TEST_AGENT,
                "prompt": "Review the repository state."
            });
            value
                .as_object_mut()
                .ok_or_else(|| "Workflow fixture must be an object".to_owned())?
                .insert(forbidden.0.to_owned(), forbidden.1);
            assert!(
                serde_json::from_value::<PublishWorkflowRequest>(value).is_err(),
                "Workflow publication must reject forbidden field {}",
                forbidden.0
            );
        }
        Ok(())
    }

    #[test]
    fn workflow_publication_accepts_only_deployment_advertised_exact_agents() {
        for request in [
            PublishWorkflowRequest {
                name: "Repository-review".to_owned(),
                display_name: "Repository review".to_owned(),
                agent: TEST_AGENT.to_owned(),
                prompt: "Review the repository state.".to_owned(),
            },
            PublishWorkflowRequest {
                name: "repository-review".to_owned(),
                display_name: " ".to_owned(),
                agent: TEST_AGENT.to_owned(),
                prompt: "Review the repository state.".to_owned(),
            },
            PublishWorkflowRequest {
                name: "repository-review".to_owned(),
                display_name: "Repository review".to_owned(),
                agent: "codex@latest".to_owned(),
                prompt: "Review the repository state.".to_owned(),
            },
            PublishWorkflowRequest {
                name: "repository-review".to_owned(),
                display_name: "Repository review".to_owned(),
                agent: TEST_AGENT.to_owned(),
                prompt: "".to_owned(),
            },
        ] {
            assert_eq!(
                request.validate(&BTreeSet::from([TEST_AGENT.to_owned()])),
                Err(PublishWorkflowError::Invalid),
                "invalid Workflow publication content must fail closed"
            );
        }

        assert_eq!(
            PublishWorkflowRequest {
                name: "repository-review".to_owned(),
                display_name: "Repository review".to_owned(),
                agent: TEST_AGENT.to_owned(),
                prompt: "Review the repository state.".to_owned(),
            }
            .validate(&BTreeSet::from([TEST_AGENT.to_owned()])),
            Ok(())
        );
        assert_eq!(
            PublishWorkflowRequest {
                name: "repository-review".to_owned(),
                display_name: "Repository review".to_owned(),
                agent: TEST_AGENT_TWO.to_owned(),
                prompt: "Review the repository state.".to_owned(),
            }
            .validate(&BTreeSet::from([
                TEST_AGENT.to_owned(),
                TEST_AGENT_TWO.to_owned(),
            ])),
            Ok(()),
            "a successor exact agent reference advertised by the deployment must be publishable"
        );
    }

    #[derive(Clone, Default)]
    struct FakeWorkflowRepository {
        records: Arc<Mutex<Vec<WorkflowRevisionRecord>>>,
    }

    impl WorkflowRepository for FakeWorkflowRepository {
        fn list_latest_workflows(
            &self,
        ) -> BoxFuture<'_, Result<Vec<WorkflowRevisionRecord>, StoreError>> {
            Box::pin(async move {
                let records = self
                    .records
                    .lock()
                    .map_err(|_| StoreError::Database("lock Workflow records".to_owned()))?;
                let mut latest = Vec::<WorkflowRevisionRecord>::new();
                for record in records.iter() {
                    if let Some(existing) = latest.iter_mut().find(|item| item.name == record.name)
                    {
                        if record.version > existing.version {
                            *existing = record.clone();
                        }
                    } else {
                        latest.push(record.clone());
                    }
                }
                Ok(latest)
            })
        }

        fn workflow_revision<'a>(
            &'a self,
            name: &'a str,
            version: i64,
        ) -> BoxFuture<'a, Result<Option<WorkflowRevisionRecord>, StoreError>> {
            Box::pin(async move {
                Ok(self
                    .records
                    .lock()
                    .map_err(|_| StoreError::Database("lock Workflow records".to_owned()))?
                    .iter()
                    .find(|record| record.name == name && record.version == version)
                    .cloned())
            })
        }

        fn publish_initial_workflow<'a>(
            &'a self,
            publication: WorkflowPublication<'a>,
        ) -> BoxFuture<'a, Result<WorkflowRevisionRecord, StoreError>> {
            self.publish(publication, false)
        }

        fn publish_next_workflow<'a>(
            &'a self,
            publication: WorkflowPublication<'a>,
        ) -> BoxFuture<'a, Result<WorkflowRevisionRecord, StoreError>> {
            self.publish(publication, true)
        }
    }

    impl FakeWorkflowRepository {
        fn publish<'a>(
            &'a self,
            publication: WorkflowPublication<'a>,
            next: bool,
        ) -> BoxFuture<'a, Result<WorkflowRevisionRecord, StoreError>> {
            Box::pin(async move {
                let mut records = self
                    .records
                    .lock()
                    .map_err(|_| StoreError::Database("lock Workflow records".to_owned()))?;
                let current = records
                    .iter()
                    .filter(|record| record.name == publication.name)
                    .map(|record| record.version)
                    .max();
                let version = match (next, current) {
                    (false, None) => 1,
                    (false, Some(_)) => return Err(StoreError::WorkflowAlreadyExists),
                    (true, None) => return Err(StoreError::WorkflowNotFound),
                    (true, Some(version)) => version + 1,
                };
                let record = WorkflowRevisionRecord {
                    name: publication.name.to_owned(),
                    version,
                    display_name: publication.display_name.to_owned(),
                    agent: publication.agent.to_owned(),
                    prompt: publication.prompt.to_owned(),
                    content_digest: publication.content_digest.to_owned(),
                    published_by: publication.published_by.to_owned(),
                    published_at: "2026-08-24T00:00:00.000000Z".to_owned(),
                };
                records.push(record.clone());
                Ok(record)
            })
        }
    }

    fn browser_cookie(response: &axum::response::Response, name: &str) -> Result<String, String> {
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

    async fn signed_in_admin(origin: &str) -> Result<(BrowserAuthService, String, String), String> {
        let service = local_fake_browser_auth_service(origin, LocalFakeIdentity::Admin)?;
        let login = browser_auth_router(service.clone())
            .oneshot(
                Request::builder()
                    .uri("/admin/auth/login")
                    .body(Body::empty())
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
        let flow_cookie = browser_cookie(&login, "steward-local-oidc-flow")?;
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
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
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
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
        let session_cookie = browser_cookie(&callback, "steward-local-session")?;
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
        let session = serde_json::from_slice::<serde_json::Value>(
            &to_bytes(session.into_body(), 64 * 1024)
                .await
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        let csrf = session["csrf"]
            .as_str()
            .ok_or_else(|| "session response omitted CSRF".to_owned())?
            .to_owned();
        Ok((service, session_cookie, csrf))
    }

    fn mutation_request(
        uri: &str,
        origin: &str,
        cookie: &str,
        csrf: &str,
        body: serde_json::Value,
    ) -> Result<Request<Body>, String> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::COOKIE, cookie)
            .header(header::ORIGIN, origin)
            .header("sec-fetch-site", "same-origin")
            .header("x-steward-csrf", csrf)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .map_err(|error| error.to_string())
    }

    #[tokio::test]
    async fn admin_publishes_immutable_v1_then_v2_without_mutating_v1() -> Result<(), String> {
        let origin = "http://127.0.0.1:33107";
        let repository = FakeWorkflowRepository::default();
        let (auth, cookie, csrf) = signed_in_admin(origin).await?;
        let app: Router = protected_admin_router_with_agents(
            repository.clone(),
            auth,
            vec![TEST_AGENT.to_owned(), TEST_AGENT_TWO.to_owned()],
        );
        let listed = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/api/v1/workflows")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
        let listed = serde_json::from_slice::<serde_json::Value>(
            &to_bytes(listed.into_body(), 1024 * 1024)
                .await
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        assert_eq!(
            listed.pointer("/agents"),
            Some(&serde_json::json!([TEST_AGENT, TEST_AGENT_TWO])),
            "the authoring UI must receive only deployment-advertised logical references"
        );
        let created_v1 = app
            .clone()
            .oneshot(mutation_request(
                "/admin/api/v1/workflows",
                origin,
                &cookie,
                &csrf,
                serde_json::json!({
                    "name": "repository-review",
                    "displayName": "Repository review",
                    "agent": TEST_AGENT,
                    "prompt": "Review version one."
                }),
            )?)
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(created_v1.status(), StatusCode::CREATED);

        let created_v2 = app
            .clone()
            .oneshot(mutation_request(
                "/admin/api/v1/workflows/repository-review/versions",
                origin,
                &cookie,
                &csrf,
                serde_json::json!({
                    "displayName": "Repository review",
                    "agent": TEST_AGENT_TWO,
                    "prompt": "Review version two."
                }),
            )?)
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(created_v2.status(), StatusCode::CREATED);

        let v1 = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/api/v1/workflows/repository-review/versions/1")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(v1.status(), StatusCode::OK);
        let v1 = serde_json::from_slice::<serde_json::Value>(
            &to_bytes(v1.into_body(), 1024 * 1024)
                .await
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        assert_eq!(v1.pointer("/workflow/version"), Some(&serde_json::json!(1)));
        assert_eq!(
            v1.pointer("/workflow/prompt"),
            Some(&serde_json::json!("Review version one."))
        );
        assert_eq!(
            v1.pointer("/workflow/publishedBy"),
            Some(&serde_json::json!("usr_abcdef0123456789abcdef0123456789"))
        );

        let forbidden_update = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/admin/api/v1/workflows/repository-review/versions/1")
                    .header(header::COOKIE, cookie)
                    .header(header::ORIGIN, origin)
                    .header("sec-fetch-site", "same-origin")
                    .header("x-steward-csrf", csrf)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::empty())
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(forbidden_update.status(), StatusCode::METHOD_NOT_ALLOWED);
        Ok(())
    }

    #[tokio::test]
    async fn empty_catalog_is_advertised_and_publication_is_unavailable() -> Result<(), String> {
        let origin = "http://127.0.0.1:33108";
        let (auth, cookie, csrf) = signed_in_admin(origin).await?;
        let app: Router =
            protected_admin_router_with_agents(FakeWorkflowRepository::default(), auth, Vec::new());
        let listed = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/api/v1/workflows")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(listed.status(), StatusCode::OK);
        let listed = serde_json::from_slice::<serde_json::Value>(
            &to_bytes(listed.into_body(), 1024 * 1024)
                .await
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        assert_eq!(listed.pointer("/agents"), Some(&serde_json::json!([])));

        let rejected = app
            .oneshot(mutation_request(
                "/admin/api/v1/workflows",
                origin,
                &cookie,
                &csrf,
                serde_json::json!({
                    "name": "repository-review",
                    "displayName": "Repository review",
                    "agent": TEST_AGENT,
                    "prompt": "Review the repository state."
                }),
            )?)
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(
            rejected.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "an empty deployment catalog must reject Workflow publication as unavailable"
        );
        Ok(())
    }
}
