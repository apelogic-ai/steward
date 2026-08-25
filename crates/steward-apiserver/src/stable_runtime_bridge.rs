//! Stable bridge resolution for controller-owned runtimes.
//!
//! The browser never selects a runtime.  The server-owned task ledger resolves the one active
//! runtime for a canonical user and configured service, after which the runtime is re-read with
//! its exact Kubernetes UID.  A route may be stable while the underlying sandbox is replaced,
//! but it is unavailable until the replacement is controller-observed as running.

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use steward_adapter_github_artifact::GitHubArtifactVerifier as SharedGitHubArtifactVerifier;
use steward_store::{ActiveTaskRuntime, PgStore};
use steward_types::CanonicalUserId;

use crate::{
    BoxFuture, RuntimeRepository,
    browser_auth::{BrowserAuthService, BrowserSessionContext, protect_browser_routes},
};

/// Session-protected stable bridge resolution route. The configured service and authenticated
/// browser principal are the only inputs; callers cannot select a runtime by name or UID.
pub const STABLE_RUNTIME_BRIDGE_PATH: &str = "/app/api/v1/stable-runtime-bridge";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BridgeService(String);

impl BridgeService {
    pub fn new(value: String) -> Result<Self, &'static str> {
        if value.trim().is_empty() {
            return Err("bridge service must be non-empty");
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Server-authoritative identity of a task runtime.  This value is never decoded from an HTTP
/// request; the task ledger derives it from an authenticated principal and configured service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BridgeRuntimeReference {
    namespace: String,
    name: String,
    uid: String,
}

impl BridgeRuntimeReference {
    pub fn new(namespace: String, name: String, uid: String) -> Result<Self, &'static str> {
        if namespace.trim().is_empty() || name.trim().is_empty() || uid.trim().is_empty() {
            return Err("active task runtime reference must be complete");
        }
        Ok(Self {
            namespace,
            name,
            uid,
        })
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn uid(&self) -> &str {
        &self.uid
    }
}

/// The task-lifecycle owner selects the caller's active approved runtime.  It must return
/// `None` unless exactly one non-finalized running task is bound to this owner and service.
pub trait ActiveTaskRuntimeSource: Clone + Send + Sync + 'static {
    fn active_task_runtime<'a>(
        &'a self,
        owner: &'a CanonicalUserId,
        service: &'a BridgeService,
    ) -> BoxFuture<'a, Result<Option<BridgeRuntimeReference>, StableBridgeError>>;
}

fn bridge_runtime_reference(
    active: ActiveTaskRuntime,
) -> Result<BridgeRuntimeReference, StableBridgeError> {
    BridgeRuntimeReference::new(
        active.runtime_namespace,
        active.runtime_name,
        active.runtime_uid,
    )
    .map_err(|_| StableBridgeError::Unavailable)
}

impl ActiveTaskRuntimeSource for PgStore {
    fn active_task_runtime<'a>(
        &'a self,
        owner: &'a CanonicalUserId,
        service: &'a BridgeService,
    ) -> BoxFuture<'a, Result<Option<BridgeRuntimeReference>, StableBridgeError>> {
        Box::pin(async move {
            PgStore::active_task_runtime(self, owner, service.as_str())
                .await
                .map_err(|_| StableBridgeError::Unavailable)?
                .map(bridge_runtime_reference)
                .transpose()
        })
    }
}

/// An immutable bridge artifact that was verified by an injected provenance verifier.  There is
/// intentionally no pod exec or copy capability in this interface.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedBridgeArtifact {
    image_reference: String,
}

impl VerifiedBridgeArtifact {
    pub fn image_reference(&self) -> &str {
        &self.image_reference
    }
}

pub trait BridgeArtifactVerifier: Clone + Send + Sync + 'static {
    /// Returns the immutable image reference only after the verifier has accepted the artifact's
    /// provenance.  The bridge additionally rejects mutable image references before use.
    fn verify_bridge_artifact<'a>(&'a self) -> BoxFuture<'a, Result<String, StableBridgeError>>;
}

/// Production callers share the controller's GitHub/Sigstore verifier rather than interpreting
/// the same bundle in two independently maintained implementations.
impl BridgeArtifactVerifier for SharedGitHubArtifactVerifier {
    fn verify_bridge_artifact<'a>(&'a self) -> BoxFuture<'a, Result<String, StableBridgeError>> {
        Box::pin(async move {
            self.verify()
                .map(|artifact| artifact.image_reference().to_owned())
                .map_err(|_| StableBridgeError::ArtifactUnverified)
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StableBridgeTarget {
    pub runtime_uid: String,
    pub sandbox: String,
    pub artifact: VerifiedBridgeArtifact,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StableBridgeResolution {
    runtime_uid: String,
    sandbox: String,
    artifact_image: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StableBridgeError {
    Unavailable,
    NotReady,
    ArtifactUnverified,
}

/// Resolves the stable bridge target from server authority, not a caller-selected runtime ID.
#[derive(Clone)]
pub struct StableRuntimeBridge<S, R, A> {
    source: S,
    runtimes: R,
    artifacts: A,
}

#[derive(Clone)]
struct StableBridgeRouteState<S, R, A> {
    bridge: StableRuntimeBridge<S, R, A>,
    service: BridgeService,
}

/// Mount the stable bridge resolver behind the common browser authentication boundary.
///
/// The route implementation is deliberately kept separate from artifact deployment: this
/// resolver can only return a target after the injected verifier accepts its immutable image.
pub fn protected_router<S, R, A>(
    source: S,
    runtimes: R,
    artifacts: A,
    browser_auth: BrowserAuthService,
    service: BridgeService,
) -> Router
where
    S: ActiveTaskRuntimeSource,
    R: RuntimeRepository,
    A: BridgeArtifactVerifier,
{
    let state = StableBridgeRouteState {
        bridge: StableRuntimeBridge::new(source, runtimes, artifacts),
        service,
    };
    let routes = Router::new()
        .route(
            STABLE_RUNTIME_BRIDGE_PATH,
            get(resolve_stable_runtime_bridge::<S, R, A>),
        )
        .with_state(state);
    protect_browser_routes(routes, browser_auth)
}

async fn resolve_stable_runtime_bridge<S, R, A>(
    session: Option<Extension<BrowserSessionContext>>,
    State(state): State<StableBridgeRouteState<S, R, A>>,
) -> Response
where
    S: ActiveTaskRuntimeSource,
    R: RuntimeRepository,
    A: BridgeArtifactVerifier,
{
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    match state
        .bridge
        .resolve(&session.principal.canonical_user_id, &state.service)
        .await
    {
        Ok(target) => Json(StableBridgeResolution {
            runtime_uid: target.runtime_uid,
            sandbox: target.sandbox,
            artifact_image: target.artifact.image_reference().to_owned(),
        })
        .into_response(),
        Err(StableBridgeError::Unavailable)
        | Err(StableBridgeError::NotReady)
        | Err(StableBridgeError::ArtifactUnverified) => {
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

impl<S, R, A> StableRuntimeBridge<S, R, A>
where
    S: ActiveTaskRuntimeSource,
    R: RuntimeRepository,
    A: BridgeArtifactVerifier,
{
    pub fn new(source: S, runtimes: R, artifacts: A) -> Self {
        Self {
            source,
            runtimes,
            artifacts,
        }
    }

    pub async fn resolve(
        &self,
        owner: &CanonicalUserId,
        service: &BridgeService,
    ) -> Result<StableBridgeTarget, StableBridgeError> {
        let active = self
            .source
            .active_task_runtime(owner, service)
            .await?
            .ok_or(StableBridgeError::NotReady)?;
        let runtime = self
            .runtimes
            .get(active.namespace(), active.name())
            .await
            .map_err(|_| StableBridgeError::Unavailable)?;
        if runtime.metadata.uid.as_deref() != Some(active.uid()) {
            return Err(StableBridgeError::NotReady);
        }
        let status = runtime.status.as_ref().ok_or(StableBridgeError::NotReady)?;
        if status.phase != steward_types::Phase::Running {
            return Err(StableBridgeError::NotReady);
        }
        let sandbox = status
            .refs
            .sandbox
            .as_deref()
            .filter(|sandbox| !sandbox.trim().is_empty())
            .ok_or(StableBridgeError::NotReady)?;
        let image_reference = self
            .artifacts
            .verify_bridge_artifact()
            .await
            .map_err(|_| StableBridgeError::ArtifactUnverified)?;
        let artifact = verified_artifact(image_reference)?;
        Ok(StableBridgeTarget {
            runtime_uid: active.uid().to_owned(),
            sandbox: sandbox.to_owned(),
            artifact,
        })
    }
}

fn verified_artifact(image_reference: String) -> Result<VerifiedBridgeArtifact, StableBridgeError> {
    let Some((repository, digest)) = image_reference.rsplit_once('@') else {
        return Err(StableBridgeError::ArtifactUnverified);
    };
    if repository.is_empty()
        || !digest.starts_with("sha256:")
        || digest.len() != "sha256:".len() + 64
        || !digest["sha256:".len()..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(StableBridgeError::ArtifactUnverified);
    }
    Ok(VerifiedBridgeArtifact { image_reference })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use kube::ResourceExt;
    use steward_types::{
        AgentRuntime, AgentRuntimeSpec, AgentType, Budget, CanonicalAuthorityBinding,
        CanonicalUserId, Duration, Email, Phase, Principal, RunnerRequirements, RuntimeRefs,
    };
    use tower::ServiceExt;

    use super::{
        ActiveTaskRuntimeSource, BridgeArtifactVerifier, BridgeRuntimeReference, BridgeService,
        STABLE_RUNTIME_BRIDGE_PATH, StableBridgeError, StableRuntimeBridge, protected_router,
    };
    use crate::browser_auth::{
        BrowserAuthService, LocalFakeIdentity, browser_auth_router, local_fake_browser_auth_service,
    };
    use crate::{AdmissionContext, BoxFuture, RuntimeCreateError, RuntimeRepository};

    #[derive(Clone)]
    struct Source(Option<BridgeRuntimeReference>);

    impl ActiveTaskRuntimeSource for Source {
        fn active_task_runtime<'a>(
            &'a self,
            _owner: &'a CanonicalUserId,
            _service: &'a BridgeService,
        ) -> BoxFuture<'a, Result<Option<BridgeRuntimeReference>, StableBridgeError>> {
            Box::pin(async move { Ok(self.0.clone()) })
        }
    }

    #[derive(Clone)]
    struct Artifacts;

    impl BridgeArtifactVerifier for Artifacts {
        fn verify_bridge_artifact<'a>(
            &'a self,
        ) -> BoxFuture<'a, Result<String, StableBridgeError>> {
            Box::pin(async {
                Ok("registry.example.test/steward-bridge@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned())
            })
        }
    }

    #[derive(Clone)]
    struct MutableArtifacts;

    impl BridgeArtifactVerifier for MutableArtifacts {
        fn verify_bridge_artifact<'a>(
            &'a self,
        ) -> BoxFuture<'a, Result<String, StableBridgeError>> {
            Box::pin(async { Ok("registry.example.test/steward-bridge:latest".to_owned()) })
        }
    }

    #[derive(Clone)]
    struct Runtimes(Arc<AgentRuntime>);

    impl RuntimeRepository for Runtimes {
        fn create<'a>(
            &'a self,
            _namespace: &'a str,
            _runtime: &'a AgentRuntime,
            _context: &'a AdmissionContext,
        ) -> BoxFuture<'a, Result<AgentRuntime, RuntimeCreateError>> {
            Box::pin(async { Err(RuntimeCreateError::Unavailable("unused".to_owned())) })
        }

        fn create_as_authority<'a>(
            &'a self,
            _namespace: &'a str,
            _runtime: &'a AgentRuntime,
        ) -> BoxFuture<'a, Result<AgentRuntime, RuntimeCreateError>> {
            Box::pin(async { Err(RuntimeCreateError::Unavailable("unused".to_owned())) })
        }

        fn get<'a>(
            &'a self,
            namespace: &'a str,
            name: &'a str,
        ) -> BoxFuture<'a, Result<AgentRuntime, String>> {
            Box::pin(async move {
                if self.0.namespace().as_deref() == Some(namespace) && self.0.name_any() == name {
                    Ok((*self.0).clone())
                } else {
                    Err("runtime not found".to_owned())
                }
            })
        }

        fn get_bound<'a>(
            &'a self,
            namespace: &'a str,
            name: &'a str,
            uid: &'a str,
        ) -> BoxFuture<'a, Result<AgentRuntime, String>> {
            Box::pin(async move {
                if self.0.namespace().as_deref() == Some(namespace)
                    && self.0.name_any() == name
                    && self.0.metadata.uid.as_deref() == Some(uid)
                {
                    Ok((*self.0).clone())
                } else {
                    Err("runtime UID no longer matches".to_owned())
                }
            })
        }

        fn get_by_uid<'a>(&'a self, _uid: &'a str) -> BoxFuture<'a, Result<AgentRuntime, String>> {
            Box::pin(async { Err("unused".to_owned()) })
        }

        fn replace<'a>(
            &'a self,
            _runtime: &'a AgentRuntime,
            _context: &'a AdmissionContext,
        ) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(async { Err("unused".to_owned()) })
        }

        fn replace_as_authority<'a>(
            &'a self,
            _runtime: &'a AgentRuntime,
        ) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(async { Err("unused".to_owned()) })
        }
    }

    fn runtime() -> Result<AgentRuntime, String> {
        let owner = CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?;
        let mut runtime = AgentRuntime::new(
            "runtime-a",
            AgentRuntimeSpec {
                principal: Principal::User {
                    acting_user: Email::parse("alice@example.com")?,
                },
                owner: Email::parse("alice@example.com")?,
                canonical_authority: Some(CanonicalAuthorityBinding::new(
                    owner.clone(),
                    Some(owner),
                )?),
                agent_type: AgentType {
                    name: "agent-a".to_owned(),
                },
                llms: Vec::new(),
                tools: Vec::new(),
                budget: Budget {
                    monthly_limit: "1.00".to_owned(),
                    single_run_limit: None,
                    currency: "USD".to_owned(),
                },
                ttl: Duration("1h".to_owned()),
                runner: RunnerRequirements::default(),
                bindings: None,
            },
        );
        runtime.metadata.namespace = Some("steward-test".to_owned());
        runtime.metadata.uid = Some("runtime-uid-a".to_owned());
        runtime.status = Some(steward_types::AgentRuntimeStatus {
            phase: Phase::Running,
            observed_generation: 1,
            spec_digest: "digest-a".to_owned(),
            refs: RuntimeRefs {
                sandbox: Some("sandbox-a".to_owned()),
                ..RuntimeRefs::default()
            },
            conditions: Vec::new(),
            spend: None,
        });
        Ok(runtime)
    }

    fn cookie(response: &axum::response::Response, name: &str) -> Result<String, String> {
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

    async fn signed_in_cookie() -> Result<(BrowserAuthService, String), String> {
        let service =
            local_fake_browser_auth_service("http://127.0.0.1:33003", LocalFakeIdentity::User)?;
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
    async fn stable_route_fails_closed_until_the_controller_reports_replacement_ready()
    -> Result<(), String> {
        let (browser_auth, session_cookie) = signed_in_cookie().await?;
        let mut replacement = runtime()?;
        replacement.metadata.uid = Some("runtime-uid-b".to_owned());
        replacement
            .status
            .as_mut()
            .ok_or("fixture runtime lacks status")?
            .phase = Phase::Provisioning;
        let app = protected_router(
            Source(Some(BridgeRuntimeReference::new(
                "steward-test".to_owned(),
                "runtime-a".to_owned(),
                "runtime-uid-b".to_owned(),
            )?)),
            Runtimes(Arc::new(replacement)),
            Artifacts,
            browser_auth,
            BridgeService::new("steward-run".to_owned())?,
        );

        let response = app
            .oneshot(
                Request::builder()
                    .uri(STABLE_RUNTIME_BRIDGE_PATH)
                    .header(header::COOKIE, session_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build stable route request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute stable route request: {error}"))?;

        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "the stable route must not serve a replacement before controller readiness"
        );
        Ok(())
    }

    #[tokio::test]
    async fn stable_route_uses_the_authenticated_owner_and_ignores_caller_runtime_identifiers()
    -> Result<(), String> {
        let (browser_auth, session_cookie) = signed_in_cookie().await?;
        let app = protected_router(
            Source(Some(BridgeRuntimeReference::new(
                "steward-test".to_owned(),
                "runtime-a".to_owned(),
                "runtime-uid-a".to_owned(),
            )?)),
            Runtimes(Arc::new(runtime()?)),
            Artifacts,
            browser_auth,
            BridgeService::new("steward-run".to_owned())?,
        );

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "{STABLE_RUNTIME_BRIDGE_PATH}?runtimeUid=caller-selected&namespace=caller-selected"
                    ))
                    .header(header::COOKIE, session_cookie)
                    .body(Body::empty())
                    .map_err(|error| format!("build stable route request: {error}"))?,
            )
            .await
            .map_err(|error| format!("execute stable route request: {error}"))?;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 8 * 1024)
            .await
            .map_err(|error| format!("read stable route response: {error}"))?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("parse stable route response: {error}"))?;
        assert_eq!(value["runtimeUid"], "runtime-uid-a");
        assert_eq!(value["sandbox"], "sandbox-a");
        assert_eq!(
            value["artifactImage"],
            "registry.example.test/steward-bridge@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert!(!value.to_string().contains("caller-selected"));
        Ok(())
    }

    #[tokio::test]
    async fn resolves_only_the_server_selected_ready_runtime() -> Result<(), String> {
        let owner = CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?;
        let bridge = StableRuntimeBridge::new(
            Source(Some(BridgeRuntimeReference::new(
                "steward-test".to_owned(),
                "runtime-a".to_owned(),
                "runtime-uid-a".to_owned(),
            )?)),
            Runtimes(Arc::new(runtime()?)),
            Artifacts,
        );

        let target = bridge
            .resolve(&owner, &BridgeService::new("steward-run".to_owned())?)
            .await
            .map_err(|error| format!("resolve stable bridge target: {error:?}"))?;

        assert_eq!(target.runtime_uid, "runtime-uid-a");
        assert_eq!(target.sandbox, "sandbox-a");
        assert_eq!(
            target.artifact.image_reference(),
            "registry.example.test/steward-bridge@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        Ok(())
    }

    #[tokio::test]
    async fn replacement_is_not_served_until_controller_reports_it_running() -> Result<(), String> {
        let owner = CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?;
        let mut replacement = runtime()?;
        replacement.metadata.uid = Some("runtime-uid-b".to_owned());
        replacement
            .status
            .as_mut()
            .ok_or("fixture runtime lacks status")?
            .phase = Phase::Provisioning;
        let bridge = StableRuntimeBridge::new(
            Source(Some(BridgeRuntimeReference::new(
                "steward-test".to_owned(),
                "runtime-a".to_owned(),
                "runtime-uid-b".to_owned(),
            )?)),
            Runtimes(Arc::new(replacement)),
            Artifacts,
        );

        assert_eq!(
            bridge
                .resolve(&owner, &BridgeService::new("steward-run".to_owned())?)
                .await,
            Err(StableBridgeError::NotReady),
            "a replacement must remain unavailable until controller readiness"
        );
        Ok(())
    }

    #[tokio::test]
    async fn replacement_with_a_stale_uid_is_not_served() -> Result<(), String> {
        let owner = CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?;
        let bridge = StableRuntimeBridge::new(
            Source(Some(BridgeRuntimeReference::new(
                "steward-test".to_owned(),
                "runtime-a".to_owned(),
                "runtime-uid-b".to_owned(),
            )?)),
            Runtimes(Arc::new(runtime()?)),
            Artifacts,
        );

        assert_eq!(
            bridge
                .resolve(&owner, &BridgeService::new("steward-run".to_owned())?)
                .await,
            Err(StableBridgeError::NotReady),
            "a stale runtime UID must not retain the stable bridge route"
        );
        Ok(())
    }

    #[tokio::test]
    async fn mutable_or_unverified_bridge_artifacts_are_rejected() -> Result<(), String> {
        let owner = CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?;
        let bridge = StableRuntimeBridge::new(
            Source(Some(BridgeRuntimeReference::new(
                "steward-test".to_owned(),
                "runtime-a".to_owned(),
                "runtime-uid-a".to_owned(),
            )?)),
            Runtimes(Arc::new(runtime()?)),
            MutableArtifacts,
        );

        assert_eq!(
            bridge
                .resolve(&owner, &BridgeService::new("steward-run".to_owned())?)
                .await,
            Err(StableBridgeError::ArtifactUnverified),
            "a mutable bridge image must not be distributed"
        );
        Ok(())
    }

    #[test]
    fn task_store_reference_preserves_the_server_selected_runtime_identity() -> Result<(), String> {
        let active = steward_store::ActiveTaskRuntime {
            task_uid: uuid::Uuid::nil(),
            runtime_uid: "runtime-uid-a".to_owned(),
            runtime_namespace: "steward-test".to_owned(),
            runtime_name: "runtime-a".to_owned(),
        };

        let reference = super::bridge_runtime_reference(active)
            .map_err(|error| format!("convert active task runtime: {error:?}"))?;

        assert_eq!(reference.namespace(), "steward-test");
        assert_eq!(reference.name(), "runtime-a");
        assert_eq!(reference.uid(), "runtime-uid-a");
        Ok(())
    }
}
