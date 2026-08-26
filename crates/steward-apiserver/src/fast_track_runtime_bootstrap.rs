//! FAST-TRACK / NON-PROMOTABLE legacy runtime bootstrap for the DEV preview.
//!
//! This is deliberately a compatibility seam for the current DEV controller and Mint v2. The
//! browser caller supplies no authority or capability fields: one verified Google session is
//! bound to one bounded, server-authored, legacy email AgentRuntime.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Extension, Json, Router};
use kube::ResourceExt;
use serde::{Deserialize, Serialize};
use steward_admission::{AdmissionDecision, Envelope, EnvelopeSpec};
use steward_types::{
    AgentRuntime, AgentRuntimeSpec, AgentType, Budget, Duration, Email, Phase, Principal,
    RunnerRequirements, ToolGrant,
};

use crate::browser_auth::{
    BrowserAuthService, BrowserMutationProof, BrowserSessionBinding, BrowserSessionContext,
    protect_browser_routes,
};
use crate::{BoxFuture, RuntimeCreateError, RuntimeRepository};

pub const FAST_TRACK_RUNTIME_BOOTSTRAP_PATH: &str = "/admin/api/v1/fast-track/connections/runtime";
pub const FAST_TRACK_RUNTIME_NAMESPACE: &str = "lbe259-fast-track";
pub const FAST_TRACK_RUNTIME_NAME: &str = "connections-bridge";
pub const FAST_TRACK_SERVICE_PRINCIPAL: &str = "steward-run";

const SERVICE_PRINCIPAL_ANNOTATION: &str = "agents.apelogic.ai/service-principal";
const FIXED_TTL: &str = "1h";
const FIXED_TOOL_PROVIDER: &str = "github";
const FIXED_TOOL_RESOURCE: &str = "github_oauth_start";
const FIXED_TOOL_ACTION: &str = "write";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BootstrapRequest {}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BootstrapResponse {
    runtime_id: &'static str,
    status: &'static str,
}

#[derive(Clone, Eq, PartialEq)]
struct SessionAuthority {
    binding: BrowserSessionBinding,
    email: Email,
}

pub trait FastTrackRuntimeRepository: Clone + Send + Sync + 'static {
    fn create_as_authority<'a>(
        &'a self,
        namespace: &'a str,
        runtime: &'a AgentRuntime,
    ) -> BoxFuture<'a, Result<AgentRuntime, RuntimeCreateError>>;

    fn get<'a>(
        &'a self,
        namespace: &'a str,
        name: &'a str,
    ) -> BoxFuture<'a, Result<AgentRuntime, String>>;
}

impl<R> FastTrackRuntimeRepository for R
where
    R: RuntimeRepository,
{
    fn create_as_authority<'a>(
        &'a self,
        namespace: &'a str,
        runtime: &'a AgentRuntime,
    ) -> BoxFuture<'a, Result<AgentRuntime, RuntimeCreateError>> {
        RuntimeRepository::create_as_authority(self, namespace, runtime)
    }

    fn get<'a>(
        &'a self,
        namespace: &'a str,
        name: &'a str,
    ) -> BoxFuture<'a, Result<AgentRuntime, String>> {
        RuntimeRepository::get(self, namespace, name)
    }
}

#[derive(Clone)]
pub struct FastTrackRuntimeBootstrap<R> {
    runtimes: R,
    session_authority: Arc<Mutex<Option<SessionAuthority>>>,
}

impl<R> FastTrackRuntimeBootstrap<R>
where
    R: FastTrackRuntimeRepository,
{
    pub fn new(runtimes: R) -> Self {
        Self {
            runtimes,
            session_authority: Arc::new(Mutex::new(None)),
        }
    }

    fn bind_session(&self, context: &BrowserSessionContext) -> Result<(), BootstrapError> {
        let requested = SessionAuthority {
            binding: context.binding.clone(),
            email: context.principal.display_email.clone(),
        };
        let mut current = self
            .session_authority
            .lock()
            .map_err(|_| BootstrapError::Unavailable)?;
        match current.as_ref() {
            Some(bound) if bound == &requested => Ok(()),
            Some(_) => Err(BootstrapError::SessionConflict),
            None => {
                *current = Some(requested);
                Ok(())
            }
        }
    }

    async fn ensure(
        &self,
        context: &BrowserSessionContext,
    ) -> Result<(StatusCode, BootstrapResponse), BootstrapError> {
        self.bind_session(context)?;
        self.ensure_runtime(context.principal.display_email.clone())
            .await
    }

    async fn ensure_runtime(
        &self,
        email: Email,
    ) -> Result<(StatusCode, BootstrapResponse), BootstrapError> {
        let runtime = fixed_runtime(email);
        enforce_fixed_admission(&runtime.spec)?;
        match self
            .runtimes
            .create_as_authority(FAST_TRACK_RUNTIME_NAMESPACE, &runtime)
            .await
        {
            Ok(created) => Ok((StatusCode::CREATED, response(&created))),
            Err(RuntimeCreateError::Kubernetes { status: 409, .. }) => {
                let existing = self
                    .runtimes
                    .get(FAST_TRACK_RUNTIME_NAMESPACE, FAST_TRACK_RUNTIME_NAME)
                    .await
                    .map_err(|_| BootstrapError::Unavailable)?;
                if !matches_fixed_runtime(&existing, &runtime) {
                    return Err(BootstrapError::RuntimeConflict);
                }
                Ok((StatusCode::OK, response(&existing)))
            }
            Err(RuntimeCreateError::Kubernetes { .. })
            | Err(RuntimeCreateError::Unavailable(_)) => Err(BootstrapError::Unavailable),
        }
    }

    /// Prewarms the fixed DEV runtime from the server's configured compatibility identity.
    ///
    /// This has no public route and accepts no browser or request fields. The same exact-match
    /// create-or-get boundary used by the protected endpoint remains authoritative.
    pub async fn prewarm(&self, compatibility_email: Email) -> Result<(), String> {
        self.ensure_runtime(compatibility_email)
            .await
            .map(|_| ())
            .map_err(|_| "prewarm fixed fast-track runtime".to_owned())
    }
}

pub fn protected_router<R>(
    bootstrap: FastTrackRuntimeBootstrap<R>,
    browser_auth: BrowserAuthService,
) -> Router
where
    R: FastTrackRuntimeRepository,
{
    protect_browser_routes(inner_router(bootstrap), browser_auth)
}

fn inner_router<R>(bootstrap: FastTrackRuntimeBootstrap<R>) -> Router
where
    R: FastTrackRuntimeRepository,
{
    Router::new()
        .route(
            FAST_TRACK_RUNTIME_BOOTSTRAP_PATH,
            post(bootstrap_runtime::<R>),
        )
        .with_state(bootstrap)
}

async fn bootstrap_runtime<R>(
    State(bootstrap): State<FastTrackRuntimeBootstrap<R>>,
    Extension(session): Extension<BrowserSessionContext>,
    Extension(_proof): Extension<BrowserMutationProof>,
    Json(_request): Json<BootstrapRequest>,
) -> Response
where
    R: FastTrackRuntimeRepository,
{
    match bootstrap.ensure(&session).await {
        Ok((status, body)) => (status, Json(body)).into_response(),
        Err(error) => error.into_response(),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BootstrapError {
    SessionConflict,
    RuntimeConflict,
    Admission,
    Unavailable,
}

impl IntoResponse for BootstrapError {
    fn into_response(self) -> Response {
        let (status, error) = match self {
            Self::SessionConflict => (StatusCode::CONFLICT, "preview_session_conflict"),
            Self::RuntimeConflict => (StatusCode::CONFLICT, "preview_runtime_conflict"),
            Self::Admission | Self::Unavailable => {
                (StatusCode::SERVICE_UNAVAILABLE, "preview_unavailable")
            }
        };
        (status, Json(serde_json::json!({ "error": error }))).into_response()
    }
}

fn fixed_runtime(email: Email) -> AgentRuntime {
    let spec = AgentRuntimeSpec {
        principal: Principal::Service {
            name: FAST_TRACK_SERVICE_PRINCIPAL.to_owned(),
            acting_user: Some(email.clone()),
        },
        owner: email,
        canonical_authority: None,
        agent_type: AgentType {
            name: "base".to_owned(),
        },
        llms: Vec::new(),
        tools: vec![ToolGrant {
            provider: FIXED_TOOL_PROVIDER.to_owned(),
            resource: FIXED_TOOL_RESOURCE.to_owned(),
            action: FIXED_TOOL_ACTION.to_owned(),
        }],
        budget: Budget {
            monthly_limit: "0.00".to_owned(),
            single_run_limit: None,
            currency: "USD".to_owned(),
        },
        ttl: Duration(FIXED_TTL.to_owned()),
        runner: RunnerRequirements::default(),
        bindings: None,
    };
    let mut runtime = AgentRuntime::new(FAST_TRACK_RUNTIME_NAME, spec);
    runtime.metadata.namespace = Some(FAST_TRACK_RUNTIME_NAMESPACE.to_owned());
    runtime.metadata.annotations = Some(BTreeMap::from([(
        SERVICE_PRINCIPAL_ANNOTATION.to_owned(),
        FAST_TRACK_SERVICE_PRINCIPAL.to_owned(),
    )]));
    runtime
}

fn enforce_fixed_admission(spec: &AgentRuntimeSpec) -> Result<(), BootstrapError> {
    let envelope = Envelope {
        revision: 1,
        spec: EnvelopeSpec {
            llms: spec.llms.clone(),
            tools: spec.tools.clone(),
            budget: spec.budget.clone(),
            ttl: spec.ttl.clone(),
            runner: spec.runner.clone(),
        },
    };
    match steward_admission::evaluate(spec, &envelope) {
        Ok(AdmissionDecision::Admit) => Ok(()),
        Ok(AdmissionDecision::Reject { .. }) | Err(_) => Err(BootstrapError::Admission),
    }
}

fn matches_fixed_runtime(existing: &AgentRuntime, desired: &AgentRuntime) -> bool {
    existing.name_any() == FAST_TRACK_RUNTIME_NAME
        && existing.namespace().as_deref() == Some(FAST_TRACK_RUNTIME_NAMESPACE)
        && existing.spec == desired.spec
        && existing
            .annotations()
            .get(SERVICE_PRINCIPAL_ANNOTATION)
            .map(String::as_str)
            == Some(FAST_TRACK_SERVICE_PRINCIPAL)
}

fn response(runtime: &AgentRuntime) -> BootstrapResponse {
    BootstrapResponse {
        runtime_id: "lbe259-fast-track/connections-bridge",
        status: runtime
            .status
            .as_ref()
            .map(|status| phase_name(status.phase))
            .unwrap_or("pending"),
    }
}

fn phase_name(phase: Phase) -> &'static str {
    match phase {
        Phase::Pending => "pending",
        Phase::Admitted => "admitted",
        Phase::Provisioning => "provisioning",
        Phase::Running => "running",
        Phase::Suspended => "suspended",
        Phase::Terminating => "terminating",
        Phase::Terminated => "terminated",
        Phase::Failed => "failed",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use kube::ResourceExt;
    use steward_types::{AgentRuntime, CanonicalUserId, Email, Principal};
    use tower::ServiceExt;

    use crate::browser_auth::{
        BrowserMutationProof, BrowserPrincipal, BrowserRole, BrowserSessionBinding,
        BrowserSessionContext, LocalFakeIdentity, local_fake_browser_auth_service,
    };
    use crate::{BoxFuture, RuntimeCreateError};

    use super::{
        FAST_TRACK_RUNTIME_BOOTSTRAP_PATH, FAST_TRACK_RUNTIME_NAME, FAST_TRACK_RUNTIME_NAMESPACE,
        FAST_TRACK_SERVICE_PRINCIPAL, FastTrackRuntimeBootstrap, FastTrackRuntimeRepository,
        fixed_runtime, inner_router, protected_router,
    };

    #[derive(Clone, Default)]
    struct FakeRuntimes {
        created: Arc<Mutex<Vec<(String, AgentRuntime)>>>,
    }

    impl FastTrackRuntimeRepository for FakeRuntimes {
        fn create_as_authority<'a>(
            &'a self,
            namespace: &'a str,
            runtime: &'a AgentRuntime,
        ) -> BoxFuture<'a, Result<AgentRuntime, RuntimeCreateError>> {
            Box::pin(async move {
                self.created
                    .lock()
                    .map_err(|_| {
                        RuntimeCreateError::Unavailable("poisoned fake runtime store".to_owned())
                    })?
                    .push((namespace.to_owned(), runtime.clone()));
                Ok(runtime.clone())
            })
        }

        fn get<'a>(
            &'a self,
            _namespace: &'a str,
            _name: &'a str,
        ) -> BoxFuture<'a, Result<AgentRuntime, String>> {
            Box::pin(async { Err("unexpected get".to_owned()) })
        }
    }

    #[derive(Clone)]
    struct ExistingRuntimes {
        existing: AgentRuntime,
        creates: Arc<Mutex<usize>>,
    }

    impl FastTrackRuntimeRepository for ExistingRuntimes {
        fn create_as_authority<'a>(
            &'a self,
            _namespace: &'a str,
            _runtime: &'a AgentRuntime,
        ) -> BoxFuture<'a, Result<AgentRuntime, RuntimeCreateError>> {
            Box::pin(async move {
                *self.creates.lock().map_err(|_| {
                    RuntimeCreateError::Unavailable("poisoned create count".to_owned())
                })? += 1;
                Err(RuntimeCreateError::Kubernetes {
                    status: 409,
                    message: "already exists".to_owned(),
                })
            })
        }

        fn get<'a>(
            &'a self,
            namespace: &'a str,
            name: &'a str,
        ) -> BoxFuture<'a, Result<AgentRuntime, String>> {
            Box::pin(async move {
                if namespace == FAST_TRACK_RUNTIME_NAMESPACE && name == FAST_TRACK_RUNTIME_NAME {
                    Ok(self.existing.clone())
                } else {
                    Err("unexpected runtime key".to_owned())
                }
            })
        }
    }

    fn session_with_binding(binding: &str) -> Result<BrowserSessionContext, String> {
        Ok(BrowserSessionContext {
            principal: BrowserPrincipal {
                canonical_user_id: CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?,
                display_name: "Alice Example".to_owned(),
                display_email: Email::parse("alice@example.com")?,
                role: BrowserRole::User,
                member_roles: Vec::new(),
            },
            binding: BrowserSessionBinding::from_test_value(binding),
        })
    }

    fn session() -> Result<BrowserSessionContext, String> {
        session_with_binding("session-a")
    }

    fn request(body: &str, session: BrowserSessionContext) -> Result<Request<Body>, String> {
        let mut request = Request::builder()
            .method("POST")
            .uri(FAST_TRACK_RUNTIME_BOOTSTRAP_PATH)
            .header("content-type", "application/json")
            .body(Body::from(body.to_owned()))
            .map_err(|error| error.to_string())?;
        request.extensions_mut().insert(session);
        request
            .extensions_mut()
            .insert(BrowserMutationProof::for_test());
        Ok(request)
    }

    #[tokio::test]
    async fn authenticated_empty_request_creates_sanitized_fixed_runtime() -> Result<(), String> {
        let runtimes = FakeRuntimes::default();
        let response = inner_router(FastTrackRuntimeBootstrap::new(runtimes.clone()))
            .oneshot(request("{}", session()?)?)
            .await
            .map_err(|error| error.to_string())?;

        assert_eq!(response.status(), axum::http::StatusCode::CREATED);
        let body = to_bytes(response.into_body(), 4096)
            .await
            .map_err(|error| error.to_string())?;
        let value: serde_json::Value =
            serde_json::from_slice(&body).map_err(|error| error.to_string())?;
        assert_eq!(
            value,
            serde_json::json!({
                "runtimeId": "lbe259-fast-track/connections-bridge",
                "status": "pending"
            })
        );

        let created = runtimes.created.lock().map_err(|_| "poisoned capture")?;
        let (namespace, runtime) = created.first().ok_or("runtime was not created")?;
        assert_eq!(namespace, FAST_TRACK_RUNTIME_NAMESPACE);
        assert_eq!(runtime.name_any(), FAST_TRACK_RUNTIME_NAME);
        assert_eq!(runtime.spec.owner.as_str(), "alice@example.com");
        assert!(matches!(
            &runtime.spec.principal,
            Principal::Service { name, acting_user: Some(acting_user) }
                if name == FAST_TRACK_SERVICE_PRINCIPAL
                    && acting_user.as_str() == "alice@example.com"
        ));
        assert!(runtime.spec.canonical_authority.is_none());
        assert!(runtime.spec.llms.is_empty());
        assert_eq!(runtime.spec.tools.len(), 1);
        assert_eq!(runtime.spec.tools[0].provider, "github");
        assert_eq!(runtime.spec.tools[0].resource, "github_oauth_start");
        assert_eq!(runtime.spec.tools[0].action, "write");
        assert_eq!(runtime.spec.ttl.0, "1h");
        assert_eq!(runtime.spec.budget.monthly_limit, "0.00");
        assert_eq!(
            runtime
                .annotations()
                .get("agents.apelogic.ai/service-principal")
                .map(String::as_str),
            Some(FAST_TRACK_SERVICE_PRINCIPAL)
        );
        assert!(
            !runtime
                .annotations()
                .contains_key("agents.apelogic.ai/member-role")
        );
        Ok(())
    }

    #[tokio::test]
    async fn startup_prewarm_authors_the_fixed_runtime_without_a_browser_request()
    -> Result<(), String> {
        let runtimes = FakeRuntimes::default();
        let bootstrap = FastTrackRuntimeBootstrap::new(runtimes.clone());

        bootstrap
            .prewarm(Email::parse("alice@example.com")?)
            .await
            .map_err(|_| "prewarm failed")?;

        let created = runtimes.created.lock().map_err(|_| "poisoned capture")?;
        let (namespace, runtime) = created.first().ok_or("runtime was not prewarmed")?;
        assert_eq!(namespace, FAST_TRACK_RUNTIME_NAMESPACE);
        assert_eq!(runtime.name_any(), FAST_TRACK_RUNTIME_NAME);
        assert_eq!(runtime.spec.owner.as_str(), "alice@example.com");
        assert_eq!(runtime.spec.ttl.0, "1h");
        assert!(matches!(
            &runtime.spec.principal,
            Principal::Service { name, acting_user: Some(acting_user) }
                if name == FAST_TRACK_SERVICE_PRINCIPAL
                    && acting_user.as_str() == "alice@example.com"
        ));
        Ok(())
    }

    #[tokio::test]
    async fn request_body_cannot_override_any_runtime_authority_or_capability() -> Result<(), String>
    {
        let runtimes = FakeRuntimes::default();
        let response = inner_router(FastTrackRuntimeBootstrap::new(runtimes.clone()))
            .oneshot(request(
                r#"{"email":"mallory@example.com","ttl":"24h","tools":[]}"#,
                session()?,
            )?)
            .await
            .map_err(|error| error.to_string())?;

        assert_eq!(
            response.status(),
            axum::http::StatusCode::UNPROCESSABLE_ENTITY
        );
        assert!(
            runtimes
                .created
                .lock()
                .map_err(|_| "poisoned capture")?
                .is_empty()
        );
        Ok(())
    }

    #[tokio::test]
    async fn same_browser_session_is_idempotent_for_exact_existing_runtime() -> Result<(), String> {
        let creates = Arc::new(Mutex::new(0));
        let repository = ExistingRuntimes {
            existing: fixed_runtime(Email::parse("alice@example.com")?),
            creates: creates.clone(),
        };
        let app = inner_router(FastTrackRuntimeBootstrap::new(repository));

        for _ in 0..2 {
            let response = app
                .clone()
                .oneshot(request("{}", session()?)?)
                .await
                .map_err(|error| error.to_string())?;
            assert_eq!(response.status(), axum::http::StatusCode::OK);
            let body = to_bytes(response.into_body(), 4096)
                .await
                .map_err(|error| error.to_string())?;
            let value: serde_json::Value =
                serde_json::from_slice(&body).map_err(|error| error.to_string())?;
            assert_eq!(value["runtimeId"], "lbe259-fast-track/connections-bridge");
            assert_eq!(value["status"], "pending");
            assert_eq!(value.as_object().map(|fields| fields.len()), Some(2));
        }
        assert_eq!(*creates.lock().map_err(|_| "poisoned create count")?, 2);
        Ok(())
    }

    #[tokio::test]
    async fn different_browser_session_cannot_take_over_process_binding() -> Result<(), String> {
        let runtimes = FakeRuntimes::default();
        let app = inner_router(FastTrackRuntimeBootstrap::new(runtimes.clone()));
        let first = app
            .clone()
            .oneshot(request("{}", session_with_binding("session-a")?)?)
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(first.status(), axum::http::StatusCode::CREATED);

        let second = app
            .oneshot(request("{}", session_with_binding("session-b")?)?)
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(second.status(), axum::http::StatusCode::CONFLICT);
        assert_eq!(
            runtimes
                .created
                .lock()
                .map_err(|_| "poisoned capture")?
                .len(),
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn mismatched_existing_runtime_fails_closed() -> Result<(), String> {
        let mut existing = fixed_runtime(Email::parse("alice@example.com")?);
        existing.spec.ttl = steward_types::Duration("30m".to_owned());
        let repository = ExistingRuntimes {
            existing,
            creates: Arc::new(Mutex::new(0)),
        };
        let response = inner_router(FastTrackRuntimeBootstrap::new(repository))
            .oneshot(request("{}", session()?)?)
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
        Ok(())
    }

    #[tokio::test]
    async fn protected_endpoint_rejects_missing_google_browser_session() -> Result<(), String> {
        let browser_auth =
            local_fake_browser_auth_service("http://127.0.0.1:33001", LocalFakeIdentity::User)?;
        let response = protected_router(
            FastTrackRuntimeBootstrap::new(FakeRuntimes::default()),
            browser_auth,
        )
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(FAST_TRACK_RUNTIME_BOOTSTRAP_PATH)
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .map_err(|error| error.to_string())?,
        )
        .await
        .map_err(|error| error.to_string())?;
        assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
        Ok(())
    }
}
