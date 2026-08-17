//! Browser-session-bound user envelope request API.

use std::hash::Hash;

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use steward_admission::{AdmissionDecision, Envelope, evaluate, validate_envelope};
use steward_types::{AgentRuntimeSpec, AgentType, CanonicalUserId, Email, Principal};
use uuid::Uuid;

use crate::BoxFuture;
use crate::browser_auth::{
    BrowserAuthService, BrowserMutationProof, BrowserSessionBinding, BrowserSessionContext,
    protect_browser_routes,
};

pub const ENVELOPE_REQUESTS_API_VERSION: &str = "steward.envelope-requests/v1";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserEnvelopeSubject {
    pub canonical_user_id: CanonicalUserId,
    pub display_email: Email,
    pub member_roles: Vec<String>,
}

/// The session binding is opaque to the request domain. It is only used by a broker to bind
/// one-time actions to the same browser session that initiated them.
#[derive(Clone, Eq, PartialEq)]
pub struct UserEnvelopeSession<B> {
    pub subject: UserEnvelopeSubject,
    pub binding: B,
}

#[derive(Clone, Copy)]
struct UserEnvelopeMutationProof;

fn subject_from_browser_session(context: &BrowserSessionContext) -> UserEnvelopeSubject {
    UserEnvelopeSubject {
        canonical_user_id: context.principal.canonical_user_id.clone(),
        display_email: context.principal.display_email.clone(),
        member_roles: context.principal.member_roles.clone(),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionReadiness {
    Connected,
    ReauthRequired,
    Missing,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableEnvelopeTemplate {
    pub id: String,
    pub display_name: String,
    pub revision: i64,
    pub ceiling: Envelope,
    pub auto_provision_threshold: Envelope,
    pub github_connection: ConnectionReadiness,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvelopeRequestStatus {
    Pending,
    Approved,
    Rejected,
    Provisioned,
    Stale,
    Conflict,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserEnvelopeRequest {
    pub id: Uuid,
    pub template_id: String,
    pub template_revision: i64,
    pub requested_envelope: Envelope,
    pub approved_envelope: Option<Envelope>,
    pub status: EnvelopeRequestStatus,
    pub approval_id: Option<Uuid>,
    pub envelope_instance_id: Option<String>,
    pub envelope_digest: Option<String>,
    pub reason: Option<String>,
    pub created_at: String,
    pub status_at: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateEnvelopeRequestBody {
    template_id: String,
    template_revision: i64,
    requested_envelope: Envelope,
    idempotency_key: String,
}

/// Submission passed only after the HTTP boundary has derived its canonical owner, required an
/// exact template revision, verified the hard ceiling, and decided the auto-provision threshold.
pub struct ValidatedEnvelopeRequest<'a> {
    pub template: &'a AvailableEnvelopeTemplate,
    pub requested_envelope: &'a Envelope,
    pub idempotency_key: &'a str,
    pub auto_provision: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnvelopeRequestBrokerError {
    NotFound,
    Conflict,
    Unavailable,
}

/// Server-side request broker. Every method receives the canonical browser subject derived by
/// middleware; no method accepts a browser-supplied owner or provider credential.
pub trait EnvelopeRequestBroker<B>: Clone + Send + Sync + 'static
where
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    fn templates<'a>(
        &'a self,
        session: &'a UserEnvelopeSession<B>,
    ) -> BoxFuture<'a, Result<Vec<AvailableEnvelopeTemplate>, EnvelopeRequestBrokerError>>;

    fn list<'a>(
        &'a self,
        session: &'a UserEnvelopeSession<B>,
    ) -> BoxFuture<'a, Result<Vec<UserEnvelopeRequest>, EnvelopeRequestBrokerError>>;

    fn get<'a>(
        &'a self,
        session: &'a UserEnvelopeSession<B>,
        request_id: Uuid,
    ) -> BoxFuture<'a, Result<Option<UserEnvelopeRequest>, EnvelopeRequestBrokerError>>;

    fn create<'a>(
        &'a self,
        session: &'a UserEnvelopeSession<B>,
        request: ValidatedEnvelopeRequest<'a>,
    ) -> BoxFuture<'a, Result<UserEnvelopeRequest, EnvelopeRequestBrokerError>>;
}

#[derive(Clone)]
struct UserEnvelopeState<P> {
    broker: P,
}

fn inner_router<P, B>(broker: P) -> Router
where
    P: EnvelopeRequestBroker<B>,
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    Router::new()
        .route(
            "/app/api/v1/envelope-templates",
            get(list_templates::<P, B>),
        )
        .route(
            "/app/api/v1/envelope-requests",
            get(list_requests::<P, B>).post(create_request::<P, B>),
        )
        .route(
            "/app/api/v1/envelope-requests/{request_id}",
            get(get_request::<P, B>),
        )
        .with_state(UserEnvelopeState { broker })
}

/// Mount user envelope APIs behind the common browser-session, same-origin, CSRF and
/// fetch-metadata boundary. User pages only learn authoritative records returned by their broker.
pub fn protected_router<P>(broker: P, browser_auth: BrowserAuthService) -> Router
where
    P: EnvelopeRequestBroker<BrowserSessionBinding>,
{
    let routes = inner_router(broker).route_layer(middleware::from_fn(adapt_browser_context));
    protect_browser_routes(routes, browser_auth)
}

async fn adapt_browser_context(mut request: Request, next: Next) -> Response {
    if let Some(context) = request.extensions().get::<BrowserSessionContext>().cloned() {
        request.extensions_mut().insert(UserEnvelopeSession {
            subject: subject_from_browser_session(&context),
            binding: context.binding,
        });
        if request.extensions().get::<BrowserMutationProof>().is_some() {
            request.extensions_mut().insert(UserEnvelopeMutationProof);
        }
    }
    next.run(request).await
}

async fn list_templates<P, B>(
    session: Option<Extension<UserEnvelopeSession<B>>>,
    State(state): State<UserEnvelopeState<P>>,
) -> Response
where
    P: EnvelopeRequestBroker<B>,
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    match state.broker.templates(&session).await {
        Ok(templates) => Json(serde_json::json!({
            "apiVersion": ENVELOPE_REQUESTS_API_VERSION,
            "templates": templates,
        }))
        .into_response(),
        Err(error) => broker_error_response(error),
    }
}

async fn list_requests<P, B>(
    session: Option<Extension<UserEnvelopeSession<B>>>,
    State(state): State<UserEnvelopeState<P>>,
) -> Response
where
    P: EnvelopeRequestBroker<B>,
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    match state.broker.list(&session).await {
        Ok(requests) => Json(serde_json::json!({
            "apiVersion": ENVELOPE_REQUESTS_API_VERSION,
            "requests": requests,
        }))
        .into_response(),
        Err(error) => broker_error_response(error),
    }
}

async fn get_request<P, B>(
    session: Option<Extension<UserEnvelopeSession<B>>>,
    State(state): State<UserEnvelopeState<P>>,
    Path(request_id): Path<Uuid>,
) -> Response
where
    P: EnvelopeRequestBroker<B>,
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    match state.broker.get(&session, request_id).await {
        Ok(Some(request)) => Json(serde_json::json!({
            "apiVersion": ENVELOPE_REQUESTS_API_VERSION,
            "request": request,
        }))
        .into_response(),
        Ok(None) | Err(EnvelopeRequestBrokerError::NotFound) => {
            StatusCode::NOT_FOUND.into_response()
        }
        Err(error) => broker_error_response(error),
    }
}

async fn create_request<P, B>(
    session: Option<Extension<UserEnvelopeSession<B>>>,
    proof: Option<Extension<UserEnvelopeMutationProof>>,
    State(state): State<UserEnvelopeState<P>>,
    Json(body): Json<CreateEnvelopeRequestBody>,
) -> Response
where
    P: EnvelopeRequestBroker<B>,
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if proof.is_none() {
        return StatusCode::FORBIDDEN.into_response();
    }
    if body.idempotency_key.is_empty() || body.idempotency_key.len() > 200 {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let templates = match state.broker.templates(&session).await {
        Ok(templates) => templates,
        Err(error) => return broker_error_response(error),
    };
    let Some(template) = templates.iter().find(|template| {
        template.id == body.template_id && template.revision == body.template_revision
    }) else {
        return StatusCode::CONFLICT.into_response();
    };
    if template.github_connection != ConnectionReadiness::Connected {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "apiVersion": ENVELOPE_REQUESTS_API_VERSION,
                "error": "required GitHub connection is not ready",
                "connectionsPath": "/admin/connections",
            })),
        )
            .into_response();
    }
    if body.requested_envelope.revision != template.revision
        || validate_envelope(&body.requested_envelope).is_err()
    {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    }
    let request_spec = envelope_as_user_runtime(&body.requested_envelope, &session.subject);
    let inside_ceiling = matches!(
        evaluate(&request_spec, &template.ceiling),
        Ok(AdmissionDecision::Admit)
    );
    if !inside_ceiling {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    }
    let auto_provision = matches!(
        evaluate(&request_spec, &template.auto_provision_threshold),
        Ok(AdmissionDecision::Admit)
    );
    match state
        .broker
        .create(
            &session,
            ValidatedEnvelopeRequest {
                template,
                requested_envelope: &body.requested_envelope,
                idempotency_key: &body.idempotency_key,
                auto_provision,
            },
        )
        .await
    {
        Ok(request) => (
            StatusCode::CREATED,
            Json(serde_json::json!({
                "apiVersion": ENVELOPE_REQUESTS_API_VERSION,
                "request": request,
            })),
        )
            .into_response(),
        Err(error) => broker_error_response(error),
    }
}

fn envelope_as_user_runtime(
    envelope: &Envelope,
    subject: &UserEnvelopeSubject,
) -> AgentRuntimeSpec {
    AgentRuntimeSpec {
        principal: Principal::User {
            acting_user: subject.display_email.clone(),
        },
        owner: subject.display_email.clone(),
        canonical_authority: None,
        agent_type: AgentType {
            name: "user-envelope-request".to_owned(),
        },
        llms: envelope.spec.llms.clone(),
        tools: envelope.spec.tools.clone(),
        budget: envelope.spec.budget.clone(),
        ttl: envelope.spec.ttl.clone(),
        runner: envelope.spec.runner.clone(),
        bindings: None,
    }
}

fn broker_error_response(error: EnvelopeRequestBrokerError) -> Response {
    match error {
        EnvelopeRequestBrokerError::NotFound => StatusCode::NOT_FOUND.into_response(),
        EnvelopeRequestBrokerError::Conflict => StatusCode::CONFLICT.into_response(),
        EnvelopeRequestBrokerError::Unavailable => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use steward_types::{Budget, Duration, ModelRef, ToolGrant};
    use tower::ServiceExt;

    use super::{
        AvailableEnvelopeTemplate, ConnectionReadiness, EnvelopeRequestBroker,
        EnvelopeRequestBrokerError, EnvelopeRequestStatus, UserEnvelopeMutationProof,
        UserEnvelopeRequest, UserEnvelopeSession, UserEnvelopeSubject, ValidatedEnvelopeRequest,
        inner_router,
    };
    use crate::BoxFuture;
    use steward_admission::{Envelope, EnvelopeSpec};
    use steward_types::{CanonicalUserId, Email};
    use uuid::Uuid;

    #[derive(Clone, Default)]
    struct TestBroker {
        create_owners: Arc<Mutex<Vec<CanonicalUserId>>>,
    }

    impl EnvelopeRequestBroker<()> for TestBroker {
        fn templates<'a>(
            &'a self,
            _session: &'a UserEnvelopeSession<()>,
        ) -> BoxFuture<'a, Result<Vec<AvailableEnvelopeTemplate>, EnvelopeRequestBrokerError>>
        {
            Box::pin(async { Ok(vec![template()]) })
        }

        fn list<'a>(
            &'a self,
            _session: &'a UserEnvelopeSession<()>,
        ) -> BoxFuture<'a, Result<Vec<UserEnvelopeRequest>, EnvelopeRequestBrokerError>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn get<'a>(
            &'a self,
            _session: &'a UserEnvelopeSession<()>,
            _request_id: Uuid,
        ) -> BoxFuture<'a, Result<Option<UserEnvelopeRequest>, EnvelopeRequestBrokerError>>
        {
            Box::pin(async { Ok(None) })
        }

        fn create<'a>(
            &'a self,
            session: &'a UserEnvelopeSession<()>,
            request: ValidatedEnvelopeRequest<'a>,
        ) -> BoxFuture<'a, Result<UserEnvelopeRequest, EnvelopeRequestBrokerError>> {
            let owners = self.create_owners.clone();
            let owner = session.subject.canonical_user_id.clone();
            let template_id = request.template.id.clone();
            let template_revision = request.template.revision;
            let requested_envelope = request.requested_envelope.clone();
            let auto_provision = request.auto_provision;
            let approved_envelope = auto_provision.then(|| requested_envelope.clone());
            let status = if auto_provision {
                EnvelopeRequestStatus::Provisioned
            } else {
                EnvelopeRequestStatus::Pending
            };
            Box::pin(async move {
                owners
                    .lock()
                    .map_err(|_| EnvelopeRequestBrokerError::Unavailable)?
                    .push(owner);
                Ok(UserEnvelopeRequest {
                    id: Uuid::nil(),
                    template_id,
                    template_revision,
                    requested_envelope,
                    approved_envelope,
                    status,
                    approval_id: None,
                    envelope_instance_id: Some("env_local_test".to_owned()),
                    envelope_digest: Some("sha256:local-test".to_owned()),
                    reason: None,
                    created_at: "2026-08-17T00:00:00Z".to_owned(),
                    status_at: "2026-08-17T00:00:00Z".to_owned(),
                })
            })
        }
    }

    fn template() -> AvailableEnvelopeTemplate {
        let ceiling = Envelope {
            revision: 3,
            spec: EnvelopeSpec {
                llms: vec![ModelRef {
                    provider: "openai".to_owned(),
                    model: "gpt-5.4".to_owned(),
                }],
                tools: vec![ToolGrant {
                    provider: "github".to_owned(),
                    resource: "repository".to_owned(),
                    action: "get_file_contents".to_owned(),
                }],
                budget: Budget {
                    monthly_limit: "100.00".to_owned(),
                    currency: "USD".to_owned(),
                },
                ttl: Duration("72h".to_owned()),
                runner: steward_types::RunnerRequirements::default(),
            },
        };
        AvailableEnvelopeTemplate {
            id: "engineer".to_owned(),
            display_name: "Engineer".to_owned(),
            revision: 3,
            auto_provision_threshold: Envelope {
                revision: 3,
                spec: EnvelopeSpec {
                    budget: Budget {
                        monthly_limit: "50.00".to_owned(),
                        currency: "USD".to_owned(),
                    },
                    ttl: Duration("24h".to_owned()),
                    runner: steward_types::RunnerRequirements::default(),
                    ..ceiling.spec.clone()
                },
            },
            ceiling,
            github_connection: ConnectionReadiness::Connected,
        }
    }

    fn session() -> Result<UserEnvelopeSession<()>, String> {
        Ok(UserEnvelopeSession {
            subject: UserEnvelopeSubject {
                canonical_user_id: CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?,
                display_email: Email::parse("alice@example.com")?,
                member_roles: Vec::new(),
            },
            binding: (),
        })
    }

    #[tokio::test]
    async fn create_uses_the_authenticated_canonical_owner_and_exact_template_revision()
    -> Result<(), String> {
        let broker = TestBroker::default();
        let app = inner_router(broker.clone());
        let request = Request::builder()
            .method("POST")
            .uri("/app/api/v1/envelope-requests")
            .header("content-type", "application/json")
            .extension(session()?)
            .extension(UserEnvelopeMutationProof)
            .body(Body::from(
                serde_json::json!({
                    "templateId": "engineer",
                    "templateRevision": 3,
                    "requestedEnvelope": template().auto_provision_threshold,
                    "idempotencyKey": "request-1",
                })
                .to_string(),
            ))
            .map_err(|error| format!("build request: {error}"))?;
        let response = app
            .oneshot(request)
            .await
            .map_err(|error| format!("submit request: {error}"))?;
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = to_bytes(response.into_body(), 16 * 1024)
            .await
            .map_err(|error| format!("read response: {error}"))?;
        let response: serde_json::Value =
            serde_json::from_slice(&body).map_err(|error| format!("parse response: {error}"))?;
        assert_eq!(response["request"]["templateRevision"], 3);
        assert_eq!(response["request"]["status"], "provisioned");
        let owners = broker
            .create_owners
            .lock()
            .map_err(|_| "read broker owners".to_owned())?;
        assert_eq!(owners.len(), 1);
        assert_eq!(owners[0].as_str(), "usr_0123456789abcdef0123456789abcdef");
        Ok(())
    }

    #[tokio::test]
    async fn unauthenticated_or_out_of_ceiling_requests_fail_closed() -> Result<(), String> {
        let app = inner_router(TestBroker::default());
        let anonymous = Request::builder()
            .uri("/app/api/v1/envelope-requests")
            .body(Body::empty())
            .map_err(|error| format!("build anonymous request: {error}"))?;
        assert_eq!(
            app.clone()
                .oneshot(anonymous)
                .await
                .map_err(|error| format!("send anonymous request: {error}"))?
                .status(),
            StatusCode::UNAUTHORIZED
        );

        let mut excessive = template().ceiling;
        excessive.spec.budget.monthly_limit = "1000.00".to_owned();
        let request = Request::builder()
            .method("POST")
            .uri("/app/api/v1/envelope-requests")
            .header("content-type", "application/json")
            .extension(session()?)
            .extension(UserEnvelopeMutationProof)
            .body(Body::from(
                serde_json::json!({
                    "templateId": "engineer",
                    "templateRevision": 3,
                    "requestedEnvelope": excessive,
                    "idempotencyKey": "request-over-ceiling",
                })
                .to_string(),
            ))
            .map_err(|error| format!("build excessive request: {error}"))?;
        assert_eq!(
            app.oneshot(request)
                .await
                .map_err(|error| format!("send excessive request: {error}"))?
                .status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        Ok(())
    }
}
