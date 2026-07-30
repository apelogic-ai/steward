//! REST admission path and server-rendered approval queue.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;

use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Extension, Json, Router};
use k8s_openapi::api::authentication::v1::{
    TokenReview, TokenReviewSpec, TokenReviewStatus, UserInfo,
};
use kube::api::{Api, PostParams};
use kube::core::Request as KubeRequest;
use kube::{Client, ResourceExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use steward_admission::{
    AdmissionDecision, AdmissionDelta, Envelope, add_budget_amount, evaluate_with_grants,
    validate_envelope,
};
pub use steward_ports::{
    DecisionChannel, DecisionReference, DecisionRequest, DecisionResolution, PortError,
};
use steward_store::{
    ApprovalCandidate, ApproveAdmission, ApprovedAdmission, DecisionFiling, DecisionFilingClaim,
    GrantReversion, ParkRejection, ParkedAdmission, PendingApproval, PgStore, StoreError,
};
use steward_types::{AgentRuntime, AgentRuntimeSpec, PENDING_APPROVAL_ANNOTATION, Principal};
use uuid::Uuid;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdmissionContext {
    pub actor: String,
    pub member_role: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdminContext {
    pub actor: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthenticatedCaller {
    pub actor: String,
    pub member_roles: Vec<String>,
    pub is_admin: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthenticationError {
    InvalidCredentials,
    Unavailable,
}

pub trait RequestAuthenticator: Clone + Send + Sync + 'static {
    fn authenticate<'a>(
        &'a self,
        bearer_token: &'a str,
    ) -> BoxFuture<'a, Result<AuthenticatedCaller, AuthenticationError>>;
}

#[derive(Clone)]
pub struct KubernetesTokenAuthenticator {
    client: Client,
    admin_group: String,
    audience: Option<String>,
}

impl KubernetesTokenAuthenticator {
    pub fn new(client: Client, admin_group: String, audience: Option<String>) -> Self {
        Self {
            client,
            admin_group,
            audience,
        }
    }
}

impl RequestAuthenticator for KubernetesTokenAuthenticator {
    fn authenticate<'a>(
        &'a self,
        bearer_token: &'a str,
    ) -> BoxFuture<'a, Result<AuthenticatedCaller, AuthenticationError>> {
        Box::pin(async move {
            let review = TokenReview {
                spec: TokenReviewSpec {
                    audiences: self.audience.clone().map(|audience| vec![audience]),
                    token: Some(bearer_token.to_owned()),
                },
                ..TokenReview::default()
            };
            let reviewed = Api::<TokenReview>::all(self.client.clone())
                .create(&PostParams::default(), &review)
                .await
                .map_err(|_| AuthenticationError::Unavailable)?;
            caller_from_token_review(reviewed.status, &self.admin_group, self.audience.as_deref())
        })
    }
}

fn caller_from_token_review(
    status: Option<TokenReviewStatus>,
    admin_group: &str,
    requested_audience: Option<&str>,
) -> Result<AuthenticatedCaller, AuthenticationError> {
    let status = status
        .filter(|status| status.authenticated == Some(true))
        .ok_or(AuthenticationError::InvalidCredentials)?;
    if requested_audience.is_some_and(|requested| {
        !status
            .audiences
            .as_deref()
            .unwrap_or_default()
            .iter()
            .any(|audience| audience == requested)
    }) {
        return Err(AuthenticationError::InvalidCredentials);
    }
    let user = status.user.ok_or(AuthenticationError::InvalidCredentials)?;
    caller_from_kubernetes_user(&user, admin_group)
}

fn caller_from_kubernetes_user(
    user: &UserInfo,
    admin_group: &str,
) -> Result<AuthenticatedCaller, AuthenticationError> {
    let actor = user
        .username
        .clone()
        .filter(|username| !username.is_empty())
        .ok_or(AuthenticationError::InvalidCredentials)?;
    let groups = user.groups.as_deref().unwrap_or_default();
    let member_roles = groups
        .iter()
        .filter_map(|group| group.strip_prefix("agents.apelogic.ai/member-role:"))
        .filter(|role| !role.is_empty())
        .map(str::to_owned)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    Ok(AuthenticatedCaller {
        actor,
        member_roles,
        is_admin: groups.iter().any(|group| group == admin_group),
    })
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BudgetIncrease {
    pub amount: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateRuntimeRequest {
    pub name: String,
    pub spec: AgentRuntimeSpec,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalRequest {
    pub rationale: String,
    pub evidence_url: String,
    pub expires_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GrantRevocationRequest {
    pub reason: String,
}

#[derive(utoipa::OpenApi)]
#[openapi(
    paths(create_runtime_contract, budget_increase_contract),
    components(schemas(CreateRuntimeRequest, BudgetIncrease))
)]
pub struct ApiDoc;

#[utoipa::path(
    post,
    path = "/v1/namespaces/{namespace}/runtimes",
    params(
        ("namespace" = String, Path, description = "AgentRuntime namespace")
    ),
    request_body = CreateRuntimeRequest,
    responses(
        (status = 201, description = "In-envelope runtime manifest admitted and created"),
        (status = 202, description = "Over-envelope runtime manifest parked for approval"),
        (status = 403, description = "Authenticated principal does not own the runtime"),
        (status = 404, description = "Kubernetes namespace does not exist"),
        (status = 409, description = "AgentRuntime name already exists"),
        (status = 422, description = "Manifest is invalid or cannot be bound to authority"),
        (status = 503, description = "Kubernetes, authority store, or decision channel unavailable")
    )
)]
#[doc(hidden)]
pub async fn create_runtime_contract() {}

#[utoipa::path(
    patch,
    path = "/v1/namespaces/{namespace}/runtimes/{name}/budget",
    params(
        ("namespace" = String, Path, description = "AgentRuntime namespace"),
        ("name" = String, Path, description = "AgentRuntime name")
    ),
    request_body = BudgetIncrease,
    responses(
        (status = 200, description = "Composed absolute manifest admitted and applied"),
        (status = 202, description = "Composed absolute manifest rejected and parked"),
        (status = 403, description = "Authenticated principal does not own the runtime"),
        (status = 422, description = "Edit or composed manifest is invalid")
    )
)]
#[doc(hidden)]
pub async fn budget_increase_contract() {}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "status"
)]
pub enum SubmissionOutcome {
    Applied {
        proposed_spec: AgentRuntimeSpec,
    },
    Parked {
        approval_id: Uuid,
        decision_id: Uuid,
        decision_key: String,
        evidence_url: String,
        proposed_spec: AgentRuntimeSpec,
        deltas: Vec<AdmissionDelta>,
        counterexample: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApiError {
    RuntimeCreate(RuntimeCreateError),
    Runtime(String),
    Store(StoreError),
    MissingRuntimeUid,
    PrincipalMismatch,
    MissingEnvelope,
    InvalidBudgetIncrease { value: String },
    Admission(String),
    DecisionChannel(String),
    Conflict(String),
    NoActiveGrants,
}

impl fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl Error for ApiError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeCreateError {
    Kubernetes { status: u16, message: String },
    Unavailable(String),
}

pub trait RuntimeRepository: Clone + Send + Sync + 'static {
    fn create<'a>(
        &'a self,
        namespace: &'a str,
        runtime: &'a AgentRuntime,
        context: &'a AdmissionContext,
    ) -> BoxFuture<'a, Result<AgentRuntime, RuntimeCreateError>>;

    fn get<'a>(
        &'a self,
        namespace: &'a str,
        name: &'a str,
    ) -> BoxFuture<'a, Result<AgentRuntime, String>>;

    fn get_bound<'a>(
        &'a self,
        namespace: &'a str,
        name: &'a str,
        runtime_uid: &'a str,
    ) -> BoxFuture<'a, Result<AgentRuntime, String>>;

    fn replace<'a>(
        &'a self,
        runtime: &'a AgentRuntime,
        context: &'a AdmissionContext,
    ) -> BoxFuture<'a, Result<(), String>>;
}

#[derive(Clone)]
pub struct KubeRuntimeRepository {
    client: Client,
}

impl KubeRuntimeRepository {
    pub fn new(client: Client) -> Self {
        Self { client }
    }
}

impl RuntimeRepository for KubeRuntimeRepository {
    fn create<'a>(
        &'a self,
        namespace: &'a str,
        runtime: &'a AgentRuntime,
        context: &'a AdmissionContext,
    ) -> BoxFuture<'a, Result<AgentRuntime, RuntimeCreateError>> {
        Box::pin(async move {
            let unavailable = |error: String| RuntimeCreateError::Unavailable(error);
            let body =
                serde_json::to_vec(runtime).map_err(|error| unavailable(error.to_string()))?;
            let mut request = KubeRequest::new(format!(
                "/apis/agents.apelogic.ai/v1alpha1/namespaces/{namespace}/agentruntimes"
            ))
            .create(&PostParams::default(), body)
            .map_err(|error| unavailable(error.to_string()))?;
            request.headers_mut().insert(
                HeaderName::from_static("impersonate-user"),
                HeaderValue::from_str(&context.actor)
                    .map_err(|error| unavailable(error.to_string()))?,
            );
            request.headers_mut().insert(
                HeaderName::from_static("impersonate-group"),
                HeaderValue::from_str(&format!(
                    "agents.apelogic.ai/member-role:{}",
                    context.member_role
                ))
                .map_err(|error| unavailable(error.to_string()))?,
            );
            self.client
                .request::<AgentRuntime>(request)
                .await
                .map_err(|error| match error {
                    kube::Error::Api(response) => RuntimeCreateError::Kubernetes {
                        status: response.code,
                        message: response.message,
                    },
                    other => RuntimeCreateError::Unavailable(other.to_string()),
                })
        })
    }

    fn get<'a>(
        &'a self,
        namespace: &'a str,
        name: &'a str,
    ) -> BoxFuture<'a, Result<AgentRuntime, String>> {
        Box::pin(async move {
            Api::<AgentRuntime>::namespaced(self.client.clone(), namespace)
                .get(name)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn get_bound<'a>(
        &'a self,
        namespace: &'a str,
        name: &'a str,
        runtime_uid: &'a str,
    ) -> BoxFuture<'a, Result<AgentRuntime, String>> {
        Box::pin(async move {
            let runtime = Api::<AgentRuntime>::namespaced(self.client.clone(), namespace)
                .get(name)
                .await
                .map_err(|error| error.to_string())?;
            if runtime.metadata.uid.as_deref() == Some(runtime_uid) {
                Ok(runtime)
            } else {
                Err(format!(
                    "AgentRuntime {namespace}/{name} no longer has UID {runtime_uid}"
                ))
            }
        })
    }

    fn replace<'a>(
        &'a self,
        runtime: &'a AgentRuntime,
        context: &'a AdmissionContext,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let namespace = runtime
                .namespace()
                .ok_or_else(|| "AgentRuntime namespace is required".to_owned())?;
            let name = runtime.name_any();
            let body = serde_json::to_vec(runtime).map_err(|error| error.to_string())?;
            let mut request = KubeRequest::new(format!(
                "/apis/agents.apelogic.ai/v1alpha1/namespaces/{namespace}/agentruntimes"
            ))
            .replace(&name, &PostParams::default(), body)
            .map_err(|error| error.to_string())?;
            request.headers_mut().insert(
                HeaderName::from_static("impersonate-user"),
                HeaderValue::from_str(&context.actor).map_err(|error| error.to_string())?,
            );
            request.headers_mut().insert(
                HeaderName::from_static("impersonate-group"),
                HeaderValue::from_str(&format!(
                    "agents.apelogic.ai/member-role:{}",
                    context.member_role
                ))
                .map_err(|error| error.to_string())?,
            );
            self.client
                .request::<AgentRuntime>(request)
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        })
    }
}

pub trait AdmissionLedger: Clone + Send + Sync + 'static {
    fn insert_envelope<'a>(
        &'a self,
        member_role: &'a str,
        envelope: &'a Envelope,
        authored_by: &'a str,
    ) -> BoxFuture<'a, Result<(), StoreError>>;

    fn latest_envelope<'a>(
        &'a self,
        member_role: &'a str,
    ) -> BoxFuture<'a, Result<Option<Envelope>, StoreError>>;

    fn park_rejection<'a>(
        &'a self,
        request: ParkRejection<'a>,
    ) -> BoxFuture<'a, Result<ParkedAdmission, StoreError>>;

    fn pending_approvals(&self) -> BoxFuture<'_, Result<Vec<PendingApproval>, StoreError>>;

    fn link_decision_reference<'a>(
        &'a self,
        approval_id: Uuid,
        decision_key: &'a str,
        evidence_url: &'a str,
    ) -> BoxFuture<'a, Result<(), StoreError>>;

    fn approve_admission<'a>(
        &'a self,
        request: ApproveAdmission<'a>,
    ) -> BoxFuture<'a, Result<ApprovedAdmission, StoreError>>;

    fn grants_for_runtime<'a>(
        &'a self,
        runtime_uid: &'a str,
        member_role: &'a str,
        envelope_revision: i64,
    ) -> BoxFuture<'a, Result<Vec<AdmissionDelta>, StoreError>>;

    fn approval_candidate<'a>(
        &'a self,
        approval_id: Uuid,
        evidence_url: &'a str,
    ) -> BoxFuture<'a, Result<ApprovalCandidate, StoreError>>;

    fn revoke_runtime_grants<'a>(
        &'a self,
        runtime_uid: &'a str,
        revoked_by: &'a str,
        reason: &'a str,
    ) -> BoxFuture<'a, Result<u64, StoreError>>;

    fn approval_for_filing(
        &self,
        approval_id: Uuid,
    ) -> BoxFuture<'_, Result<DecisionFiling, StoreError>>;

    fn claim_decision_filing(
        &self,
        approval_id: Uuid,
    ) -> BoxFuture<'_, Result<DecisionFilingClaim, StoreError>>;

    fn complete_decision_filing<'a>(
        &'a self,
        approval_id: Uuid,
        token: Uuid,
        decision_key: &'a str,
        evidence_url: &'a str,
    ) -> BoxFuture<'a, Result<(), StoreError>>;

    fn release_decision_filing(
        &self,
        approval_id: Uuid,
        token: Uuid,
    ) -> BoxFuture<'_, Result<(), StoreError>>;

    fn grant_reversion<'a>(
        &'a self,
        runtime_uid: &'a str,
    ) -> BoxFuture<'a, Result<Option<GrantReversion>, StoreError>>;

    fn grant_application<'a>(
        &'a self,
        runtime_uid: &'a str,
    ) -> BoxFuture<'a, Result<Option<GrantReversion>, StoreError>>;
}

impl AdmissionLedger for PgStore {
    fn insert_envelope<'a>(
        &'a self,
        member_role: &'a str,
        envelope: &'a Envelope,
        authored_by: &'a str,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(
            async move { PgStore::insert_envelope(self, member_role, envelope, authored_by).await },
        )
    }

    fn latest_envelope<'a>(
        &'a self,
        member_role: &'a str,
    ) -> BoxFuture<'a, Result<Option<Envelope>, StoreError>> {
        Box::pin(async move { PgStore::latest_envelope(self, member_role).await })
    }

    fn park_rejection<'a>(
        &'a self,
        request: ParkRejection<'a>,
    ) -> BoxFuture<'a, Result<ParkedAdmission, StoreError>> {
        Box::pin(async move { PgStore::park_rejection(self, request).await })
    }

    fn pending_approvals(&self) -> BoxFuture<'_, Result<Vec<PendingApproval>, StoreError>> {
        Box::pin(async move { PgStore::pending_approvals(self).await })
    }

    fn link_decision_reference<'a>(
        &'a self,
        approval_id: Uuid,
        decision_key: &'a str,
        evidence_url: &'a str,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            PgStore::link_decision_reference(self, approval_id, decision_key, evidence_url).await
        })
    }

    fn approve_admission<'a>(
        &'a self,
        request: ApproveAdmission<'a>,
    ) -> BoxFuture<'a, Result<ApprovedAdmission, StoreError>> {
        Box::pin(async move { PgStore::approve_admission(self, request).await })
    }

    fn grants_for_runtime<'a>(
        &'a self,
        runtime_uid: &'a str,
        member_role: &'a str,
        envelope_revision: i64,
    ) -> BoxFuture<'a, Result<Vec<AdmissionDelta>, StoreError>> {
        Box::pin(async move {
            PgStore::grants_for_runtime(self, runtime_uid, member_role, envelope_revision).await
        })
    }

    fn approval_candidate<'a>(
        &'a self,
        approval_id: Uuid,
        evidence_url: &'a str,
    ) -> BoxFuture<'a, Result<ApprovalCandidate, StoreError>> {
        Box::pin(async move { PgStore::approval_candidate(self, approval_id, evidence_url).await })
    }

    fn revoke_runtime_grants<'a>(
        &'a self,
        runtime_uid: &'a str,
        revoked_by: &'a str,
        reason: &'a str,
    ) -> BoxFuture<'a, Result<u64, StoreError>> {
        Box::pin(async move {
            PgStore::revoke_runtime_grants(self, runtime_uid, revoked_by, reason).await
        })
    }

    fn approval_for_filing(
        &self,
        approval_id: Uuid,
    ) -> BoxFuture<'_, Result<DecisionFiling, StoreError>> {
        Box::pin(async move { PgStore::approval_for_filing(self, approval_id).await })
    }

    fn claim_decision_filing(
        &self,
        approval_id: Uuid,
    ) -> BoxFuture<'_, Result<DecisionFilingClaim, StoreError>> {
        Box::pin(async move { PgStore::claim_decision_filing(self, approval_id).await })
    }

    fn complete_decision_filing<'a>(
        &'a self,
        approval_id: Uuid,
        token: Uuid,
        decision_key: &'a str,
        evidence_url: &'a str,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            PgStore::complete_decision_filing(self, approval_id, token, decision_key, evidence_url)
                .await
        })
    }

    fn release_decision_filing(
        &self,
        approval_id: Uuid,
        token: Uuid,
    ) -> BoxFuture<'_, Result<(), StoreError>> {
        Box::pin(async move { PgStore::release_decision_filing(self, approval_id, token).await })
    }

    fn grant_reversion<'a>(
        &'a self,
        runtime_uid: &'a str,
    ) -> BoxFuture<'a, Result<Option<GrantReversion>, StoreError>> {
        Box::pin(async move { PgStore::grant_reversion(self, runtime_uid).await })
    }

    fn grant_application<'a>(
        &'a self,
        runtime_uid: &'a str,
    ) -> BoxFuture<'a, Result<Option<GrantReversion>, StoreError>> {
        Box::pin(async move { PgStore::grant_application(self, runtime_uid).await })
    }
}

#[derive(Clone)]
struct AppState<R, L, D> {
    runtimes: R,
    ledger: L,
    decisions: D,
}

pub fn router<R, L, A, D>(runtimes: R, ledger: L, authenticator: A, decisions: D) -> Router
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    A: RequestAuthenticator,
    D: DecisionChannel + Clone,
{
    let admission = Router::new()
        .route(
            "/v1/namespaces/{namespace}/runtimes",
            post(create_runtime_handler::<R, L, D>),
        )
        .route(
            "/v1/namespaces/{namespace}/runtimes/{name}/budget",
            patch(budget_increase_handler::<R, L, D>),
        )
        .route_layer(middleware::from_fn_with_state(
            authenticator.clone(),
            authenticate_admission::<A>,
        ));
    let admin = Router::new()
        .route("/admin/approvals", get(approval_queue_handler::<R, L, D>))
        .route(
            "/admin/envelopes/{member_role}",
            post(author_envelope_handler::<R, L, D>),
        )
        .route(
            "/admin/approvals/{approval_id}/approve",
            post(approve_handler::<R, L, D>),
        )
        .route(
            "/admin/approvals/{approval_id}/file",
            post(file_decision_handler::<R, L, D>),
        )
        .route(
            "/admin/runtimes/{runtime_uid}/grants/revoke",
            post(revoke_grants_handler::<R, L, D>),
        )
        .route_layer(middleware::from_fn_with_state(
            authenticator,
            authenticate_admin::<A>,
        ));
    admission.merge(admin).with_state(AppState {
        runtimes,
        ledger,
        decisions,
    })
}

async fn authenticate_admission<A: RequestAuthenticator>(
    State(authenticator): State<A>,
    mut request: Request,
    next: Next,
) -> Response {
    let caller = match authenticate_request(&authenticator, request.headers()).await {
        Ok(caller) => caller,
        Err(response) => return response,
    };
    let roles = caller
        .member_roles
        .iter()
        .filter(|role| !role.is_empty())
        .cloned()
        .collect::<BTreeSet<_>>();
    let Some(member_role) = roles.first().filter(|_| roles.len() == 1) else {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "exactly one authenticated member role is required",
            })),
        )
            .into_response();
    };
    request.extensions_mut().insert(AdmissionContext {
        actor: caller.actor,
        member_role: member_role.clone(),
    });
    next.run(request).await
}

async fn authenticate_admin<A: RequestAuthenticator>(
    State(authenticator): State<A>,
    mut request: Request,
    next: Next,
) -> Response {
    let caller = match authenticate_request(&authenticator, request.headers()).await {
        Ok(caller) => caller,
        Err(response) => return response,
    };
    if !caller.is_admin {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "administrator authority is required",
            })),
        )
            .into_response();
    }
    request.extensions_mut().insert(AdminContext {
        actor: caller.actor,
    });
    next.run(request).await
}

async fn authenticate_request<A: RequestAuthenticator>(
    authenticator: &A,
    headers: &HeaderMap,
) -> Result<AuthenticatedCaller, Response> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| !token.is_empty())
        .ok_or_else(unauthorized)?;
    authenticator.authenticate(token).await.map_err(|error| {
        let status = match error {
            AuthenticationError::InvalidCredentials => StatusCode::UNAUTHORIZED,
            AuthenticationError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        };
        (
            status,
            Json(serde_json::json!({
                "error": "caller authentication failed",
            })),
        )
            .into_response()
    })
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(
            axum::http::header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Bearer"),
        )],
        Json(serde_json::json!({
            "error": "bearer authentication is required",
        })),
    )
        .into_response()
}

async fn budget_increase_handler<R, L, D>(
    State(state): State<AppState<R, L, D>>,
    Extension(context): Extension<AdmissionContext>,
    Path((namespace, name)): Path<(String, String)>,
    Json(edit): Json<BudgetIncrease>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel + Clone,
{
    match submit_budget_increase(
        &state.runtimes,
        &state.ledger,
        &state.decisions,
        &context,
        &namespace,
        &name,
        &edit,
    )
    .await
    {
        Ok(outcome @ SubmissionOutcome::Applied { .. }) => {
            (StatusCode::OK, Json(outcome)).into_response()
        }
        Ok(outcome @ SubmissionOutcome::Parked { .. }) => {
            (StatusCode::ACCEPTED, Json(outcome)).into_response()
        }
        Err(error) => error.into_response(),
    }
}

async fn create_runtime_handler<R, L, D>(
    State(state): State<AppState<R, L, D>>,
    Extension(context): Extension<AdmissionContext>,
    Path(namespace): Path<String>,
    Json(request): Json<CreateRuntimeRequest>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel + Clone,
{
    match submit_runtime_request(
        &state.runtimes,
        &state.ledger,
        &state.decisions,
        &context,
        &namespace,
        &request,
    )
    .await
    {
        Ok(outcome @ SubmissionOutcome::Applied { .. }) => {
            (StatusCode::CREATED, Json(outcome)).into_response()
        }
        Ok(outcome @ SubmissionOutcome::Parked { .. }) => {
            (StatusCode::ACCEPTED, Json(outcome)).into_response()
        }
        Err(error) => error.into_response(),
    }
}

async fn approval_queue_handler<R, L, D>(
    State(state): State<AppState<R, L, D>>,
    Extension(_admin): Extension<AdminContext>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel + Clone,
{
    match state.ledger.pending_approvals().await {
        Ok(approvals) => Html(render_approval_queue(&approvals)).into_response(),
        Err(error) => ApiError::Store(error).into_response(),
    }
}

async fn author_envelope_handler<R, L, D>(
    State(state): State<AppState<R, L, D>>,
    Extension(admin): Extension<AdminContext>,
    Path(member_role): Path<String>,
    Json(envelope): Json<Envelope>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel + Clone,
{
    if member_role.is_empty() || envelope.revision <= 0 {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "member role and positive envelope revision are required",
            })),
        )
            .into_response();
    }
    if let Err(error) = validate_envelope(&envelope) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": format!("invalid envelope: {error:?}"),
            })),
        )
            .into_response();
    }
    match state
        .ledger
        .insert_envelope(&member_role, &envelope, &admin.actor)
        .await
    {
        Ok(()) => StatusCode::CREATED.into_response(),
        Err(error) => ApiError::Store(error).into_response(),
    }
}

async fn approve_handler<R, L, D>(
    State(state): State<AppState<R, L, D>>,
    Extension(admin): Extension<AdminContext>,
    Path(approval_id): Path<Uuid>,
    Json(request): Json<ApprovalRequest>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel + Clone,
{
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
        Ok(outcome) => (StatusCode::OK, Json(outcome)).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn revoke_grants_handler<R, L, D>(
    State(state): State<AppState<R, L, D>>,
    Extension(admin): Extension<AdminContext>,
    Path(runtime_uid): Path<String>,
    Json(request): Json<GrantRevocationRequest>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel + Clone,
{
    let revoked = match state
        .ledger
        .revoke_runtime_grants(&runtime_uid, &admin.actor, &request.reason)
        .await
    {
        Ok(revoked) => revoked,
        Err(error) => return ApiError::Store(error).into_response(),
    };
    if revoked == 0 {
        return ApiError::NoActiveGrants.into_response();
    }
    match reconcile_grant_reversion(&state.runtimes, &state.ledger, &runtime_uid).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => error.into_response(),
    }
}

async fn file_decision_handler<R, L, D>(
    State(state): State<AppState<R, L, D>>,
    Extension(_admin): Extension<AdminContext>,
    Path(approval_id): Path<Uuid>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel + Clone,
{
    match file_decision_reference(&state.ledger, &state.decisions, approval_id).await {
        Ok(reference) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "decisionKey": reference.key,
                "evidenceUrl": reference.evidence_url,
            })),
        )
            .into_response(),
        Err(error) => error.into_response(),
    }
}

fn render_approval_queue(approvals: &[PendingApproval]) -> String {
    let rows = approvals
        .iter()
        .map(|approval| {
            let counterexample = steward_admission::AdmissionDecision::Reject {
                deltas: approval.deltas.clone(),
            }
            .counterexample()
            .unwrap_or_else(|| "envelope exceeded".to_owned());
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                escape_html(&approval.runtime_uid),
                escape_html(&approval.member_role),
                escape_html(&approval.actor),
                escape_html(&counterexample),
                escape_html(approval.decision_key.as_deref().unwrap_or("unfiled")),
                escape_html(approval.evidence_url.as_deref().unwrap_or("unfiled")),
            )
        })
        .collect::<Vec<_>>()
        .join("");
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>Pending approvals</title></head>\
         <body><main><h1>Pending approvals</h1><table><thead><tr><th>Runtime UID</th><th>Member role</th>\
         <th>Actor</th><th>Counterexample</th><th>Decision key</th><th>Evidence</th></tr></thead>\
         <tbody>{rows}</tbody></table></main></body></html>"
    )
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::RuntimeCreate(RuntimeCreateError::Kubernetes { status, .. }) => {
                StatusCode::from_u16(*status).unwrap_or(StatusCode::SERVICE_UNAVAILABLE)
            }
            Self::PrincipalMismatch => StatusCode::FORBIDDEN,
            Self::MissingEnvelope | Self::MissingRuntimeUid => StatusCode::UNPROCESSABLE_ENTITY,
            Self::InvalidBudgetIncrease { .. } | Self::Admission(_) => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::NoActiveGrants => StatusCode::NOT_FOUND,
            Self::Store(StoreError::ApprovalNotFound) => StatusCode::NOT_FOUND,
            Self::Store(
                StoreError::ApprovalNotPending
                | StoreError::MissingDecisionReference
                | StoreError::DecisionReferenceMismatch
                | StoreError::DecisionFilingInProgress
                | StoreError::EvidenceMismatch
                | StoreError::StaleEnvelope
                | StoreError::EnvelopeRevisionNotIncreasing,
            ) => StatusCode::CONFLICT,
            Self::Store(StoreError::InvalidGrantExpiry | StoreError::MissingRevocationReason) => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            Self::RuntimeCreate(RuntimeCreateError::Unavailable(_))
            | Self::Runtime(_)
            | Self::Store(StoreError::Database(_) | StoreError::DecisionFilingClaimLost)
            | Self::DecisionChannel(_) => StatusCode::SERVICE_UNAVAILABLE,
        };
        (
            status,
            Json(serde_json::json!({
                "error": self.to_string(),
            })),
        )
            .into_response()
    }
}

pub async fn submit_budget_increase<R, L, D>(
    runtimes: &R,
    ledger: &L,
    decisions: &D,
    context: &AdmissionContext,
    namespace: &str,
    name: &str,
    edit: &BudgetIncrease,
) -> Result<SubmissionOutcome, ApiError>
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel,
{
    let runtime = runtimes
        .get(namespace, name)
        .await
        .map_err(ApiError::Runtime)?;
    match &runtime.spec.principal {
        Principal::User { acting_user } if acting_user.0 == context.actor => {}
        _ => return Err(ApiError::PrincipalMismatch),
    }
    let runtime_uid = runtime
        .metadata
        .uid
        .as_deref()
        .ok_or(ApiError::MissingRuntimeUid)?;
    let envelope = ledger
        .latest_envelope(&context.member_role)
        .await
        .map_err(ApiError::Store)?
        .ok_or(ApiError::MissingEnvelope)?;
    let mut proposed = runtime.clone();
    proposed.spec.budget.monthly_limit =
        add_budget_amount(&runtime.spec.budget.monthly_limit, &edit.amount).map_err(|_| {
            ApiError::InvalidBudgetIncrease {
                value: edit.amount.clone(),
            }
        })?;
    let grants = ledger
        .grants_for_runtime(runtime_uid, &context.member_role, envelope.revision)
        .await
        .map_err(ApiError::Store)?;
    let decision = evaluate_with_grants(&proposed.spec, &envelope, &grants)
        .map_err(|error| ApiError::Admission(format!("{error:?}")))?;
    match decision {
        AdmissionDecision::Admit => {
            runtimes
                .replace(&proposed, context)
                .await
                .map_err(ApiError::Runtime)?;
            Ok(SubmissionOutcome::Applied {
                proposed_spec: proposed.spec,
            })
        }
        AdmissionDecision::Reject { deltas } => {
            let counterexample = AdmissionDecision::Reject {
                deltas: deltas.clone(),
            }
            .counterexample()
            .ok_or_else(|| {
                ApiError::Admission("rejected decision did not carry a counterexample".to_owned())
            })?;
            let base_spec_digest = spec_digest(&runtime.spec)?;
            let spec_digest = spec_digest(&proposed.spec)?;
            let parked = ledger
                .park_rejection(ParkRejection {
                    runtime_uid,
                    runtime_namespace: namespace,
                    runtime_name: name,
                    spec_digest: &spec_digest,
                    base_spec_digest: &base_spec_digest,
                    base_spec: &runtime.spec,
                    envelope_revision: envelope.revision,
                    deltas: &deltas,
                    proposed_spec: &proposed.spec,
                    actor: &context.actor,
                    member_role: &context.member_role,
                })
                .await
                .map_err(ApiError::Store)?;
            let reference = match (parked.decision_key, parked.evidence_url) {
                (Some(key), Some(evidence_url)) => DecisionReference { key, evidence_url },
                (None, None) => {
                    file_decision_reference(ledger, decisions, parked.approval_id).await?
                }
                _ => {
                    return Err(ApiError::Conflict(
                        "parked approval has an incomplete decision reference".to_owned(),
                    ));
                }
            };
            Ok(SubmissionOutcome::Parked {
                approval_id: parked.approval_id,
                decision_id: parked.decision_id,
                decision_key: reference.key,
                evidence_url: reference.evidence_url,
                proposed_spec: proposed.spec,
                deltas,
                counterexample,
            })
        }
    }
}

pub async fn submit_runtime_request<R, L, D>(
    runtimes: &R,
    ledger: &L,
    decisions: &D,
    context: &AdmissionContext,
    namespace: &str,
    request: &CreateRuntimeRequest,
) -> Result<SubmissionOutcome, ApiError>
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel,
{
    match &request.spec.principal {
        Principal::User { acting_user } if acting_user.0 == context.actor => {}
        _ => return Err(ApiError::PrincipalMismatch),
    }
    let envelope = ledger
        .latest_envelope(&context.member_role)
        .await
        .map_err(ApiError::Store)?
        .ok_or(ApiError::MissingEnvelope)?;
    let decision = evaluate_with_grants(&request.spec, &envelope, &[])
        .map_err(|error| ApiError::Admission(format!("{error:?}")))?;
    let mut runtime = AgentRuntime::new(&request.name, request.spec.clone());
    runtime.metadata.namespace = Some(namespace.to_owned());
    runtime.metadata.annotations = Some(std::collections::BTreeMap::from([(
        "agents.apelogic.ai/member-role".to_owned(),
        context.member_role.clone(),
    )]));
    match decision {
        AdmissionDecision::Admit => {
            runtimes
                .create(namespace, &runtime, context)
                .await
                .map_err(ApiError::RuntimeCreate)?;
            Ok(SubmissionOutcome::Applied {
                proposed_spec: request.spec.clone(),
            })
        }
        AdmissionDecision::Reject { deltas } => {
            let counterexample = AdmissionDecision::Reject {
                deltas: deltas.clone(),
            }
            .counterexample()
            .ok_or_else(|| {
                ApiError::Admission("rejected decision did not carry a counterexample".to_owned())
            })?;
            let request_digest = spec_digest(&request.spec)?;
            runtime.spec.llms.clear();
            runtime.spec.tools.clear();
            runtime.spec.budget.monthly_limit = "0".to_owned();
            runtime.spec.budget.currency = envelope.spec.budget.currency.clone();
            runtime.spec.ttl = envelope.spec.ttl.clone();
            runtime.spec.bindings = None;
            runtime.metadata.annotations.get_or_insert_default().insert(
                PENDING_APPROVAL_ANNOTATION.to_owned(),
                request_digest.clone(),
            );
            let created = match runtimes.create(namespace, &runtime, context).await {
                Ok(created) => created,
                Err(RuntimeCreateError::Kubernetes { status: 409, .. }) => {
                    let existing = runtimes
                        .get(namespace, &request.name)
                        .await
                        .map_err(ApiError::Runtime)?;
                    let annotations = existing.annotations();
                    if annotations
                        .get(PENDING_APPROVAL_ANNOTATION)
                        .map(String::as_str)
                        != Some(request_digest.as_str())
                        || annotations
                            .get("agents.apelogic.ai/member-role")
                            .map(String::as_str)
                            != Some(context.member_role.as_str())
                        || existing.spec != runtime.spec
                    {
                        return Err(ApiError::Conflict(
                            "an unrelated AgentRuntime already uses this name".to_owned(),
                        ));
                    }
                    existing
                }
                Err(error) => return Err(ApiError::RuntimeCreate(error)),
            };
            let runtime_uid = created
                .metadata
                .uid
                .as_deref()
                .ok_or(ApiError::MissingRuntimeUid)?;
            if let Some(application) = ledger
                .grant_application(runtime_uid)
                .await
                .map_err(ApiError::Store)?
            {
                if application.runtime_uid != runtime_uid
                    || application.runtime_namespace != namespace
                    || application.runtime_name != request.name
                    || application.actor != context.actor
                    || application.member_role != context.member_role
                    || application.base_spec != created.spec
                    || application.proposed_spec != request.spec
                {
                    return Err(ApiError::Conflict(
                        "active approved application does not match this create request".to_owned(),
                    ));
                }
                let mut restored = created;
                restored.spec = application.proposed_spec;
                restored
                    .metadata
                    .annotations
                    .get_or_insert_default()
                    .remove(PENDING_APPROVAL_ANNOTATION);
                runtimes
                    .replace(&restored, context)
                    .await
                    .map_err(ApiError::Runtime)?;
                return Ok(SubmissionOutcome::Applied {
                    proposed_spec: request.spec.clone(),
                });
            }
            let base_spec_digest = spec_digest(&created.spec)?;
            let parked = ledger
                .park_rejection(ParkRejection {
                    runtime_uid,
                    runtime_namespace: namespace,
                    runtime_name: &request.name,
                    spec_digest: &request_digest,
                    base_spec_digest: &base_spec_digest,
                    base_spec: &created.spec,
                    envelope_revision: envelope.revision,
                    deltas: &deltas,
                    proposed_spec: &request.spec,
                    actor: &context.actor,
                    member_role: &context.member_role,
                })
                .await
                .map_err(ApiError::Store)?;
            let reference = match (parked.decision_key, parked.evidence_url) {
                (Some(key), Some(evidence_url)) => DecisionReference { key, evidence_url },
                (None, None) => {
                    file_decision_reference(ledger, decisions, parked.approval_id).await?
                }
                _ => {
                    return Err(ApiError::Conflict(
                        "parked approval has an incomplete decision reference".to_owned(),
                    ));
                }
            };
            Ok(SubmissionOutcome::Parked {
                approval_id: parked.approval_id,
                decision_id: parked.decision_id,
                decision_key: reference.key,
                evidence_url: reference.evidence_url,
                proposed_spec: request.spec.clone(),
                deltas,
                counterexample,
            })
        }
    }
}

pub async fn approve_parked_request<R, L, D>(
    runtimes: &R,
    ledger: &L,
    decisions: &D,
    admin: &AdminContext,
    approval_id: Uuid,
    request: &ApprovalRequest,
) -> Result<SubmissionOutcome, ApiError>
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel,
{
    if request.rationale.is_empty()
        || request.evidence_url.is_empty()
        || request.expires_at.is_empty()
    {
        return Err(ApiError::Admission(
            "approval rationale, evidence URL, and grant expiry are required".to_owned(),
        ));
    }
    let candidate = ledger
        .approval_candidate(approval_id, &request.evidence_url)
        .await
        .map_err(ApiError::Store)?;
    let latest_envelope = ledger
        .latest_envelope(&candidate.member_role)
        .await
        .map_err(ApiError::Store)?
        .ok_or(ApiError::MissingEnvelope)?;
    if latest_envelope.revision != candidate.envelope_revision {
        return Err(ApiError::Conflict(
            "approval envelope is no longer current".to_owned(),
        ));
    }
    let mut runtime = runtimes
        .get_bound(
            &candidate.runtime_namespace,
            &candidate.runtime_name,
            &candidate.runtime_uid,
        )
        .await
        .map_err(ApiError::Runtime)?;
    if runtime.metadata.uid.as_deref() != Some(candidate.runtime_uid.as_str()) {
        return Err(ApiError::Runtime(
            "runtime repository returned a different runtime UID".to_owned(),
        ));
    }
    let current_digest = spec_digest(&runtime.spec)?;
    let already_applied = runtime.spec == candidate.proposed_spec;
    if !already_applied && current_digest != candidate.base_spec_digest {
        return Err(ApiError::Conflict(
            "runtime changed after the approval request was parked".to_owned(),
        ));
    }
    let approved = ledger
        .approve_admission(ApproveAdmission {
            approval_id,
            decided_by: &admin.actor,
            rationale: &request.rationale,
            evidence_url: &request.evidence_url,
            expires_at: &request.expires_at,
        })
        .await
        .map_err(ApiError::Store)?;
    let pending_annotation_removed = runtime
        .metadata
        .annotations
        .get_or_insert_default()
        .remove(PENDING_APPROVAL_ANNOTATION)
        .is_some();
    if !already_applied || pending_annotation_removed {
        runtime.spec = approved.proposed_spec.clone();
        runtimes
            .replace(
                &runtime,
                &AdmissionContext {
                    actor: approved.actor.clone(),
                    member_role: approved.member_role.clone(),
                },
            )
            .await
            .map_err(ApiError::Runtime)?;
    }
    decisions
        .record_resolution(&DecisionResolution {
            request_id: approval_id.to_string(),
            key: approved.decision_key,
            decided_by: approved.decided_by,
            rationale: approved.rationale,
            evidence_url: approved.evidence_url,
        })
        .await
        .map_err(|error| ApiError::DecisionChannel(format!("{error:?}")))?;
    Ok(SubmissionOutcome::Applied {
        proposed_spec: approved.proposed_spec,
    })
}

async fn file_decision_reference<L, D>(
    ledger: &L,
    decisions: &D,
    approval_id: Uuid,
) -> Result<DecisionReference, ApiError>
where
    L: AdmissionLedger,
    D: DecisionChannel,
{
    let claim = ledger
        .claim_decision_filing(approval_id)
        .await
        .map_err(ApiError::Store)?;
    let filing = claim.filing;
    if let (Some(key), Some(evidence_url)) = (filing.decision_key, filing.evidence_url) {
        return Ok(DecisionReference { key, evidence_url });
    }
    let token = claim
        .token
        .ok_or(ApiError::Store(StoreError::DecisionFilingClaimLost))?;
    let counterexample = AdmissionDecision::Reject {
        deltas: filing.deltas,
    }
    .counterexample()
    .ok_or_else(|| {
        ApiError::Admission("parked decision did not carry a counterexample".to_owned())
    })?;
    let reference = match decisions
        .request(&DecisionRequest {
            request_id: filing.approval_id.to_string(),
            runtime_uid: filing.runtime_uid,
            actor: filing.actor,
            member_role: filing.member_role,
            counterexample,
        })
        .await
    {
        Ok(reference) => reference,
        Err(error) => {
            ledger
                .release_decision_filing(approval_id, token)
                .await
                .map_err(ApiError::Store)?;
            return Err(ApiError::DecisionChannel(format!("{error:?}")));
        }
    };
    ledger
        .complete_decision_filing(approval_id, token, &reference.key, &reference.evidence_url)
        .await
        .map_err(ApiError::Store)?;
    Ok(reference)
}

async fn reconcile_grant_reversion<R, L>(
    runtimes: &R,
    ledger: &L,
    runtime_uid: &str,
) -> Result<(), ApiError>
where
    R: RuntimeRepository,
    L: AdmissionLedger,
{
    let Some(reversion) = ledger
        .grant_reversion(runtime_uid)
        .await
        .map_err(ApiError::Store)?
    else {
        return Ok(());
    };
    let mut runtime = runtimes
        .get_bound(
            &reversion.runtime_namespace,
            &reversion.runtime_name,
            &reversion.runtime_uid,
        )
        .await
        .map_err(ApiError::Runtime)?;
    if runtime.spec == reversion.proposed_spec {
        runtime.spec = reversion.base_spec;
        runtimes
            .replace(
                &runtime,
                &AdmissionContext {
                    actor: reversion.actor,
                    member_role: reversion.member_role,
                },
            )
            .await
            .map_err(ApiError::Runtime)?;
    }
    Ok(())
}

fn spec_digest(spec: &AgentRuntimeSpec) -> Result<String, ApiError> {
    let serialized =
        serde_json::to_vec(spec).map_err(|error| ApiError::Admission(error.to_string()))?;
    Ok(Sha256::digest(serialized)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use k8s_openapi::api::authentication::v1::{TokenReviewStatus, UserInfo};
    use kube::ResourceExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration as StdDuration;

    use steward_admission::{AdmissionDecision, AdmissionDelta, Envelope, EnvelopeSpec};
    use steward_ports::{
        DecisionChannel, DecisionReference, DecisionRequest, DecisionResolution, PortError,
    };
    use steward_store::{
        ApprovalCandidate, ApproveAdmission, ApprovedAdmission, DecisionFiling,
        DecisionFilingClaim, GrantReversion, ParkRejection, ParkedAdmission, PendingApproval,
        StoreError,
    };
    use steward_types::{
        AgentRuntime, AgentRuntimeSpec, AgentType, Budget, Duration, Email, ModelRef, Principal,
    };
    use tower::ServiceExt;
    use utoipa::OpenApi;
    use uuid::Uuid;

    use super::{
        AdmissionContext, AdmissionLedger, ApiDoc, ApiError, AuthenticatedCaller,
        AuthenticationError, BoxFuture, BudgetIncrease, CreateRuntimeRequest, RequestAuthenticator,
        RuntimeCreateError, RuntimeRepository, SubmissionOutcome, caller_from_kubernetes_user,
        caller_from_token_review, router, submit_budget_increase,
    };

    #[derive(Clone)]
    struct FakeAuthenticator;

    #[test]
    fn kubernetes_identity_is_the_only_source_of_roles_and_admin_authority() -> Result<(), String> {
        let caller = caller_from_kubernetes_user(
            &UserInfo {
                username: Some("alice@example.com".to_owned()),
                groups: Some(vec![
                    "agents.apelogic.ai/member-role:engineer".to_owned(),
                    "agents.apelogic.ai/member-role:engineer".to_owned(),
                    "agents.apelogic.ai/admin".to_owned(),
                ]),
                ..UserInfo::default()
            },
            "agents.apelogic.ai/admin",
        )
        .map_err(|error| format!("authenticated Kubernetes user was rejected: {error:?}"))?;
        assert_eq!(caller.actor, "alice@example.com");
        assert_eq!(caller.member_roles, vec!["engineer"]);
        assert!(caller.is_admin);
        assert_eq!(
            caller_from_kubernetes_user(
                &UserInfo {
                    groups: Some(vec!["agents.apelogic.ai/admin".to_owned()]),
                    ..UserInfo::default()
                },
                "agents.apelogic.ai/admin",
            ),
            Err(AuthenticationError::InvalidCredentials),
            "a group without an authenticated Kubernetes username is not a caller"
        );
        Ok(())
    }

    #[test]
    fn kubernetes_token_audience_must_match_the_configured_api_audience() {
        let status = TokenReviewStatus {
            authenticated: Some(true),
            audiences: Some(vec!["steward-api".to_owned()]),
            user: Some(UserInfo {
                username: Some("alice@example.com".to_owned()),
                groups: Some(vec!["agents.apelogic.ai/member-role:engineer".to_owned()]),
                ..UserInfo::default()
            }),
            ..TokenReviewStatus::default()
        };
        assert!(
            caller_from_token_review(
                Some(status.clone()),
                "agents.apelogic.ai/admin",
                Some("steward-api"),
            )
            .is_ok()
        );
        assert_eq!(
            caller_from_token_review(Some(status), "agents.apelogic.ai/admin", Some("other-api"),),
            Err(AuthenticationError::InvalidCredentials),
            "a valid Kubernetes token for another audience must fail closed"
        );
    }

    impl RequestAuthenticator for FakeAuthenticator {
        fn authenticate<'a>(
            &'a self,
            bearer_token: &'a str,
        ) -> BoxFuture<'a, Result<AuthenticatedCaller, AuthenticationError>> {
            Box::pin(async move {
                match bearer_token {
                    "user-session" => Ok(AuthenticatedCaller {
                        actor: "alice@example.com".to_owned(),
                        member_roles: vec!["engineer".to_owned()],
                        is_admin: false,
                    }),
                    "user-duplicate-role-session" => Ok(AuthenticatedCaller {
                        actor: "alice@example.com".to_owned(),
                        member_roles: vec!["engineer".to_owned(), "engineer".to_owned()],
                        is_admin: false,
                    }),
                    "admin-session" => Ok(AuthenticatedCaller {
                        actor: "admin@example.com".to_owned(),
                        member_roles: Vec::new(),
                        is_admin: true,
                    }),
                    _ => Err(AuthenticationError::InvalidCredentials),
                }
            })
        }
    }

    #[test]
    fn runtime_create_is_published_in_the_openapi_contract() -> Result<(), String> {
        let document = serde_json::to_value(ApiDoc::openapi())
            .map_err(|error| format!("failed to serialize OpenAPI document: {error}"))?;
        let operation = document
            .pointer("/paths/~1v1~1namespaces~1{namespace}~1runtimes/post")
            .ok_or_else(|| "runtime create operation is absent from OpenAPI".to_owned())?;
        assert!(
            operation
                .pointer("/requestBody/content/application~1json/schema")
                .is_some(),
            "runtime create must advertise its request schema"
        );
        for status in ["201", "202", "403", "404", "409", "422", "503"] {
            assert!(
                operation.pointer(&format!("/responses/{status}")).is_some(),
                "runtime create OpenAPI is missing its {status} response"
            );
        }
        for schema in [
            "CreateRuntimeRequest",
            "AgentRuntimeSpec",
            "Principal",
            "AgentType",
            "ModelRef",
            "ToolGrant",
            "Budget",
            "Duration",
            "BindingRef",
            "Email",
        ] {
            assert!(
                document
                    .pointer(&format!("/components/schemas/{schema}"))
                    .is_some(),
                "runtime create OpenAPI is missing its {schema} component"
            );
        }
        Ok(())
    }

    #[derive(Clone, Default)]
    struct FakeDecisionChannel {
        requests: Arc<Mutex<Vec<DecisionRequest>>>,
        resolutions: Arc<Mutex<Vec<DecisionResolution>>>,
    }

    #[derive(Clone, Default)]
    struct SlowDecisionChannel {
        requests: Arc<AtomicUsize>,
    }

    impl DecisionChannel for SlowDecisionChannel {
        async fn request(
            &self,
            _request: &DecisionRequest,
        ) -> Result<DecisionReference, PortError> {
            self.requests.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(StdDuration::from_millis(25)).await;
            Ok(DecisionReference {
                key: "PROJ-123".to_owned(),
                evidence_url: "https://jira.example.com/browse/PROJ-123".to_owned(),
            })
        }

        async fn record_resolution(
            &self,
            _resolution: &DecisionResolution,
        ) -> Result<(), PortError> {
            Ok(())
        }
    }

    impl DecisionChannel for FakeDecisionChannel {
        async fn request(&self, request: &DecisionRequest) -> Result<DecisionReference, PortError> {
            self.requests
                .lock()
                .map_err(|_| PortError::Failed {
                    reason: "fake decision-request lock was poisoned".to_owned(),
                })?
                .push(request.clone());
            Ok(DecisionReference {
                key: "PROJ-123".to_owned(),
                evidence_url: "https://jira.example.com/browse/PROJ-123".to_owned(),
            })
        }

        async fn record_resolution(
            &self,
            resolution: &DecisionResolution,
        ) -> Result<(), PortError> {
            self.resolutions
                .lock()
                .map_err(|_| PortError::Failed {
                    reason: "fake decision-resolution lock was poisoned".to_owned(),
                })?
                .push(resolution.clone());
            Ok(())
        }
    }

    #[derive(Clone)]
    struct FakeRuntimeRepository {
        runtime: Arc<Mutex<AgentRuntime>>,
    }

    #[derive(Clone)]
    struct RejectedCreateRepository {
        runtime: AgentRuntime,
        status: u16,
    }

    impl RuntimeRepository for RejectedCreateRepository {
        fn create<'a>(
            &'a self,
            _namespace: &'a str,
            _runtime: &'a AgentRuntime,
            _context: &'a AdmissionContext,
        ) -> BoxFuture<'a, Result<AgentRuntime, RuntimeCreateError>> {
            Box::pin(async {
                Err(RuntimeCreateError::Kubernetes {
                    status: self.status,
                    message: "Kubernetes rejected the AgentRuntime".to_owned(),
                })
            })
        }

        fn get<'a>(
            &'a self,
            _namespace: &'a str,
            _name: &'a str,
        ) -> BoxFuture<'a, Result<AgentRuntime, String>> {
            Box::pin(async move { Ok(self.runtime.clone()) })
        }

        fn get_bound<'a>(
            &'a self,
            _namespace: &'a str,
            _name: &'a str,
            _runtime_uid: &'a str,
        ) -> BoxFuture<'a, Result<AgentRuntime, String>> {
            Box::pin(async move { Ok(self.runtime.clone()) })
        }

        fn replace<'a>(
            &'a self,
            _runtime: &'a AgentRuntime,
            _context: &'a AdmissionContext,
        ) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }
    }

    impl RuntimeRepository for FakeRuntimeRepository {
        fn create<'a>(
            &'a self,
            _namespace: &'a str,
            runtime: &'a AgentRuntime,
            _context: &'a AdmissionContext,
        ) -> BoxFuture<'a, Result<AgentRuntime, RuntimeCreateError>> {
            Box::pin(async move {
                let mut stored = self.runtime.lock().map_err(|_| {
                    RuntimeCreateError::Unavailable("fake runtime lock was poisoned".to_owned())
                })?;
                if stored.name_any() == runtime.name_any() {
                    return Err(RuntimeCreateError::Kubernetes {
                        status: 409,
                        message: "AgentRuntime already exists".to_owned(),
                    });
                }
                let mut created = runtime.clone();
                created.metadata.uid = Some(format!("{}-uid", created.name_any()));
                created.metadata.resource_version = Some("1".to_owned());
                *stored = created.clone();
                Ok(created)
            })
        }

        fn get<'a>(
            &'a self,
            _namespace: &'a str,
            _name: &'a str,
        ) -> BoxFuture<'a, Result<AgentRuntime, String>> {
            Box::pin(async move {
                self.runtime
                    .lock()
                    .map(|runtime| runtime.clone())
                    .map_err(|_| "fake runtime lock was poisoned".to_owned())
            })
        }

        fn get_bound<'a>(
            &'a self,
            namespace: &'a str,
            name: &'a str,
            runtime_uid: &'a str,
        ) -> BoxFuture<'a, Result<AgentRuntime, String>> {
            Box::pin(async move {
                let runtime = self
                    .runtime
                    .lock()
                    .map_err(|_| "fake runtime lock was poisoned".to_owned())?
                    .clone();
                if runtime.metadata.namespace.as_deref() == Some(namespace)
                    && runtime.metadata.name.as_deref() == Some(name)
                    && runtime.metadata.uid.as_deref() == Some(runtime_uid)
                {
                    Ok(runtime)
                } else {
                    Err(format!(
                        "AgentRuntime {namespace}/{name} is not bound to UID {runtime_uid}"
                    ))
                }
            })
        }

        fn replace<'a>(
            &'a self,
            runtime: &'a AgentRuntime,
            _context: &'a AdmissionContext,
        ) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(async move {
                let mut stored = self
                    .runtime
                    .lock()
                    .map_err(|_| "fake runtime lock was poisoned".to_owned())?;
                *stored = runtime.clone();
                Ok(())
            })
        }
    }

    #[derive(Clone)]
    struct FakeLedger {
        envelope: Arc<Mutex<Envelope>>,
        grants: Vec<AdmissionDelta>,
        parked: ParkedRows,
        decision_references: DecisionReferences,
        decision_filing_claim: Arc<Mutex<Option<Uuid>>>,
        revoke_rows: u64,
        reversion: Option<GrantReversion>,
        application: Arc<Mutex<Option<GrantReversion>>>,
    }

    #[derive(Clone)]
    struct FakeParked {
        runtime_uid: String,
        runtime_namespace: String,
        runtime_name: String,
        deltas: Vec<AdmissionDelta>,
        proposed_spec: AgentRuntimeSpec,
        base_spec_digest: String,
        actor: String,
        member_role: String,
        envelope_revision: i64,
    }

    type ParkedRows = Arc<Mutex<Vec<FakeParked>>>;
    type DecisionReferences = Arc<Mutex<Vec<(Uuid, String, String)>>>;

    impl AdmissionLedger for FakeLedger {
        fn insert_envelope<'a>(
            &'a self,
            _member_role: &'a str,
            _envelope: &'a Envelope,
            _authored_by: &'a str,
        ) -> BoxFuture<'a, Result<(), StoreError>> {
            Box::pin(async { Ok(()) })
        }

        fn latest_envelope<'a>(
            &'a self,
            _member_role: &'a str,
        ) -> BoxFuture<'a, Result<Option<Envelope>, StoreError>> {
            Box::pin(async move {
                self.envelope
                    .lock()
                    .map(|envelope| Some(envelope.clone()))
                    .map_err(|_| StoreError::Database("fake ledger lock was poisoned".to_owned()))
            })
        }

        fn park_rejection<'a>(
            &'a self,
            request: ParkRejection<'a>,
        ) -> BoxFuture<'a, Result<ParkedAdmission, StoreError>> {
            Box::pin(async move {
                let mut parked = self.parked.lock().map_err(|_| {
                    StoreError::Database("fake ledger lock was poisoned".to_owned())
                })?;
                let approved_application_exists = self
                    .application
                    .lock()
                    .map_err(|_| {
                        StoreError::Database(
                            "fake approved-application lock was poisoned".to_owned(),
                        )
                    })?
                    .is_some();
                if parked.is_empty() || approved_application_exists {
                    parked.push(FakeParked {
                        runtime_uid: request.runtime_uid.to_owned(),
                        runtime_namespace: request.runtime_namespace.to_owned(),
                        runtime_name: request.runtime_name.to_owned(),
                        deltas: request.deltas.to_vec(),
                        proposed_spec: request.proposed_spec.clone(),
                        base_spec_digest: request.base_spec_digest.to_owned(),
                        actor: request.actor.to_owned(),
                        member_role: request.member_role.to_owned(),
                        envelope_revision: request.envelope_revision,
                    });
                }
                let reference = self
                    .decision_references
                    .lock()
                    .map_err(|_| StoreError::Database("fake ledger lock was poisoned".to_owned()))?
                    .iter()
                    .find(|(approval_id, _, _)| *approval_id == Uuid::nil())
                    .cloned();
                Ok(ParkedAdmission {
                    decision_id: Uuid::nil(),
                    approval_id: Uuid::nil(),
                    decision_key: reference.as_ref().map(|(_, key, _)| key.clone()),
                    evidence_url: reference.map(|(_, _, url)| url),
                })
            })
        }

        fn pending_approvals(&self) -> BoxFuture<'_, Result<Vec<PendingApproval>, StoreError>> {
            Box::pin(async move {
                let rows = self.parked.lock().map_err(|_| {
                    StoreError::Database("fake ledger lock was poisoned".to_owned())
                })?;
                let references = self.decision_references.lock().map_err(|_| {
                    StoreError::Database("fake ledger lock was poisoned".to_owned())
                })?;
                let reference = references
                    .iter()
                    .find(|(approval_id, _, _)| *approval_id == Uuid::nil());
                Ok(rows
                    .iter()
                    .map(|parked| PendingApproval {
                        approval_id: Uuid::nil(),
                        decision_id: Uuid::nil(),
                        runtime_uid: parked.runtime_uid.clone(),
                        decision_key: reference.map(|(_, key, _)| key.clone()),
                        evidence_url: reference.map(|(_, _, url)| url.clone()),
                        deltas: parked.deltas.clone(),
                        proposed_spec: parked.proposed_spec.clone(),
                        base_spec_digest: parked.base_spec_digest.clone(),
                        envelope_revision: parked.envelope_revision,
                        actor: parked.actor.clone(),
                        member_role: parked.member_role.clone(),
                    })
                    .collect())
            })
        }

        fn link_decision_reference<'a>(
            &'a self,
            approval_id: Uuid,
            decision_key: &'a str,
            evidence_url: &'a str,
        ) -> BoxFuture<'a, Result<(), StoreError>> {
            Box::pin(async move {
                self.decision_references
                    .lock()
                    .map_err(|_| StoreError::Database("fake ledger lock was poisoned".to_owned()))?
                    .push((
                        approval_id,
                        decision_key.to_owned(),
                        evidence_url.to_owned(),
                    ));
                Ok(())
            })
        }

        fn approve_admission<'a>(
            &'a self,
            request: ApproveAdmission<'a>,
        ) -> BoxFuture<'a, Result<ApprovedAdmission, StoreError>> {
            Box::pin(async move {
                let rows = self.parked.lock().map_err(|_| {
                    StoreError::Database("fake ledger lock was poisoned".to_owned())
                })?;
                let parked = rows.first().ok_or(StoreError::ApprovalNotFound)?;
                let references = self.decision_references.lock().map_err(|_| {
                    StoreError::Database("fake ledger lock was poisoned".to_owned())
                })?;
                let (_, decision_key, evidence_url) = references
                    .iter()
                    .find(|(approval_id, _, _)| *approval_id == request.approval_id)
                    .ok_or(StoreError::MissingDecisionReference)?;
                if evidence_url != request.evidence_url {
                    return Err(StoreError::EvidenceMismatch);
                }
                Ok(ApprovedAdmission {
                    approval_id: request.approval_id,
                    decision_id: Uuid::nil(),
                    runtime_uid: parked.runtime_uid.clone(),
                    proposed_spec: parked.proposed_spec.clone(),
                    base_spec_digest: parked.base_spec_digest.clone(),
                    actor: parked.actor.clone(),
                    member_role: parked.member_role.clone(),
                    decision_key: decision_key.clone(),
                    evidence_url: evidence_url.clone(),
                    grants: parked.deltas.clone(),
                    decided_by: request.decided_by.to_owned(),
                    rationale: request.rationale.to_owned(),
                })
            })
        }

        fn grants_for_runtime<'a>(
            &'a self,
            _runtime_uid: &'a str,
            _member_role: &'a str,
            _envelope_revision: i64,
        ) -> BoxFuture<'a, Result<Vec<AdmissionDelta>, StoreError>> {
            Box::pin(async move { Ok(self.grants.clone()) })
        }

        fn approval_candidate<'a>(
            &'a self,
            approval_id: Uuid,
            evidence_url: &'a str,
        ) -> BoxFuture<'a, Result<ApprovalCandidate, StoreError>> {
            Box::pin(async move {
                let rows = self.parked.lock().map_err(|_| {
                    StoreError::Database("fake ledger lock was poisoned".to_owned())
                })?;
                let parked = rows.first().ok_or(StoreError::ApprovalNotFound)?;
                let references = self.decision_references.lock().map_err(|_| {
                    StoreError::Database("fake ledger lock was poisoned".to_owned())
                })?;
                let (_, _, stored_evidence) = references
                    .iter()
                    .find(|(id, _, _)| *id == approval_id)
                    .ok_or(StoreError::MissingDecisionReference)?;
                if stored_evidence != evidence_url {
                    return Err(StoreError::EvidenceMismatch);
                }
                Ok(ApprovalCandidate {
                    approval_id,
                    runtime_uid: parked.runtime_uid.clone(),
                    proposed_spec: parked.proposed_spec.clone(),
                    base_spec_digest: parked.base_spec_digest.clone(),
                    member_role: parked.member_role.clone(),
                    envelope_revision: parked.envelope_revision,
                    runtime_namespace: parked.runtime_namespace.clone(),
                    runtime_name: parked.runtime_name.clone(),
                })
            })
        }

        fn approval_for_filing(
            &self,
            approval_id: Uuid,
        ) -> BoxFuture<'_, Result<DecisionFiling, StoreError>> {
            Box::pin(async move {
                let rows = self.parked.lock().map_err(|_| {
                    StoreError::Database("fake ledger lock was poisoned".to_owned())
                })?;
                let parked = rows.first().ok_or(StoreError::ApprovalNotFound)?;
                let reference = self
                    .decision_references
                    .lock()
                    .map_err(|_| StoreError::Database("fake ledger lock was poisoned".to_owned()))?
                    .iter()
                    .find(|(id, _, _)| *id == approval_id)
                    .cloned();
                Ok(DecisionFiling {
                    approval_id,
                    runtime_uid: parked.runtime_uid.clone(),
                    actor: parked.actor.clone(),
                    member_role: parked.member_role.clone(),
                    deltas: parked.deltas.clone(),
                    decision_key: reference.as_ref().map(|(_, key, _)| key.clone()),
                    evidence_url: reference.map(|(_, _, url)| url),
                })
            })
        }

        fn claim_decision_filing(
            &self,
            approval_id: Uuid,
        ) -> BoxFuture<'_, Result<DecisionFilingClaim, StoreError>> {
            Box::pin(async move {
                let filing = self.approval_for_filing(approval_id).await?;
                if filing.decision_key.is_some() && filing.evidence_url.is_some() {
                    return Ok(DecisionFilingClaim {
                        filing,
                        token: None,
                    });
                }
                let mut claim = self.decision_filing_claim.lock().map_err(|_| {
                    StoreError::Database("fake filing claim lock was poisoned".to_owned())
                })?;
                if claim.is_some() {
                    return Err(StoreError::DecisionFilingInProgress);
                }
                let token = Uuid::new_v4();
                *claim = Some(token);
                Ok(DecisionFilingClaim {
                    filing,
                    token: Some(token),
                })
            })
        }

        fn complete_decision_filing<'a>(
            &'a self,
            approval_id: Uuid,
            token: Uuid,
            decision_key: &'a str,
            evidence_url: &'a str,
        ) -> BoxFuture<'a, Result<(), StoreError>> {
            Box::pin(async move {
                let mut claim = self.decision_filing_claim.lock().map_err(|_| {
                    StoreError::Database("fake filing claim lock was poisoned".to_owned())
                })?;
                if *claim != Some(token) {
                    return Err(StoreError::DecisionFilingClaimLost);
                }
                self.decision_references
                    .lock()
                    .map_err(|_| StoreError::Database("fake ledger lock was poisoned".to_owned()))?
                    .push((
                        approval_id,
                        decision_key.to_owned(),
                        evidence_url.to_owned(),
                    ));
                *claim = None;
                Ok(())
            })
        }

        fn release_decision_filing(
            &self,
            _approval_id: Uuid,
            token: Uuid,
        ) -> BoxFuture<'_, Result<(), StoreError>> {
            Box::pin(async move {
                let mut claim = self.decision_filing_claim.lock().map_err(|_| {
                    StoreError::Database("fake filing claim lock was poisoned".to_owned())
                })?;
                if *claim == Some(token) {
                    *claim = None;
                }
                Ok(())
            })
        }

        fn grant_reversion<'a>(
            &'a self,
            _runtime_uid: &'a str,
        ) -> BoxFuture<'a, Result<Option<GrantReversion>, StoreError>> {
            Box::pin(async move { Ok(self.reversion.clone()) })
        }

        fn grant_application<'a>(
            &'a self,
            _runtime_uid: &'a str,
        ) -> BoxFuture<'a, Result<Option<GrantReversion>, StoreError>> {
            Box::pin(async move {
                self.application
                    .lock()
                    .map(|application| application.clone())
                    .map_err(|_| {
                        StoreError::Database(
                            "fake approved-application lock was poisoned".to_owned(),
                        )
                    })
            })
        }

        fn revoke_runtime_grants<'a>(
            &'a self,
            _runtime_uid: &'a str,
            _revoked_by: &'a str,
            _reason: &'a str,
        ) -> BoxFuture<'a, Result<u64, StoreError>> {
            Box::pin(async move { Ok(self.revoke_rows) })
        }
    }

    fn runtime() -> AgentRuntime {
        let spec = AgentRuntimeSpec {
            principal: Principal::User {
                acting_user: Email("alice@example.com".to_owned()),
            },
            owner: Email("alice@example.com".to_owned()),
            agent_type: AgentType {
                name: "base".to_owned(),
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
        };
        let mut runtime = AgentRuntime::new("runtime-a", spec);
        runtime.metadata.namespace = Some("team-a".to_owned());
        runtime.metadata.uid = Some("runtime-uid-a".to_owned());
        runtime.metadata.annotations = Some(std::collections::BTreeMap::from([(
            "agents.apelogic.ai/member-role".to_owned(),
            "engineer".to_owned(),
        )]));
        runtime.metadata.resource_version = Some("1".to_owned());
        runtime
    }

    fn ledger() -> FakeLedger {
        FakeLedger {
            envelope: Arc::new(Mutex::new(Envelope {
                revision: 3,
                spec: EnvelopeSpec {
                    llms: vec![ModelRef {
                        provider: "provider-a".to_owned(),
                        model: "model-a".to_owned(),
                    }],
                    tools: Vec::new(),
                    budget: Budget {
                        monthly_limit: "200.00".to_owned(),
                        currency: "USD".to_owned(),
                    },
                    ttl: Duration("24h".to_owned()),
                },
            })),
            grants: Vec::new(),
            parked: Arc::new(Mutex::new(Vec::new())),
            decision_references: Arc::new(Mutex::new(Vec::new())),
            decision_filing_claim: Arc::new(Mutex::new(None)),
            revoke_rows: 0,
            reversion: None,
            application: Arc::new(Mutex::new(None)),
        }
    }

    #[derive(Clone, Default)]
    struct FlakyDecisionChannel {
        attempts: Arc<Mutex<usize>>,
    }

    impl DecisionChannel for FlakyDecisionChannel {
        async fn request(
            &self,
            _request: &DecisionRequest,
        ) -> Result<DecisionReference, PortError> {
            let mut attempts = self.attempts.lock().map_err(|_| PortError::Failed {
                reason: "flaky channel lock was poisoned".to_owned(),
            })?;
            *attempts += 1;
            if *attempts == 1 {
                return Err(PortError::Failed {
                    reason: "channel temporarily unavailable".to_owned(),
                });
            }
            Ok(DecisionReference {
                key: "PROJ-123".to_owned(),
                evidence_url: "https://jira.example.com/browse/PROJ-123".to_owned(),
            })
        }

        async fn record_resolution(
            &self,
            _resolution: &DecisionResolution,
        ) -> Result<(), PortError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn sequential_edits_are_admitted_against_the_composed_absolute_manifest()
    -> Result<(), String> {
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let ledger = ledger();
        let decisions = FakeDecisionChannel::default();
        let context = AdmissionContext {
            actor: "alice@example.com".to_owned(),
            member_role: "engineer".to_owned(),
        };
        let edit = BudgetIncrease {
            amount: "60.00".to_owned(),
        };

        let first = submit_budget_increase(
            &runtimes,
            &ledger,
            &decisions,
            &context,
            "team-a",
            "runtime-a",
            &edit,
        )
        .await;
        assert!(
            matches!(first, Ok(SubmissionOutcome::Applied { .. })),
            "the first edit should compose to 160.00 and remain inside the envelope: {first:?}"
        );
        let second = submit_budget_increase(
            &runtimes,
            &ledger,
            &decisions,
            &context,
            "team-a",
            "runtime-a",
            &edit,
        )
        .await;
        let expected_delta = AdmissionDelta::Budget {
            requested: "220.00".to_owned(),
            ceiling: "200.00".to_owned(),
            currency: "USD".to_owned(),
        };
        assert!(
            matches!(
                second,
                Ok(SubmissionOutcome::Parked {
                    ref proposed_spec,
                    ref deltas,
                    ..
                }) if proposed_spec.budget.monthly_limit == "220.00"
                    && deltas == std::slice::from_ref(&expected_delta)
            ),
            "the second edit must be evaluated as absolute 220.00 and parked: {second:?}"
        );
        let parked = ledger
            .parked
            .lock()
            .map_err(|_| "fake ledger lock was poisoned")?;
        assert_eq!(parked.len(), 1);
        assert_eq!(parked[0].runtime_uid, "runtime-uid-a");
        assert_eq!(parked[0].deltas, vec![expected_delta]);
        assert_eq!(parked[0].proposed_spec.budget.monthly_limit, "220.00");
        let envelope = ledger
            .envelope
            .lock()
            .map_err(|_| "fake envelope lock was poisoned")?
            .clone();
        assert_eq!(
            steward_admission::evaluate(&parked[0].proposed_spec, &envelope),
            Ok(AdmissionDecision::Reject {
                deltas: parked[0].deltas.clone()
            })
        );
        drop(parked);
        let requests = decisions
            .requests
            .lock()
            .map_err(|_| "fake decision-request lock was poisoned")?;
        assert_eq!(
            requests.as_slice(),
            &[DecisionRequest {
                request_id: Uuid::nil().to_string(),
                runtime_uid: "runtime-uid-a".to_owned(),
                actor: "alice@example.com".to_owned(),
                member_role: "engineer".to_owned(),
                counterexample:
                    "envelope exceeded: budget.monthlyLimit requested 220.00 USD, ceiling 200.00 USD"
                        .to_owned(),
            }],
            "a parked over-envelope request must be filed with its structured counterexample"
        );
        let references = ledger
            .decision_references
            .lock()
            .map_err(|_| "fake decision-reference lock was poisoned")?;
        assert_eq!(
            references.as_slice(),
            &[(
                Uuid::nil(),
                "PROJ-123".to_owned(),
                "https://jira.example.com/browse/PROJ-123".to_owned(),
            )],
            "the returned Jira reference must be bound to the parked approval"
        );
        Ok(())
    }

    #[tokio::test]
    async fn rest_admission_honors_the_runtime_grants_used_by_the_webhook() -> Result<(), String> {
        let mut granted_runtime = runtime();
        granted_runtime.spec.budget.monthly_limit = "220.00".to_owned();
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(granted_runtime)),
        };
        let mut ledger = ledger();
        ledger.grants = vec![AdmissionDelta::Budget {
            requested: "220.00".to_owned(),
            ceiling: "200.00".to_owned(),
            currency: "USD".to_owned(),
        }];
        let result = submit_budget_increase(
            &runtimes,
            &ledger,
            &FakeDecisionChannel::default(),
            &AdmissionContext {
                actor: "alice@example.com".to_owned(),
                member_role: "engineer".to_owned(),
            },
            "team-a",
            "runtime-a",
            &BudgetIncrease {
                amount: "0.00".to_owned(),
            },
        )
        .await;
        assert!(
            matches!(result, Ok(SubmissionOutcome::Applied { .. })),
            "the REST front door must honor the same instance grant as the webhook: {result:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn runtime_creation_rejects_principal_impersonation_before_writing_desired_state()
    -> Result<(), String> {
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let runtime_state = runtimes.runtime.clone();
        let app = router(
            runtimes,
            ledger(),
            FakeAuthenticator,
            FakeDecisionChannel::default(),
        );
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/namespaces/team-a/runtimes")
                    .header("authorization", "Bearer user-session")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "runtime-b",
                            "spec": {
                                "principal": {
                                    "kind": "user",
                                    "actingUser": "bob@example.org",
                                },
                                "owner": "bob@example.org",
                                "agentType": {"name": "base"},
                                "llms": [{
                                    "provider": "provider-a",
                                    "model": "model-a",
                                }],
                                "tools": [],
                                "budget": {
                                    "monthlyLimit": "100.00",
                                    "currency": "USD",
                                },
                                "ttl": "24h",
                            },
                        })
                        .to_string(),
                    ))
                    .map_err(|error| format!("failed to build create request: {error}"))?,
            )
            .await
            .map_err(|error| format!("create request failed: {error}"))?;
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "an authenticated member must not create a runtime for another principal"
        );
        assert_eq!(
            runtime_state
                .lock()
                .map_err(|_| "fake runtime lock was poisoned")?
                .name_any(),
            "runtime-a",
            "a rejected create must not write desired state"
        );
        Ok(())
    }

    #[tokio::test]
    async fn authenticated_member_can_create_an_in_envelope_runtime() -> Result<(), String> {
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let runtime_state = runtimes.runtime.clone();
        let app = router(
            runtimes,
            ledger(),
            FakeAuthenticator,
            FakeDecisionChannel::default(),
        );
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/namespaces/team-a/runtimes")
                    .header("authorization", "Bearer user-session")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "runtime-b",
                            "spec": {
                                "principal": {
                                    "kind": "user",
                                    "actingUser": "alice@example.com",
                                },
                                "owner": "bob@example.org",
                                "agentType": {"name": "base"},
                                "llms": [{
                                    "provider": "provider-a",
                                    "model": "model-a",
                                }],
                                "tools": [],
                                "budget": {
                                    "monthlyLimit": "100.00",
                                    "currency": "USD",
                                },
                                "ttl": "24h",
                            },
                        })
                        .to_string(),
                    ))
                    .map_err(|error| format!("failed to build create request: {error}"))?,
            )
            .await
            .map_err(|error| format!("create request failed: {error}"))?;
        assert_eq!(
            response.status(),
            StatusCode::CREATED,
            "an authenticated member's in-envelope request must create desired state"
        );
        let created = runtime_state
            .lock()
            .map_err(|_| "fake runtime lock was poisoned")?;
        assert_eq!(created.name_any(), "runtime-b");
        assert_eq!(created.namespace().as_deref(), Some("team-a"));
        assert_eq!(
            created.spec.owner.0, "bob@example.org",
            "the accountable owner may differ from the acting user"
        );
        assert_eq!(
            created
                .metadata
                .annotations
                .as_ref()
                .and_then(|annotations| annotations.get("agents.apelogic.ai/member-role"))
                .map(String::as_str),
            Some("engineer"),
            "the API must bind the authenticated member role into desired state"
        );
        Ok(())
    }

    #[tokio::test]
    async fn runtime_creation_preserves_kubernetes_client_error_statuses() -> Result<(), String> {
        for expected in [
            StatusCode::NOT_FOUND,
            StatusCode::CONFLICT,
            StatusCode::UNPROCESSABLE_ENTITY,
        ] {
            let app = router(
                RejectedCreateRepository {
                    runtime: runtime(),
                    status: expected.as_u16(),
                },
                ledger(),
                FakeAuthenticator,
                FakeDecisionChannel::default(),
            );
            let response = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/namespaces/team-a/runtimes")
                        .header("authorization", "Bearer user-session")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::json!({
                                "name": "runtime-b",
                                "spec": {
                                    "principal": {
                                        "kind": "user",
                                        "actingUser": "alice@example.com",
                                    },
                                    "owner": "alice@example.com",
                                    "agentType": {"name": "base"},
                                    "llms": [{
                                        "provider": "provider-a",
                                        "model": "model-a",
                                    }],
                                    "tools": [],
                                    "budget": {
                                        "monthlyLimit": "100.00",
                                        "currency": "USD",
                                    },
                                    "ttl": "24h",
                                },
                            })
                            .to_string(),
                        ))
                        .map_err(|error| format!("failed to build create request: {error}"))?,
                )
                .await
                .map_err(|error| format!("create request failed: {error}"))?;
            assert_eq!(
                response.status(),
                expected,
                "a deterministic Kubernetes client error must not be reported as a retryable API outage"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn runtime_creation_parks_an_over_envelope_manifest_without_provisioning_it()
    -> Result<(), String> {
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let runtime_state = runtimes.runtime.clone();
        let ledger = ledger();
        let parked_state = ledger.parked.clone();
        let decisions = FakeDecisionChannel::default();
        let decision_requests = decisions.requests.clone();
        let app = router(runtimes, ledger, FakeAuthenticator, decisions);
        let body = serde_json::json!({
            "name": "runtime-b",
            "spec": {
                "principal": {
                    "kind": "user",
                    "actingUser": "alice@example.com",
                },
                "owner": "alice@example.com",
                "agentType": {"name": "base"},
                "llms": [{
                    "provider": "provider-a",
                    "model": "model-a",
                }],
                "tools": [],
                "budget": {
                    "monthlyLimit": "201.00",
                    "currency": "USD",
                },
                "ttl": "24h",
            },
        })
        .to_string();
        let request = || {
            Request::builder()
                .method("POST")
                .uri("/v1/namespaces/team-a/runtimes")
                .header("authorization", "Bearer user-session")
                .header("content-type", "application/json")
                .body(Body::from(body.clone()))
                .map_err(|error| format!("failed to build create request: {error}"))
        };
        let response = app
            .clone()
            .oneshot(request()?)
            .await
            .map_err(|error| format!("create request failed: {error}"))?;
        assert_eq!(
            response.status(),
            StatusCode::ACCEPTED,
            "an over-envelope create must be parked by the REST admission door"
        );
        {
            let pending = runtime_state
                .lock()
                .map_err(|_| "fake runtime lock was poisoned")?;
            assert_eq!(pending.name_any(), "runtime-b");
            assert!(
                pending.spec.llms.is_empty()
                    && pending.spec.tools.is_empty()
                    && pending.spec.budget.monthly_limit == "0",
                "the persisted placeholder must carry no usable model, tool, or budget authority"
            );
            assert!(
                pending
                    .annotations()
                    .contains_key("agents.apelogic.ai/pending-approval"),
                "the controller must be able to hold the placeholder inert until approval"
            );
        }
        assert_eq!(
            parked_state
                .lock()
                .map_err(|_| "fake parked lock was poisoned")?
                .len(),
            1,
            "the rejected initial manifest must create one instance-bound approval"
        );
        assert_eq!(
            decision_requests
                .lock()
                .map_err(|_| "fake decision-request lock was poisoned")?
                .len(),
            1,
            "the parked initial manifest must be filed with the decision channel"
        );
        let replay = app
            .oneshot(request()?)
            .await
            .map_err(|error| format!("replayed create request failed: {error}"))?;
        assert_eq!(
            replay.status(),
            StatusCode::ACCEPTED,
            "a retry after the inert placeholder was created must resume the same parked request"
        );
        assert_eq!(
            parked_state
                .lock()
                .map_err(|_| "fake parked lock was poisoned")?
                .len(),
            1,
            "retrying an initial create must not create a second approval"
        );
        assert_eq!(
            decision_requests
                .lock()
                .map_err(|_| "fake decision-request lock was poisoned")?
                .len(),
            1,
            "retrying a filed initial create must not create another external decision"
        );
        Ok(())
    }

    #[tokio::test]
    async fn runtime_creation_retry_reuses_an_approved_unapplied_request() -> Result<(), String> {
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let runtime_state = runtimes.runtime.clone();
        let ledger = ledger();
        let decisions = FakeDecisionChannel::default();
        let context = AdmissionContext {
            actor: "alice@example.com".to_owned(),
            member_role: "engineer".to_owned(),
        };
        let mut proposed_spec = runtime().spec;
        proposed_spec.budget.monthly_limit = "201.00".to_owned();
        let request = CreateRuntimeRequest {
            name: "runtime-b".to_owned(),
            spec: proposed_spec.clone(),
        };

        let first = super::submit_runtime_request(
            &runtimes, &ledger, &decisions, &context, "team-a", &request,
        )
        .await
        .map_err(|error| format!("initial create was not parked: {error:?}"))?;
        assert!(
            matches!(first, SubmissionOutcome::Parked { .. }),
            "the over-envelope create must be parked before approval"
        );
        let placeholder = runtime_state
            .lock()
            .map_err(|_| "fake runtime lock was poisoned")?
            .clone();
        *ledger
            .application
            .lock()
            .map_err(|_| "fake approved-application lock was poisoned")? = Some(GrantReversion {
            runtime_uid: placeholder
                .metadata
                .uid
                .clone()
                .ok_or_else(|| "placeholder is missing its runtime UID".to_owned())?,
            runtime_namespace: "team-a".to_owned(),
            runtime_name: "runtime-b".to_owned(),
            actor: context.actor.clone(),
            member_role: context.member_role.clone(),
            base_spec: placeholder.spec.clone(),
            proposed_spec: proposed_spec.clone(),
        });

        let retry = super::submit_runtime_request(
            &runtimes, &ledger, &decisions, &context, "team-a", &request,
        )
        .await
        .map_err(|error| format!("approved create retry failed: {error:?}"))?;

        assert!(
            matches!(retry, SubmissionOutcome::Applied { .. }),
            "an approved-but-unapplied create must converge instead of parking another approval: {retry:?}"
        );
        assert_eq!(
            ledger
                .parked
                .lock()
                .map_err(|_| "fake parked lock was poisoned")?
                .len(),
            1,
            "recovery must not create a second approval for the same UID and proposal"
        );
        let restored = runtime_state
            .lock()
            .map_err(|_| "fake runtime lock was poisoned")?;
        assert_eq!(restored.spec, proposed_spec);
        assert!(
            !restored
                .annotations()
                .contains_key("agents.apelogic.ai/pending-approval"),
            "recovery must clear the inert-placeholder marker"
        );
        Ok(())
    }

    #[tokio::test]
    async fn approving_an_initial_create_replaces_and_unblocks_its_placeholder()
    -> Result<(), String> {
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let runtime_state = runtimes.runtime.clone();
        let ledger = ledger();
        let mut proposed_spec = runtime().spec;
        proposed_spec.budget.monthly_limit = "201.00".to_owned();
        let outcome = super::submit_runtime_request(
            &runtimes,
            &ledger,
            &FakeDecisionChannel::default(),
            &AdmissionContext {
                actor: "alice@example.com".to_owned(),
                member_role: "engineer".to_owned(),
            },
            "team-a",
            &CreateRuntimeRequest {
                name: "runtime-b".to_owned(),
                spec: proposed_spec.clone(),
            },
        )
        .await
        .map_err(|error| format!("initial create was not parked: {error:?}"))?;
        let SubmissionOutcome::Parked { approval_id, .. } = outcome else {
            return Err("over-envelope initial create was not parked".to_owned());
        };

        super::approve_parked_request(
            &runtimes,
            &ledger,
            &FakeDecisionChannel::default(),
            &super::AdminContext {
                actor: "admin@example.com".to_owned(),
            },
            approval_id,
            &super::ApprovalRequest {
                rationale: "approved for this runtime".to_owned(),
                evidence_url: "https://jira.example.com/browse/PROJ-123".to_owned(),
                expires_at: "2999-01-01T00:00:00Z".to_owned(),
            },
        )
        .await
        .map_err(|error| format!("initial create approval failed: {error:?}"))?;

        let approved = runtime_state
            .lock()
            .map_err(|_| "fake runtime lock was poisoned")?;
        assert_eq!(approved.spec, proposed_spec);
        assert!(
            !approved
                .annotations()
                .contains_key("agents.apelogic.ai/pending-approval"),
            "approval must remove the controller's inert-placeholder hold"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_repeated_member_role_group_is_one_authenticated_role() -> Result<(), String> {
        let app = router(
            FakeRuntimeRepository {
                runtime: Arc::new(Mutex::new(runtime())),
            },
            ledger(),
            FakeAuthenticator,
            FakeDecisionChannel::default(),
        );
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/v1/namespaces/team-a/runtimes/runtime-a/budget")
                    .header("authorization", "Bearer user-duplicate-role-session")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"amount":"1.00"}"#))
                    .map_err(|error| format!("failed to build duplicate-role request: {error}"))?,
            )
            .await
            .map_err(|error| format!("duplicate-role request failed: {error}"))?;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "repeated copies of the same authenticated group must match webhook semantics"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_channel_outage_can_be_retried_without_duplicate_approvals() -> Result<(), String> {
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let ledger = ledger();
        let decisions = FlakyDecisionChannel::default();
        let context = AdmissionContext {
            actor: "alice@example.com".to_owned(),
            member_role: "engineer".to_owned(),
        };
        let edit = BudgetIncrease {
            amount: "120.00".to_owned(),
        };
        let first = submit_budget_increase(
            &runtimes,
            &ledger,
            &decisions,
            &context,
            "team-a",
            "runtime-a",
            &edit,
        )
        .await;
        assert!(
            matches!(first, Err(ApiError::DecisionChannel(_))),
            "the first channel outage must fail closed: {first:?}"
        );
        let second = submit_budget_increase(
            &runtimes,
            &ledger,
            &decisions,
            &context,
            "team-a",
            "runtime-a",
            &edit,
        )
        .await;
        assert!(
            matches!(second, Ok(SubmissionOutcome::Parked { .. })),
            "resubmitting the same rejected manifest must retry channel filing: {second:?}"
        );
        let third = submit_budget_increase(
            &runtimes,
            &ledger,
            &decisions,
            &context,
            "team-a",
            "runtime-a",
            &edit,
        )
        .await;
        assert!(
            matches!(third, Ok(SubmissionOutcome::Parked { .. })),
            "a filed pending approval must remain an idempotent submission: {third:?}"
        );
        assert_eq!(
            ledger
                .parked
                .lock()
                .map_err(|_| "fake ledger lock was poisoned")?
                .len(),
            1,
            "channel recovery must reuse one durable approval"
        );
        assert_eq!(
            *decisions
                .attempts
                .lock()
                .map_err(|_| "flaky channel lock was poisoned")?,
            2,
            "a retry after the durable decision reference exists must not create another issue"
        );
        Ok(())
    }

    #[tokio::test]
    async fn an_unfiled_approval_has_an_authenticated_recovery_route() -> Result<(), String> {
        let ledger = ledger();
        let decisions = FlakyDecisionChannel::default();
        let app = router(
            FakeRuntimeRepository {
                runtime: Arc::new(Mutex::new(runtime())),
            },
            ledger,
            FakeAuthenticator,
            decisions.clone(),
        );
        let budget_request = || {
            Request::builder()
                .method("PATCH")
                .uri("/v1/namespaces/team-a/runtimes/runtime-a/budget")
                .header("authorization", "Bearer user-session")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"amount":"120.00"}"#))
                .map_err(|error| format!("failed to build budget request: {error}"))
        };
        let outage = app
            .clone()
            .oneshot(budget_request()?)
            .await
            .map_err(|error| format!("budget outage request failed: {error}"))?;
        assert_eq!(outage.status(), StatusCode::SERVICE_UNAVAILABLE);
        let filed = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/approvals/00000000-0000-0000-0000-000000000000/file")
                    .header("authorization", "Bearer admin-session")
                    .body(Body::empty())
                    .map_err(|error| format!("failed to build filing request: {error}"))?,
            )
            .await
            .map_err(|error| format!("filing request failed: {error}"))?;
        assert_eq!(
            filed.status(),
            StatusCode::OK,
            "an administrator must be able to finish a decision-channel write after a crash window"
        );
        let retried = app
            .oneshot(budget_request()?)
            .await
            .map_err(|error| format!("retried budget request failed: {error}"))?;
        assert_eq!(retried.status(), StatusCode::ACCEPTED);
        assert_eq!(
            *decisions
                .attempts
                .lock()
                .map_err(|_| "flaky channel lock was poisoned")?,
            2,
            "the recovered durable reference must prevent a second external decision request"
        );
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_retries_file_exactly_one_external_decision() -> Result<(), String> {
        let ledger = ledger();
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let parked = submit_budget_increase(
            &runtimes,
            &ledger,
            &FlakyDecisionChannel::default(),
            &AdmissionContext {
                actor: "alice@example.com".to_owned(),
                member_role: "engineer".to_owned(),
            },
            "team-a",
            "runtime-a",
            &BudgetIncrease {
                amount: "120.00".to_owned(),
            },
        )
        .await;
        assert!(matches!(parked, Err(ApiError::DecisionChannel(_))));
        let decisions = SlowDecisionChannel::default();
        let (left, right) = tokio::join!(
            super::file_decision_reference(&ledger, &decisions, Uuid::nil()),
            super::file_decision_reference(&ledger, &decisions, Uuid::nil()),
        );
        assert!(
            left.is_ok() || right.is_ok(),
            "one filing attempt must complete successfully"
        );
        assert_eq!(
            decisions.requests.load(Ordering::SeqCst),
            1,
            "a durable filing claim must serialize the outbound decision-channel call"
        );
        Ok(())
    }

    #[tokio::test]
    async fn stale_envelope_blocks_approval_before_desired_state_changes() -> Result<(), String> {
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let runtime_state = runtimes.runtime.clone();
        let ledger = ledger();
        let parked = submit_budget_increase(
            &runtimes,
            &ledger,
            &FakeDecisionChannel::default(),
            &AdmissionContext {
                actor: "alice@example.com".to_owned(),
                member_role: "engineer".to_owned(),
            },
            "team-a",
            "runtime-a",
            &BudgetIncrease {
                amount: "120.00".to_owned(),
            },
        )
        .await;
        assert!(matches!(parked, Ok(SubmissionOutcome::Parked { .. })));
        ledger
            .envelope
            .lock()
            .map_err(|_| "fake envelope lock was poisoned")?
            .revision = 4;
        let result = super::approve_parked_request(
            &runtimes,
            &ledger,
            &FakeDecisionChannel::default(),
            &super::AdminContext {
                actor: "admin@example.com".to_owned(),
            },
            Uuid::nil(),
            &super::ApprovalRequest {
                rationale: "old authority must not cross revisions".to_owned(),
                evidence_url: "https://jira.example.com/browse/PROJ-123".to_owned(),
                expires_at: "2999-01-01T00:00:00Z".to_owned(),
            },
        )
        .await;
        assert!(
            matches!(result, Err(ApiError::Conflict(_))),
            "an approval against a superseded envelope must fail closed: {result:?}"
        );
        assert_eq!(
            runtime_state
                .lock()
                .map_err(|_| "fake runtime lock was poisoned")?
                .spec
                .budget
                .monthly_limit,
            "100.00"
        );
        Ok(())
    }

    #[tokio::test]
    async fn admin_faults_are_not_reported_as_retryable_outages() -> Result<(), String> {
        let app = router(
            FakeRuntimeRepository {
                runtime: Arc::new(Mutex::new(runtime())),
            },
            ledger(),
            FakeAuthenticator,
            FakeDecisionChannel::default(),
        );
        let missing_approval = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/approvals/00000000-0000-0000-0000-000000000000/approve")
                    .header("authorization", "Bearer admin-session")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"rationale":"bounded exception","evidenceUrl":"https://jira.example.com/browse/PROJ-123","expiresAt":"2999-01-01T00:00:00Z"}"#,
                    ))
                    .map_err(|error| format!("failed to build missing approval request: {error}"))?,
            )
            .await
            .map_err(|error| format!("missing approval request failed: {error}"))?;
        assert_eq!(missing_approval.status(), StatusCode::NOT_FOUND);

        let no_grants = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/runtimes/runtime-uid-a/grants/revoke")
                    .header("authorization", "Bearer admin-session")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"reason":"scope ended"}"#))
                    .map_err(|error| format!("failed to build empty revoke request: {error}"))?,
            )
            .await
            .map_err(|error| format!("empty revoke request failed: {error}"))?;
        assert_eq!(
            no_grants.status(),
            StatusCode::NOT_FOUND,
            "revoking a runtime with no active grant must not claim success"
        );
        Ok(())
    }

    #[tokio::test]
    async fn revocation_restores_an_unchanged_escalated_runtime() -> Result<(), String> {
        let base = runtime();
        let mut escalated = base.clone();
        escalated.spec.budget.monthly_limit = "220.00".to_owned();
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(escalated.clone())),
        };
        let runtime_state = runtimes.runtime.clone();
        let mut ledger = ledger();
        ledger.revoke_rows = 1;
        ledger.reversion = Some(GrantReversion {
            runtime_uid: "runtime-uid-a".to_owned(),
            runtime_namespace: "team-a".to_owned(),
            runtime_name: "runtime-a".to_owned(),
            actor: "alice@example.com".to_owned(),
            member_role: "engineer".to_owned(),
            base_spec: base.spec,
            proposed_spec: escalated.spec,
        });
        let response = router(
            runtimes,
            ledger,
            FakeAuthenticator,
            FakeDecisionChannel::default(),
        )
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/runtimes/runtime-uid-a/grants/revoke")
                .header("authorization", "Bearer admin-session")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"reason":"scope ended"}"#))
                .map_err(|error| format!("failed to build revoke request: {error}"))?,
        )
        .await
        .map_err(|error| format!("revoke request failed: {error}"))?;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            runtime_state
                .lock()
                .map_err(|_| "fake runtime lock was poisoned")?
                .spec
                .budget
                .monthly_limit,
            "100.00",
            "revoking an active grant must remove its already-applied authority"
        );
        Ok(())
    }

    #[tokio::test]
    async fn privileged_routes_reject_missing_authentication() -> Result<(), String> {
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let app = router(
            runtimes,
            ledger(),
            FakeAuthenticator,
            FakeDecisionChannel::default(),
        );

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/approvals")
                    .body(Body::empty())
                    .map_err(|error| format!("failed to build unauthenticated request: {error}"))?,
            )
            .await
            .map_err(|error| format!("unauthenticated request failed: {error}"))?;

        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "a router with no authenticated caller must reject privileged routes explicitly"
        );
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/approvals")
                    .header("authorization", "Bearer user-session")
                    .body(Body::empty())
                    .map_err(|error| {
                        format!("failed to build unauthorized-admin request: {error}")
                    })?,
            )
            .await
            .map_err(|error| format!("unauthorized-admin request failed: {error}"))?;
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "an authenticated non-admin must not reach privileged admin routes"
        );
        for (authorization, expected) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some("Bearer user-session"), StatusCode::FORBIDDEN),
        ] {
            for (uri, body) in [
                (
                    "/admin/approvals/00000000-0000-0000-0000-000000000000/approve",
                    r#"{"rationale":"not authorized","evidenceUrl":"https://jira.example.com/browse/PROJ-123"}"#,
                ),
                (
                    "/admin/approvals/00000000-0000-0000-0000-000000000000/file",
                    "{}",
                ),
                (
                    "/admin/runtimes/runtime-uid-a/grants/revoke",
                    r#"{"reason":"not authorized"}"#,
                ),
            ] {
                let mut request = Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json");
                if let Some(authorization) = authorization {
                    request = request.header("authorization", authorization);
                }
                let response = app
                    .clone()
                    .oneshot(request.body(Body::from(body)).map_err(|error| {
                        format!("failed to build unauthorized grant request: {error}")
                    })?)
                    .await
                    .map_err(|error| format!("unauthorized grant request failed: {error}"))?;
                assert_eq!(
                    response.status(),
                    expected,
                    "every grant-authority route must enforce admin authentication: {uri}"
                );
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn rest_route_returns_the_parked_shared_counterexample() -> Result<(), String> {
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let ledger = ledger();
        let envelope_body = serde_json::to_vec(
            &*ledger
                .envelope
                .lock()
                .map_err(|_| "fake envelope lock was poisoned")?,
        )
        .map_err(|error| format!("failed to serialize envelope: {error}"))?;
        let app = router(
            runtimes,
            ledger,
            FakeAuthenticator,
            FakeDecisionChannel::default(),
        );
        let authored = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/envelopes/engineer")
                    .header("authorization", "Bearer admin-session")
                    .header("content-type", "application/json")
                    .body(Body::from(envelope_body))
                    .map_err(|error| format!("failed to build envelope request: {error}"))?,
            )
            .await
            .map_err(|error| format!("envelope authoring request failed: {error}"))?;
        assert_eq!(
            authored.status(),
            StatusCode::CREATED,
            "an authenticated admin must be able to author an immutable envelope revision"
        );
        let request = || {
            Request::builder()
                .method("PATCH")
                .uri("/v1/namespaces/team-a/runtimes/runtime-a/budget")
                .header("authorization", "Bearer user-session")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"amount":"60.00"}"#))
                .map_err(|error| format!("failed to build API request: {error}"))
        };

        let first = app
            .clone()
            .oneshot(request()?)
            .await
            .map_err(|error| format!("first API request failed: {error}"))?;
        assert_eq!(
            first.status(),
            StatusCode::OK,
            "first composed value should apply"
        );
        let second = app
            .clone()
            .oneshot(request()?)
            .await
            .map_err(|error| format!("second API request failed: {error}"))?;
        assert_eq!(
            second.status(),
            StatusCode::ACCEPTED,
            "second composed value should park"
        );
        let body = to_bytes(second.into_body(), 1024 * 1024)
            .await
            .map_err(|error| format!("failed to read parked response: {error}"))?;
        let response = serde_json::from_slice::<serde_json::Value>(&body)
            .map_err(|error| format!("parked response was not JSON: {error}"))?;
        assert_eq!(
            response
                .get("counterexample")
                .and_then(|value| value.as_str()),
            Some("envelope exceeded: budget.monthlyLimit requested 220.00 USD, ceiling 200.00 USD")
        );
        assert_eq!(
            response.pointer("/proposedSpec/budget/monthlyLimit"),
            Some(&serde_json::json!("220.00"))
        );
        assert_eq!(
            response.get("decisionKey"),
            Some(&serde_json::json!("PROJ-123"))
        );
        assert_eq!(
            response.get("evidenceUrl"),
            Some(&serde_json::json!(
                "https://jira.example.com/browse/PROJ-123"
            ))
        );
        let queue = app
            .oneshot(
                Request::builder()
                    .uri("/admin/approvals")
                    .header("authorization", "Bearer admin-session")
                    .body(Body::empty())
                    .map_err(|error| format!("failed to build queue request: {error}"))?,
            )
            .await
            .map_err(|error| format!("approval queue request failed: {error}"))?;
        assert_eq!(queue.status(), StatusCode::OK);
        let queue_body = to_bytes(queue.into_body(), 1024 * 1024)
            .await
            .map_err(|error| format!("failed to read approval queue: {error}"))?;
        let queue_html = String::from_utf8(queue_body.to_vec())
            .map_err(|error| format!("approval queue was not UTF-8: {error}"))?;
        for expected in [
            "runtime-uid-a",
            "engineer",
            "alice@example.com",
            "requested 220.00 USD",
            "ceiling 200.00 USD",
            "PROJ-123",
            "https://jira.example.com/browse/PROJ-123",
        ] {
            assert!(
                queue_html.contains(expected),
                "approval queue must render {expected:?} from the parked row"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn envelope_authoring_rejects_malformed_authority_before_persistence()
    -> Result<(), String> {
        let app = router(
            FakeRuntimeRepository {
                runtime: Arc::new(Mutex::new(runtime())),
            },
            ledger(),
            FakeAuthenticator,
            FakeDecisionChannel::default(),
        );
        for body in [
            r#"{"revision":4,"spec":{"llms":[],"tools":[],"budget":{"monthlyLimit":"invalid","currency":"USD"},"ttl":"24h"}}"#,
            r#"{"revision":4,"spec":{"llms":[],"tools":[],"budget":{"monthlyLimit":"100.00","currency":"US dollars"},"ttl":"24h"}}"#,
            r#"{"revision":4,"spec":{"llms":[],"tools":[],"budget":{"monthlyLimit":"100.00","currency":"USD"},"ttl":"forever"}}"#,
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/admin/envelopes/engineer")
                        .header("authorization", "Bearer admin-session")
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .map_err(|error| {
                            format!("failed to build invalid envelope request: {error}")
                        })?,
                )
                .await
                .map_err(|error| format!("invalid envelope request failed: {error}"))?;
            assert_eq!(
                response.status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "malformed envelope authority must not be persisted"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn only_the_steward_admin_approval_route_applies_a_parked_manifest() -> Result<(), String>
    {
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let runtime_state = runtimes.runtime.clone();
        let ledger = ledger();
        let decisions = FakeDecisionChannel::default();
        let app = router(runtimes, ledger, FakeAuthenticator, decisions.clone());
        let parked = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/v1/namespaces/team-a/runtimes/runtime-a/budget")
                    .header("authorization", "Bearer user-session")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"amount":"120.00"}"#))
                    .map_err(|error| format!("failed to build parked request: {error}"))?,
            )
            .await
            .map_err(|error| format!("parked request failed: {error}"))?;
        assert_eq!(parked.status(), StatusCode::ACCEPTED);
        assert_eq!(
            runtime_state
                .lock()
                .map_err(|_| "fake runtime lock was poisoned")?
                .spec
                .budget
                .monthly_limit,
            "100.00",
            "filing or transitioning a Jira item must not mutate Steward desired state"
        );

        let approved = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/approvals/00000000-0000-0000-0000-000000000000/approve")
                    .header("authorization", "Bearer admin-session")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"rationale":"approved for this runtime","evidenceUrl":"https://jira.example.com/browse/PROJ-123","expiresAt":"2999-01-01T00:00:00Z"}"#,
                    ))
                    .map_err(|error| format!("failed to build approval request: {error}"))?,
            )
            .await
            .map_err(|error| format!("approval request failed: {error}"))?;
        assert_eq!(
            approved.status(),
            StatusCode::OK,
            "the authenticated Steward approval route must apply the parked manifest"
        );
        assert_eq!(
            runtime_state
                .lock()
                .map_err(|_| "fake runtime lock was poisoned")?
                .spec
                .budget
                .monthly_limit,
            "220.00"
        );
        let resolutions = decisions
            .resolutions
            .lock()
            .map_err(|_| "fake decision-resolution lock was poisoned")?;
        assert_eq!(resolutions.len(), 1);
        assert_eq!(resolutions[0].key, "PROJ-123");
        assert_eq!(resolutions[0].decided_by, "admin@example.com");
        Ok(())
    }

    #[tokio::test]
    async fn approval_never_overwrites_a_runtime_changed_after_parking() -> Result<(), String> {
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let runtime_state = runtimes.runtime.clone();
        let app = router(
            runtimes,
            ledger(),
            FakeAuthenticator,
            FakeDecisionChannel::default(),
        );
        let parked = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/v1/namespaces/team-a/runtimes/runtime-a/budget")
                    .header("authorization", "Bearer user-session")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"amount":"120.00"}"#))
                    .map_err(|error| format!("failed to build parked request: {error}"))?,
            )
            .await
            .map_err(|error| format!("parked request failed: {error}"))?;
        assert_eq!(parked.status(), StatusCode::ACCEPTED);
        {
            let mut runtime = runtime_state
                .lock()
                .map_err(|_| "fake runtime lock was poisoned")?;
            runtime.spec.llms[0].model = "model-b".to_owned();
            runtime.metadata.resource_version = Some("2".to_owned());
        }
        let approved = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/approvals/00000000-0000-0000-0000-000000000000/approve")
                    .header("authorization", "Bearer admin-session")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"rationale":"stale request","evidenceUrl":"https://jira.example.com/browse/PROJ-123","expiresAt":"2999-01-01T00:00:00Z"}"#,
                    ))
                    .map_err(|error| format!("failed to build stale approval request: {error}"))?,
            )
            .await
            .map_err(|error| format!("stale approval request failed: {error}"))?;
        assert_eq!(
            approved.status(),
            StatusCode::CONFLICT,
            "approval must fail before issuing a grant when the parked resourceVersion is stale"
        );
        assert_eq!(
            runtime_state
                .lock()
                .map_err(|_| "fake runtime lock was poisoned")?
                .spec
                .llms[0]
                .model,
            "model-b",
            "stale approval must preserve the intervening runtime change"
        );
        Ok(())
    }
}
