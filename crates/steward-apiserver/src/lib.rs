//! REST admission path and server-rendered approval queue.

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;

use axum::extract::{Path, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Extension, Json, Router};
use kube::api::{Api, PostParams};
use kube::core::Request as KubeRequest;
use kube::{Client, ResourceExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use steward_admission::{AdmissionDecision, AdmissionDelta, Envelope, evaluate};
use steward_store::{ParkRejection, ParkedAdmission, PendingApproval, PgStore, StoreError};
use steward_types::{AgentRuntime, AgentRuntimeSpec, Principal};
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BudgetIncrease {
    pub amount: String,
}

#[derive(utoipa::OpenApi)]
#[openapi(paths(budget_increase_contract), components(schemas(BudgetIncrease)))]
pub struct ApiDoc;

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
        proposed_spec: AgentRuntimeSpec,
        deltas: Vec<AdmissionDelta>,
        counterexample: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApiError {
    Runtime(String),
    Store(String),
    MissingRuntimeUid,
    PrincipalMismatch,
    MissingEnvelope,
    InvalidBudgetIncrease { value: String },
    Admission(String),
}

impl fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl Error for ApiError {}

pub trait RuntimeRepository: Clone + Send + Sync + 'static {
    fn get<'a>(
        &'a self,
        namespace: &'a str,
        name: &'a str,
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
}

#[derive(Clone)]
struct AppState<R, L> {
    runtimes: R,
    ledger: L,
}

pub fn router<R, L>(runtimes: R, ledger: L) -> Router
where
    R: RuntimeRepository,
    L: AdmissionLedger,
{
    Router::new()
        .route(
            "/v1/namespaces/{namespace}/runtimes/{name}/budget",
            patch(budget_increase_handler::<R, L>),
        )
        .route("/admin/approvals", get(approval_queue_handler::<R, L>))
        .route(
            "/admin/envelopes/{member_role}",
            post(author_envelope_handler::<R, L>),
        )
        .with_state(AppState { runtimes, ledger })
}

async fn budget_increase_handler<R, L>(
    State(state): State<AppState<R, L>>,
    Extension(context): Extension<AdmissionContext>,
    Path((namespace, name)): Path<(String, String)>,
    Json(edit): Json<BudgetIncrease>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger,
{
    match submit_budget_increase(
        &state.runtimes,
        &state.ledger,
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

async fn approval_queue_handler<R, L>(
    State(state): State<AppState<R, L>>,
    Extension(_admin): Extension<AdminContext>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger,
{
    match state.ledger.pending_approvals().await {
        Ok(approvals) => Html(render_approval_queue(&approvals)).into_response(),
        Err(error) => ApiError::Store(error.to_string()).into_response(),
    }
}

async fn author_envelope_handler<R, L>(
    State(state): State<AppState<R, L>>,
    Extension(admin): Extension<AdminContext>,
    Path(member_role): Path<String>,
    Json(envelope): Json<Envelope>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger,
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
    match state
        .ledger
        .insert_envelope(&member_role, &envelope, &admin.actor)
        .await
    {
        Ok(()) => StatusCode::CREATED.into_response(),
        Err(error) => ApiError::Store(error.to_string()).into_response(),
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
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                escape_html(&approval.runtime_uid),
                escape_html(&approval.member_role),
                escape_html(&approval.actor),
                escape_html(&counterexample),
            )
        })
        .collect::<Vec<_>>()
        .join("");
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>Pending approvals</title></head>\
         <body><main><h1>Pending approvals</h1><table><thead><tr><th>Runtime UID</th><th>Member role</th>\
         <th>Actor</th><th>Counterexample</th></tr></thead><tbody>{rows}</tbody></table></main></body></html>"
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
            Self::PrincipalMismatch => StatusCode::FORBIDDEN,
            Self::MissingEnvelope | Self::MissingRuntimeUid => StatusCode::UNPROCESSABLE_ENTITY,
            Self::InvalidBudgetIncrease { .. } | Self::Admission(_) => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            Self::Runtime(_) | Self::Store(_) => StatusCode::SERVICE_UNAVAILABLE,
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

pub async fn submit_budget_increase<R, L>(
    runtimes: &R,
    ledger: &L,
    context: &AdmissionContext,
    namespace: &str,
    name: &str,
    edit: &BudgetIncrease,
) -> Result<SubmissionOutcome, ApiError>
where
    R: RuntimeRepository,
    L: AdmissionLedger,
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
        .map_err(|error| ApiError::Store(error.to_string()))?
        .ok_or(ApiError::MissingEnvelope)?;
    let mut proposed = runtime.clone();
    proposed.spec.budget.monthly_limit = add_decimal(
        &runtime.spec.budget.monthly_limit,
        &edit.amount,
        &edit.amount,
    )?;
    let decision = evaluate(&proposed.spec, &envelope)
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
            let serialized = serde_json::to_vec(&proposed.spec)
                .map_err(|error| ApiError::Admission(error.to_string()))?;
            let digest = Sha256::digest(serialized);
            let spec_digest = digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            let parked = ledger
                .park_rejection(ParkRejection {
                    runtime_uid,
                    spec_digest: &spec_digest,
                    envelope_revision: envelope.revision,
                    deltas: &deltas,
                    proposed_spec: &proposed.spec,
                    actor: &context.actor,
                    member_role: &context.member_role,
                })
                .await
                .map_err(|error| ApiError::Store(error.to_string()))?;
            Ok(SubmissionOutcome::Parked {
                approval_id: parked.approval_id,
                decision_id: parked.decision_id,
                proposed_spec: proposed.spec,
                deltas,
                counterexample,
            })
        }
    }
}

fn add_decimal(left: &str, right: &str, reported_value: &str) -> Result<String, ApiError> {
    let (mut left_digits, left_scale) = decimal_digits(left, reported_value)?;
    let (mut right_digits, right_scale) = decimal_digits(right, reported_value)?;
    let scale = left_scale.max(right_scale);
    left_digits.extend(std::iter::repeat_n(0, scale - left_scale));
    right_digits.extend(std::iter::repeat_n(0, scale - right_scale));
    let width = left_digits.len().max(right_digits.len());
    left_digits.splice(0..0, std::iter::repeat_n(0, width - left_digits.len()));
    right_digits.splice(0..0, std::iter::repeat_n(0, width - right_digits.len()));

    let mut carry = 0;
    let mut sum = Vec::with_capacity(width + 1);
    for (left, right) in left_digits.into_iter().zip(right_digits).rev() {
        let value = left + right + carry;
        sum.push(value % 10);
        carry = value / 10;
    }
    if carry > 0 {
        sum.push(carry);
    }
    sum.reverse();
    let first_nonzero = sum
        .iter()
        .position(|digit| *digit != 0)
        .unwrap_or(sum.len().saturating_sub(scale + 1));
    let mut text = sum[first_nonzero..]
        .iter()
        .map(|digit| char::from(b'0' + *digit))
        .collect::<String>();
    if scale > 0 {
        if text.len() <= scale {
            text.insert_str(0, &"0".repeat(scale + 1 - text.len()));
        }
        text.insert(text.len() - scale, '.');
    }
    Ok(text)
}

fn decimal_digits(value: &str, reported_value: &str) -> Result<(Vec<u8>, usize), ApiError> {
    let mut parts = value.split('.');
    let integer = parts.next().unwrap_or_default();
    let fractional = parts.next().unwrap_or_default();
    if integer.is_empty()
        || parts.next().is_some()
        || !integer.bytes().all(|byte| byte.is_ascii_digit())
        || !fractional.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(ApiError::InvalidBudgetIncrease {
            value: reported_value.to_owned(),
        });
    }
    let digits = integer
        .bytes()
        .chain(fractional.bytes())
        .map(|byte| byte - b'0')
        .collect();
    Ok((digits, fractional.len()))
}

#[cfg(test)]
mod tests {
    use axum::Extension;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use std::sync::{Arc, Mutex};

    use steward_admission::{AdmissionDecision, AdmissionDelta, Envelope, EnvelopeSpec};
    use steward_store::{ParkRejection, ParkedAdmission, PendingApproval, StoreError};
    use steward_types::{
        AgentRuntime, AgentRuntimeSpec, AgentType, Budget, Duration, Email, ModelRef, Principal,
    };
    use tower::ServiceExt;
    use uuid::Uuid;

    use super::{
        AdminContext, AdmissionContext, AdmissionLedger, BoxFuture, BudgetIncrease,
        RuntimeRepository, SubmissionOutcome, router, submit_budget_increase,
    };

    #[derive(Clone)]
    struct FakeRuntimeRepository {
        runtime: Arc<Mutex<AgentRuntime>>,
    }

    impl RuntimeRepository for FakeRuntimeRepository {
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
        envelope: Envelope,
        parked: ParkedRows,
    }

    type ParkedRows = Arc<Mutex<Vec<(String, Vec<AdmissionDelta>, AgentRuntimeSpec)>>>;

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
            Box::pin(async move { Ok(Some(self.envelope.clone())) })
        }

        fn park_rejection<'a>(
            &'a self,
            request: ParkRejection<'a>,
        ) -> BoxFuture<'a, Result<ParkedAdmission, StoreError>> {
            Box::pin(async move {
                self.parked
                    .lock()
                    .map_err(|_| StoreError::Database("fake ledger lock was poisoned".to_owned()))?
                    .push((
                        request.runtime_uid.to_owned(),
                        request.deltas.to_vec(),
                        request.proposed_spec.clone(),
                    ));
                Ok(ParkedAdmission {
                    decision_id: Uuid::nil(),
                    approval_id: Uuid::nil(),
                })
            })
        }

        fn pending_approvals(&self) -> BoxFuture<'_, Result<Vec<PendingApproval>, StoreError>> {
            Box::pin(async move {
                self.parked
                    .lock()
                    .map_err(|_| StoreError::Database("fake ledger lock was poisoned".to_owned()))
                    .map(|rows| {
                        rows.iter()
                            .map(|(runtime_uid, deltas, proposed_spec)| PendingApproval {
                                approval_id: Uuid::nil(),
                                decision_id: Uuid::nil(),
                                runtime_uid: runtime_uid.clone(),
                                deltas: deltas.clone(),
                                proposed_spec: proposed_spec.clone(),
                                actor: "alice@example.com".to_owned(),
                                member_role: "engineer".to_owned(),
                            })
                            .collect()
                    })
            })
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
        runtime
    }

    fn ledger() -> FakeLedger {
        FakeLedger {
            envelope: Envelope {
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
            },
            parked: Arc::new(Mutex::new(Vec::new())),
        }
    }

    #[tokio::test]
    async fn sequential_edits_are_admitted_against_the_composed_absolute_manifest()
    -> Result<(), String> {
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let ledger = ledger();
        let context = AdmissionContext {
            actor: "alice@example.com".to_owned(),
            member_role: "engineer".to_owned(),
        };
        let edit = BudgetIncrease {
            amount: "60.00".to_owned(),
        };

        let first =
            submit_budget_increase(&runtimes, &ledger, &context, "team-a", "runtime-a", &edit)
                .await;
        assert!(
            matches!(first, Ok(SubmissionOutcome::Applied { .. })),
            "the first edit should compose to 160.00 and remain inside the envelope: {first:?}"
        );
        let second =
            submit_budget_increase(&runtimes, &ledger, &context, "team-a", "runtime-a", &edit)
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
        assert_eq!(parked[0].0, "runtime-uid-a");
        assert_eq!(parked[0].1, vec![expected_delta]);
        assert_eq!(parked[0].2.budget.monthly_limit, "220.00");
        assert_eq!(
            steward_admission::evaluate(&parked[0].2, &ledger.envelope),
            Ok(AdmissionDecision::Reject {
                deltas: parked[0].1.clone()
            })
        );
        Ok(())
    }

    #[tokio::test]
    async fn rest_route_returns_the_parked_shared_counterexample() -> Result<(), String> {
        let runtimes = FakeRuntimeRepository {
            runtime: Arc::new(Mutex::new(runtime())),
        };
        let ledger = ledger();
        let context = AdmissionContext {
            actor: "alice@example.com".to_owned(),
            member_role: "engineer".to_owned(),
        };
        let envelope_body = serde_json::to_vec(&ledger.envelope)
            .map_err(|error| format!("failed to serialize envelope: {error}"))?;
        let app = router(runtimes, ledger)
            .layer(Extension(context))
            .layer(Extension(AdminContext {
                actor: "admin@example.com".to_owned(),
            }));
        let authored = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/envelopes/engineer")
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
        let queue = app
            .oneshot(
                Request::builder()
                    .uri("/admin/approvals")
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
        ] {
            assert!(
                queue_html.contains(expected),
                "approval queue must render {expected:?} from the parked row"
            );
        }
        Ok(())
    }
}
