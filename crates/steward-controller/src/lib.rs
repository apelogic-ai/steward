//! Kubernetes reconciliation for `AgentRuntime` resources.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use futures::StreamExt;
use kube::api::{Api, Patch, PatchParams};
use kube::core::DynamicObject;
use kube::core::admission::{AdmissionRequest, AdmissionResponse, Operation};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::finalizer::{Event, finalizer};
use kube::runtime::watcher;
use kube::{Client, ResourceExt};
use sha2::{Digest, Sha256};
use steward_admission::{AdmissionDecision, Envelope, evaluate};
use steward_ports::{PortError, SandboxObservation, SandboxRequest, SandboxRuntime};
use steward_store::{PgStore, StoreError};
use steward_types::{AgentRuntime, AgentRuntimeStatus, Phase, RuntimeId, RuntimeRefs};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReconcileIntent {
    Ensure,
    Delete,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ReconcileDecision {
    Status(AgentRuntimeStatus),
    Deleted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReconcileError {
    MissingNamespace,
    MissingRuntimeUid,
    InvalidSpec { reason: String },
    Runtime(PortError),
    DeletionPending,
}

impl fmt::Display for ReconcileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl Error for ReconcileError {}

#[derive(Debug)]
pub enum ControllerError {
    Reconcile(ReconcileError),
    Kubernetes(kube::Error),
    Finalizer(String),
}

impl fmt::Display for ControllerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reconcile(error) => write!(formatter, "runtime reconciliation failed: {error}"),
            Self::Kubernetes(error) => write!(formatter, "Kubernetes API request failed: {error}"),
            Self::Finalizer(error) => write!(formatter, "finalizer reconciliation failed: {error}"),
        }
    }
}

impl Error for ControllerError {}

struct ControllerContext<R> {
    client: Client,
    sandbox_runtime: R,
}

pub async fn reconcile_once<R: SandboxRuntime>(
    runtime: &AgentRuntime,
    intent: ReconcileIntent,
    sandbox_runtime: &R,
) -> Result<ReconcileDecision, ReconcileError> {
    let workspace_key = runtime
        .metadata
        .namespace
        .clone()
        .ok_or(ReconcileError::MissingNamespace)?;
    let runtime_id = runtime
        .metadata
        .uid
        .clone()
        .map(RuntimeId)
        .ok_or(ReconcileError::MissingRuntimeUid)?;
    let request = SandboxRequest {
        runtime: runtime_id,
        workspace_key,
        agent_type: runtime.spec.agent_type.clone(),
    };

    let observation = match intent {
        ReconcileIntent::Ensure => sandbox_runtime.ensure(&request).await,
        ReconcileIntent::Delete => sandbox_runtime.delete(&request).await,
    }
    .map_err(ReconcileError::Runtime)?;

    let (phase, refs) = match (intent, observation) {
        (ReconcileIntent::Delete, SandboxObservation::Provisioning { refs })
        | (ReconcileIntent::Delete, SandboxObservation::Running { refs }) => {
            (Phase::Terminating, refs)
        }
        (ReconcileIntent::Ensure, SandboxObservation::Absent) => {
            (Phase::Provisioning, RuntimeRefs::default())
        }
        (ReconcileIntent::Ensure, SandboxObservation::Provisioning { refs }) => {
            (Phase::Provisioning, refs)
        }
        (ReconcileIntent::Ensure, SandboxObservation::Running { refs }) => (Phase::Running, refs),
        (ReconcileIntent::Delete, SandboxObservation::Absent) => {
            return Ok(ReconcileDecision::Deleted);
        }
    };
    let serialized_spec =
        serde_json::to_vec(&runtime.spec).map_err(|error| ReconcileError::InvalidSpec {
            reason: error.to_string(),
        })?;
    let digest = Sha256::digest(serialized_spec);
    let mut spec_digest = String::with_capacity(digest.len() * 2);
    for byte in digest {
        spec_digest.push_str(&format!("{byte:02x}"));
    }
    Ok(ReconcileDecision::Status(AgentRuntimeStatus {
        phase,
        observed_generation: runtime.metadata.generation.unwrap_or_default(),
        spec_digest,
        refs,
        conditions: Vec::new(),
        spend: None,
    }))
}

const FINALIZER: &str = "agents.apelogic.ai/runtime";

pub async fn run_controller<R: SandboxRuntime>(client: Client, sandbox_runtime: R) {
    let runtimes = Api::<AgentRuntime>::all(client.clone());
    let context = Arc::new(ControllerContext {
        client,
        sandbox_runtime,
    });
    Controller::new(runtimes, watcher::Config::default())
        .shutdown_on_signal()
        .run(reconcile, error_policy, context)
        .for_each(|result| async move {
            match result {
                Ok(reference) => eprintln!("reconciled {reference:?}"),
                Err(error) => eprintln!("reconcile error: {error}"),
            }
        })
        .await;
}

async fn reconcile<R: SandboxRuntime>(
    runtime: Arc<AgentRuntime>,
    context: Arc<ControllerContext<R>>,
) -> Result<Action, ControllerError> {
    let namespace = runtime
        .namespace()
        .ok_or(ControllerError::Reconcile(ReconcileError::MissingNamespace))?;
    let api = Api::<AgentRuntime>::namespaced(context.client.clone(), &namespace);
    finalizer(&api, FINALIZER, runtime, |event| async {
        match event {
            Event::Apply(runtime) => {
                let decision =
                    reconcile_once(&runtime, ReconcileIntent::Ensure, &context.sandbox_runtime)
                        .await
                        .map_err(ControllerError::Reconcile)?;
                let ReconcileDecision::Status(status) = decision else {
                    return Err(ControllerError::Reconcile(ReconcileError::DeletionPending));
                };
                let running = status.phase == Phase::Running;
                if runtime.status.as_ref() != Some(&status) {
                    let name = runtime.name_any();
                    let patch = status_merge_patch(&status);
                    api.patch_status(&name, &PatchParams::default(), &Patch::Merge(&patch))
                        .await
                        .map_err(ControllerError::Kubernetes)?;
                }
                Ok(if running {
                    Action::requeue(StdDuration::from_secs(60))
                } else {
                    Action::requeue(StdDuration::from_secs(2))
                })
            }
            Event::Cleanup(runtime) => {
                let decision =
                    reconcile_once(&runtime, ReconcileIntent::Delete, &context.sandbox_runtime)
                        .await
                        .map_err(ControllerError::Reconcile)?;
                match decision {
                    ReconcileDecision::Deleted => Ok(Action::await_change()),
                    ReconcileDecision::Status(status) => {
                        let name = runtime.name_any();
                        let patch = status_merge_patch(&status);
                        api.patch_status(&name, &PatchParams::default(), &Patch::Merge(&patch))
                            .await
                            .map_err(ControllerError::Kubernetes)?;
                        Err(ControllerError::Reconcile(ReconcileError::DeletionPending))
                    }
                }
            }
        }
    })
    .await
    .map_err(|error| ControllerError::Finalizer(error.to_string()))
}

fn status_merge_patch(status: &AgentRuntimeStatus) -> serde_json::Value {
    serde_json::json!({
        "status": {
            "phase": status.phase,
            "observedGeneration": status.observed_generation,
            "specDigest": status.spec_digest,
            "refs": {
                "workspace": status.refs.workspace,
                "sandbox": status.refs.sandbox,
                "litellmKey": status.refs.litellm_key,
            },
            "conditions": status.conditions,
            "spend": status.spend,
        },
    })
}

fn error_policy<R: SandboxRuntime>(
    _runtime: Arc<AgentRuntime>,
    _error: &ControllerError,
    _context: Arc<ControllerContext<R>>,
) -> Action {
    Action::requeue(StdDuration::from_secs(5))
}

const MEMBER_ROLE_GROUP_PREFIX: &str = "agents.apelogic.ai/member-role:";

pub type WebhookFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait WebhookEnvelopeReader: Clone + Send + Sync + 'static {
    fn latest_envelope<'a>(
        &'a self,
        member_role: &'a str,
    ) -> WebhookFuture<'a, Result<Option<Envelope>, StoreError>>;
}

impl WebhookEnvelopeReader for PgStore {
    fn latest_envelope<'a>(
        &'a self,
        member_role: &'a str,
    ) -> WebhookFuture<'a, Result<Option<Envelope>, StoreError>> {
        Box::pin(async move { PgStore::latest_envelope(self, member_role).await })
    }
}

pub async fn validate_admission<R: WebhookEnvelopeReader>(
    request: &AdmissionRequest<AgentRuntime>,
    envelopes: &R,
) -> AdmissionResponse {
    let response = AdmissionResponse::from(request);
    if request.operation == Operation::Delete {
        return response;
    }
    if !matches!(request.operation, Operation::Create | Operation::Update) {
        return response.deny("AgentRuntime admission supports CREATE and UPDATE only");
    }
    let Some(runtime) = request.object.as_ref() else {
        return response.deny("AgentRuntime admission request has no object");
    };
    let Some(username) = request.user_info.username.as_deref() else {
        return response.deny("authenticated Kubernetes username is required");
    };
    match &runtime.spec.principal {
        steward_types::Principal::User { acting_user } if acting_user.0 == username => {}
        _ => {
            return response
                .deny("AgentRuntime acting user must match the authenticated Kubernetes username");
        }
    }
    let roles = request
        .user_info
        .groups
        .iter()
        .flatten()
        .filter_map(|group| group.strip_prefix(MEMBER_ROLE_GROUP_PREFIX))
        .filter(|role| !role.is_empty())
        .collect::<BTreeSet<_>>();
    let Some(member_role) = roles.iter().next().copied().filter(|_| roles.len() == 1) else {
        return response.deny("exactly one authenticated member-role group is required");
    };
    let envelope = match envelopes.latest_envelope(member_role).await {
        Ok(Some(envelope)) => envelope,
        Ok(None) => return response.deny("no envelope exists for the authenticated member role"),
        Err(error) => {
            return response.deny(format!(
                "member-role envelope lookup failed closed: {error}"
            ));
        }
    };
    match evaluate(&runtime.spec, &envelope) {
        Ok(AdmissionDecision::Admit) => response,
        Ok(decision @ AdmissionDecision::Reject { .. }) => response.deny(
            decision
                .counterexample()
                .unwrap_or_else(|| "envelope exceeded".to_owned()),
        ),
        Err(error) => response.deny(format!("AgentRuntime admission failed closed: {error:?}")),
    }
}

#[derive(Clone)]
struct WebhookState<R> {
    envelopes: R,
}

pub fn webhook_router<R: WebhookEnvelopeReader>(envelopes: R) -> Router {
    Router::new()
        .route("/validate-agent-runtime", post(webhook_handler::<R>))
        .with_state(WebhookState { envelopes })
}

async fn webhook_handler<R: WebhookEnvelopeReader>(
    State(state): State<WebhookState<R>>,
    Json(review): Json<kube::core::admission::AdmissionReview<AgentRuntime>>,
) -> Json<kube::core::admission::AdmissionReview<DynamicObject>> {
    let response = match review.try_into() {
        Ok(request) => validate_admission(&request, &state.envelopes).await,
        Err(error) => AdmissionResponse::invalid(error),
    };
    Json(response.into_review())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use steward_ports::{PortError, SandboxObservation, SandboxRequest, SandboxRuntime};
    use steward_types::{
        AgentRuntime, AgentRuntimeSpec, AgentType, Budget, Duration, Email, ModelRef, Phase,
        Principal, RuntimeRefs,
    };

    use super::{ReconcileDecision, ReconcileIntent, reconcile_once, status_merge_patch};

    #[derive(Default)]
    struct FakeSandboxRuntime {
        state: Mutex<FakeState>,
    }

    #[derive(Default)]
    struct FakeState {
        created: usize,
        deleted: usize,
        refs: Option<RuntimeRefs>,
    }

    impl SandboxRuntime for FakeSandboxRuntime {
        async fn ensure(&self, request: &SandboxRequest) -> Result<SandboxObservation, PortError> {
            let mut state = self.state.lock().map_err(|_| PortError::Failed {
                reason: "fake runtime state lock was poisoned".to_owned(),
            })?;
            if state.refs.is_none() {
                state.created += 1;
                state.refs = Some(RuntimeRefs {
                    workspace: Some(format!("workspace-{}", request.workspace_key)),
                    sandbox: Some(format!("sandbox-{}", request.runtime.0)),
                    litellm_key: None,
                });
            }
            let refs = state.refs.clone().ok_or_else(|| PortError::Failed {
                reason: "fake runtime did not retain created refs".to_owned(),
            })?;
            Ok(SandboxObservation::Running { refs })
        }

        async fn delete(&self, _request: &SandboxRequest) -> Result<SandboxObservation, PortError> {
            let mut state = self.state.lock().map_err(|_| PortError::Failed {
                reason: "fake runtime state lock was poisoned".to_owned(),
            })?;
            if state.refs.take().is_some() {
                state.deleted += 1;
            }
            Ok(SandboxObservation::Absent)
        }
    }

    struct PendingDeleteRuntime;

    impl SandboxRuntime for PendingDeleteRuntime {
        async fn ensure(&self, _request: &SandboxRequest) -> Result<SandboxObservation, PortError> {
            Ok(SandboxObservation::Absent)
        }

        async fn delete(&self, _request: &SandboxRequest) -> Result<SandboxObservation, PortError> {
            Ok(SandboxObservation::Provisioning {
                refs: RuntimeRefs {
                    workspace: Some("workspace-a".to_owned()),
                    sandbox: Some("sandbox-a".to_owned()),
                    litellm_key: None,
                },
            })
        }
    }

    fn fixture() -> AgentRuntime {
        let mut runtime = AgentRuntime::new(
            "runtime-a",
            AgentRuntimeSpec {
                principal: Principal::User {
                    acting_user: Email("alice@example.com".to_owned()),
                },
                owner: Email("alice@example.com".to_owned()),
                agent_type: AgentType {
                    name: "base".to_owned(),
                },
                llms: vec![ModelRef {
                    provider: "example".to_owned(),
                    model: "model-a".to_owned(),
                }],
                tools: Vec::new(),
                budget: Budget {
                    monthly_limit: "1.00".to_owned(),
                    currency: "USD".to_owned(),
                },
                ttl: Duration("1h".to_owned()),
                bindings: None,
            },
        );
        runtime.metadata.namespace = Some("team-a".to_owned());
        runtime.metadata.uid = Some("runtime-uid-a".to_owned());
        runtime.metadata.generation = Some(3);
        runtime
    }

    #[tokio::test]
    async fn reconcile_is_idempotent_across_restart_and_delete() -> Result<(), String> {
        let runtime = fixture();
        let sandbox_runtime = FakeSandboxRuntime::default();

        let first = reconcile_once(&runtime, ReconcileIntent::Ensure, &sandbox_runtime)
            .await
            .map_err(|error| format!("first ensure reconcile failed: {error:?}"))?;
        let second = reconcile_once(&runtime, ReconcileIntent::Ensure, &sandbox_runtime)
            .await
            .map_err(|error| format!("restart ensure reconcile failed: {error:?}"))?;

        assert_eq!(
            first, second,
            "a restarted controller must converge to the same runtime status"
        );
        let ReconcileDecision::Status(status) = first else {
            return Err("ensure reconcile did not return status".to_owned());
        };
        assert_eq!(status.phase, Phase::Running);
        assert_eq!(status.observed_generation, 3);
        assert!(status.refs.workspace.is_some());
        assert!(status.refs.sandbox.is_some());
        let patch = status_merge_patch(&status);
        for pointer in ["/status/refs/litellmKey", "/status/spend"] {
            assert!(
                patch
                    .pointer(pointer)
                    .is_some_and(serde_json::Value::is_null),
                "absent cache field {pointer} must be an explicit merge-patch tombstone"
            );
        }

        let first_delete = reconcile_once(&runtime, ReconcileIntent::Delete, &sandbox_runtime)
            .await
            .map_err(|error| format!("first delete reconcile failed: {error:?}"))?;
        let second_delete = reconcile_once(&runtime, ReconcileIntent::Delete, &sandbox_runtime)
            .await
            .map_err(|error| format!("restart delete reconcile failed: {error:?}"))?;
        assert_eq!(first_delete, ReconcileDecision::Deleted);
        assert_eq!(second_delete, ReconcileDecision::Deleted);

        {
            let state = sandbox_runtime
                .state
                .lock()
                .map_err(|_| "fake runtime state lock was poisoned".to_owned())?;
            assert_eq!(state.created, 1, "ensure must create exactly one sandbox");
            assert_eq!(state.deleted, 1, "delete must remove exactly one sandbox");
        }

        let pending = reconcile_once(&runtime, ReconcileIntent::Delete, &PendingDeleteRuntime)
            .await
            .map_err(|error| format!("pending delete reconcile failed: {error:?}"))?;
        let ReconcileDecision::Status(pending_status) = pending else {
            return Err("pending delete did not return status".to_owned());
        };
        assert_eq!(
            pending_status.phase,
            Phase::Terminating,
            "an accepted external delete must become observable before finalizer removal"
        );
        Ok(())
    }
}

#[cfg(test)]
mod webhook_tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use kube::core::admission::{AdmissionRequest, AdmissionReview};
    use steward_admission::{Envelope, EnvelopeSpec};
    use steward_store::StoreError;
    use steward_types::{AgentRuntime, Budget, Duration, ModelRef};
    use tower::ServiceExt;

    use super::{WebhookEnvelopeReader, WebhookFuture, validate_admission, webhook_router};

    #[derive(Clone)]
    struct FakeEnvelopes {
        envelope: Envelope,
    }

    impl WebhookEnvelopeReader for FakeEnvelopes {
        fn latest_envelope<'a>(
            &'a self,
            _member_role: &'a str,
        ) -> WebhookFuture<'a, Result<Option<Envelope>, StoreError>> {
            Box::pin(async move { Ok(Some(self.envelope.clone())) })
        }
    }

    fn admission_review_value() -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "admission.k8s.io/v1",
            "kind": "AdmissionReview",
            "request": {
                "uid": "request-a",
                "kind": {
                    "group": "agents.apelogic.ai",
                    "version": "v1alpha1",
                    "kind": "AgentRuntime"
                },
                "resource": {
                    "group": "agents.apelogic.ai",
                    "version": "v1alpha1",
                    "resource": "agentruntimes"
                },
                "name": "runtime-a",
                "namespace": "team-a",
                "operation": "UPDATE",
                "userInfo": {
                    "username": "alice@example.com",
                    "groups": ["agents.apelogic.ai/member-role:engineer"]
                },
                "object": {
                    "apiVersion": "agents.apelogic.ai/v1alpha1",
                    "kind": "AgentRuntime",
                    "metadata": {
                        "name": "runtime-a",
                        "namespace": "team-a",
                        "uid": "runtime-uid-a"
                    },
                    "spec": {
                        "principal": {
                            "kind": "user",
                            "actingUser": "alice@example.com"
                        },
                        "owner": "alice@example.com",
                        "agentType": {"name": "base"},
                        "llms": [{"provider": "provider-a", "model": "model-a"}],
                        "tools": [],
                        "budget": {"monthlyLimit": "220.00", "currency": "USD"},
                        "ttl": "24h"
                    }
                },
                "oldObject": null,
                "dryRun": false,
                "options": null
            }
        })
    }

    fn fake_envelopes() -> FakeEnvelopes {
        FakeEnvelopes {
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
        }
    }

    #[tokio::test]
    async fn webhook_hard_denies_with_the_shared_counterexample() -> Result<(), String> {
        let review =
            serde_json::from_value::<AdmissionReview<AgentRuntime>>(admission_review_value())
                .map_err(|error| format!("failed to construct AdmissionReview fixture: {error}"))?;
        let request: AdmissionRequest<AgentRuntime> = review
            .try_into()
            .map_err(|error| format!("failed to read AdmissionRequest fixture: {error}"))?;
        let envelopes = fake_envelopes();

        let response = validate_admission(&request, &envelopes).await;

        assert!(
            !response.allowed,
            "over-envelope kubectl update must be denied"
        );
        assert_eq!(response.uid, "request-a");
        assert_eq!(
            response.result.message,
            "envelope exceeded: budget.monthlyLimit requested 220.00 USD, ceiling 200.00 USD"
        );
        Ok(())
    }

    #[tokio::test]
    async fn webhook_http_route_returns_an_admission_review() -> Result<(), String> {
        let app = webhook_router(fake_envelopes());
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/validate-agent-runtime")
                    .header("content-type", "application/json")
                    .body(Body::from(admission_review_value().to_string()))
                    .map_err(|error| format!("failed to build webhook request: {error}"))?,
            )
            .await
            .map_err(|error| format!("webhook route failed: {error}"))?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .map_err(|error| format!("failed to read webhook response: {error}"))?;
        let review = serde_json::from_slice::<serde_json::Value>(&body)
            .map_err(|error| format!("webhook response was not JSON: {error}"))?;
        assert_eq!(
            review.pointer("/response/allowed"),
            Some(&serde_json::json!(false))
        );
        assert_eq!(
            review.pointer("/response/status/message"),
            Some(&serde_json::json!(
                "envelope exceeded: budget.monthlyLimit requested 220.00 USD, ceiling 200.00 USD"
            ))
        );
        Ok(())
    }
}
