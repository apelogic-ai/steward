//! Kubernetes reconciliation for `AgentRuntime` resources.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use axum::extract::State;
use axum::http::{HeaderName, HeaderValue};
use axum::routing::post;
use axum::{Json, Router};
use futures::StreamExt;
use kube::api::{Api, Patch, PatchParams, PostParams};
use kube::core::DynamicObject;
use kube::core::Request as KubeRequest;
use kube::core::admission::{AdmissionRequest, AdmissionResponse, Operation};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::finalizer::{Event, finalizer};
use kube::runtime::watcher;
use kube::{Client, ResourceExt};
use sha2::{Digest, Sha256};
use steward_admission::{AdmissionDecision, AdmissionDelta, Envelope, evaluate_with_grants};
use steward_ports::{PortError, SandboxObservation, SandboxRequest, SandboxRuntime};
use steward_store::{GrantReversion, PgStore, StoreError};
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
    Authority(String),
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
    authority: Option<PgStore>,
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
        tools: runtime.spec.tools.clone(),
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
pub const MEMBER_ROLE_ANNOTATION: &str = "agents.apelogic.ai/member-role";

pub async fn run_controller<R: SandboxRuntime>(client: Client, sandbox_runtime: R) {
    run_controller_inner(client, sandbox_runtime, None).await;
}

pub async fn run_controller_with_database<R: SandboxRuntime>(
    client: Client,
    sandbox_runtime: R,
    database_url: &str,
) -> Result<(), StoreError> {
    let authority = PgStore::connect(database_url).await?;
    authority.migrate().await?;
    run_controller_with_store(client, sandbox_runtime, authority).await;
    Ok(())
}

pub async fn run_controller_with_store<R: SandboxRuntime>(
    client: Client,
    sandbox_runtime: R,
    authority: PgStore,
) {
    run_controller_inner(client, sandbox_runtime, Some(authority)).await;
}

async fn run_controller_inner<R: SandboxRuntime>(
    client: Client,
    sandbox_runtime: R,
    authority: Option<PgStore>,
) {
    let runtimes = Api::<AgentRuntime>::all(client.clone());
    let context = Arc::new(ControllerContext {
        client,
        sandbox_runtime,
        authority,
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
                if let Some(authority) = &context.authority {
                    if let Some(reversion) = authority
                        .grant_reversion(runtime.metadata.uid.as_deref().ok_or(
                            ControllerError::Reconcile(ReconcileError::MissingRuntimeUid),
                        )?)
                        .await
                        .map_err(|error| {
                            ControllerError::Reconcile(ReconcileError::Authority(error.to_string()))
                        })?
                    {
                        let latest_envelope = authority
                            .latest_envelope(&reversion.member_role)
                            .await
                            .map_err(|error| {
                                ControllerError::Reconcile(ReconcileError::Authority(
                                    error.to_string(),
                                ))
                            })?
                            .ok_or_else(|| {
                                ControllerError::Reconcile(ReconcileError::Authority(
                                    "grant member role no longer has an envelope".to_owned(),
                                ))
                            })?;
                        let surviving_grants = authority
                            .grants_for_runtime(
                                runtime.metadata.uid.as_deref().ok_or(
                                    ControllerError::Reconcile(ReconcileError::MissingRuntimeUid),
                                )?,
                                &reversion.member_role,
                                latest_envelope.revision,
                            )
                            .await
                            .map_err(|error| {
                                ControllerError::Reconcile(ReconcileError::Authority(
                                    error.to_string(),
                                ))
                            })?;
                        match authority_action(
                            &runtime,
                            &reversion,
                            &latest_envelope,
                            &surviving_grants,
                        )
                        .map_err(ControllerError::Reconcile)?
                        {
                            AuthorityAction::Continue => {}
                            AuthorityAction::Restore(mut restored) => {
                                restored.metadata = runtime.metadata.clone();
                                replace_as_authority(
                                    &context.client,
                                    &restored,
                                    &reversion.actor,
                                    &reversion.member_role,
                                )
                                .await?;
                                return Ok(Action::requeue(StdDuration::from_secs(2)));
                            }
                            AuthorityAction::Suspend => {
                                return suspend_runtime(&runtime, &api, &context.sandbox_runtime)
                                    .await;
                            }
                        }
                    }
                    if let Some(application) = authority
                        .grant_application(runtime.metadata.uid.as_deref().ok_or(
                            ControllerError::Reconcile(ReconcileError::MissingRuntimeUid),
                        )?)
                        .await
                        .map_err(|error| {
                            ControllerError::Reconcile(ReconcileError::Authority(error.to_string()))
                        })?
                    {
                        match authority_application_action(&runtime, &application)
                            .map_err(ControllerError::Reconcile)?
                        {
                            AuthorityAction::Restore(mut proposed) => {
                                proposed.metadata = runtime.metadata.clone();
                                replace_as_authority(
                                    &context.client,
                                    &proposed,
                                    &application.actor,
                                    &application.member_role,
                                )
                                .await?;
                                return Ok(Action::requeue(StdDuration::from_secs(2)));
                            }
                            AuthorityAction::Continue | AuthorityAction::Suspend => {}
                        }
                    }
                    let Some(member_role) = runtime
                        .annotations()
                        .get(MEMBER_ROLE_ANNOTATION)
                        .filter(|role| !role.is_empty())
                    else {
                        return suspend_runtime(&runtime, &api, &context.sandbox_runtime).await;
                    };
                    let latest_envelope = authority
                        .latest_envelope(member_role)
                        .await
                        .map_err(|error| {
                            ControllerError::Reconcile(ReconcileError::Authority(error.to_string()))
                        })?
                        .ok_or_else(|| {
                            ControllerError::Reconcile(ReconcileError::Authority(
                                "runtime member role no longer has an envelope".to_owned(),
                            ))
                        })?;
                    let grants =
                        authority
                            .grants_for_runtime(
                                runtime.metadata.uid.as_deref().ok_or(
                                    ControllerError::Reconcile(ReconcileError::MissingRuntimeUid),
                                )?,
                                member_role,
                                latest_envelope.revision,
                            )
                            .await
                            .map_err(|error| {
                                ControllerError::Reconcile(ReconcileError::Authority(
                                    error.to_string(),
                                ))
                            })?;
                    if matches!(
                        runtime_authority_action(&runtime, &latest_envelope, &grants)
                            .map_err(ControllerError::Reconcile)?,
                        AuthorityAction::Suspend
                    ) {
                        return suspend_runtime(&runtime, &api, &context.sandbox_runtime).await;
                    }
                }
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

async fn suspend_runtime<R: SandboxRuntime>(
    runtime: &AgentRuntime,
    api: &Api<AgentRuntime>,
    sandbox_runtime: &R,
) -> Result<Action, ControllerError> {
    let decision = reconcile_once(runtime, ReconcileIntent::Delete, sandbox_runtime)
        .await
        .map_err(ControllerError::Reconcile)?;
    let status = match decision {
        ReconcileDecision::Deleted => suspended_status(runtime)?,
        ReconcileDecision::Status(status) => status,
    };
    if runtime.status.as_ref() != Some(&status) {
        api.patch_status(
            &runtime.name_any(),
            &PatchParams::default(),
            &Patch::Merge(&status_merge_patch(&status)),
        )
        .await
        .map_err(ControllerError::Kubernetes)?;
    }
    Ok(Action::requeue(StdDuration::from_secs(60)))
}

#[derive(Clone, Debug)]
enum AuthorityAction {
    Continue,
    Restore(Box<AgentRuntime>),
    Suspend,
}

fn authority_action(
    runtime: &AgentRuntime,
    reversion: &GrantReversion,
    latest_envelope: &Envelope,
    surviving_grants: &[AdmissionDelta],
) -> Result<AuthorityAction, ReconcileError> {
    validate_authority_binding(runtime, reversion)?;
    let base_is_admitted =
        evaluate_with_grants(&reversion.base_spec, latest_envelope, surviving_grants).map_err(
            |error| ReconcileError::InvalidSpec {
                reason: format!("{error:?}"),
            },
        )? == AdmissionDecision::Admit;
    if runtime.spec == reversion.base_spec && base_is_admitted {
        return Ok(AuthorityAction::Continue);
    }
    if runtime.spec == reversion.proposed_spec && base_is_admitted {
        let mut restored = runtime.clone();
        restored.spec = reversion.base_spec.clone();
        Ok(AuthorityAction::Restore(Box::new(restored)))
    } else {
        Ok(AuthorityAction::Suspend)
    }
}

fn authority_application_action(
    runtime: &AgentRuntime,
    application: &GrantReversion,
) -> Result<AuthorityAction, ReconcileError> {
    validate_authority_binding(runtime, application)?;
    if runtime.spec == application.base_spec {
        let mut proposed = runtime.clone();
        proposed.spec = application.proposed_spec.clone();
        Ok(AuthorityAction::Restore(Box::new(proposed)))
    } else {
        Ok(AuthorityAction::Continue)
    }
}

fn runtime_authority_action(
    runtime: &AgentRuntime,
    latest_envelope: &Envelope,
    grants: &[AdmissionDelta],
) -> Result<AuthorityAction, ReconcileError> {
    match evaluate_with_grants(&runtime.spec, latest_envelope, grants).map_err(|error| {
        ReconcileError::InvalidSpec {
            reason: format!("{error:?}"),
        }
    })? {
        AdmissionDecision::Admit => Ok(AuthorityAction::Continue),
        AdmissionDecision::Reject { .. } => Ok(AuthorityAction::Suspend),
    }
}

fn validate_authority_binding(
    runtime: &AgentRuntime,
    authority: &GrantReversion,
) -> Result<(), ReconcileError> {
    let namespace = runtime
        .metadata
        .namespace
        .as_deref()
        .ok_or(ReconcileError::MissingNamespace)?;
    let runtime_uid = runtime
        .metadata
        .uid
        .as_deref()
        .ok_or(ReconcileError::MissingRuntimeUid)?;
    if namespace != authority.runtime_namespace
        || runtime.name_any() != authority.runtime_name
        || runtime_uid != authority.runtime_uid
    {
        return Err(ReconcileError::Authority(
            "grant authority is bound to a different runtime instance".to_owned(),
        ));
    }
    Ok(())
}

fn suspended_status(runtime: &AgentRuntime) -> Result<AgentRuntimeStatus, ControllerError> {
    let serialized_spec = serde_json::to_vec(&runtime.spec).map_err(|error| {
        ControllerError::Reconcile(ReconcileError::InvalidSpec {
            reason: error.to_string(),
        })
    })?;
    let digest = Sha256::digest(serialized_spec);
    let spec_digest = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(AgentRuntimeStatus {
        phase: Phase::Suspended,
        observed_generation: runtime.metadata.generation.unwrap_or_default(),
        spec_digest,
        refs: RuntimeRefs::default(),
        conditions: Vec::new(),
        spend: None,
    })
}

async fn replace_as_authority(
    client: &Client,
    runtime: &AgentRuntime,
    actor: &str,
    member_role: &str,
) -> Result<(), ControllerError> {
    let namespace = runtime
        .namespace()
        .ok_or(ControllerError::Reconcile(ReconcileError::MissingNamespace))?;
    let body = serde_json::to_vec(runtime).map_err(|error| {
        ControllerError::Reconcile(ReconcileError::InvalidSpec {
            reason: error.to_string(),
        })
    })?;
    let mut request = KubeRequest::new(format!(
        "/apis/agents.apelogic.ai/v1alpha1/namespaces/{namespace}/agentruntimes"
    ))
    .replace(&runtime.name_any(), &PostParams::default(), body)
    .map_err(|error| ControllerError::Reconcile(ReconcileError::Authority(error.to_string())))?;
    request.headers_mut().insert(
        HeaderName::from_static("impersonate-user"),
        HeaderValue::from_str(actor).map_err(|error| {
            ControllerError::Reconcile(ReconcileError::Authority(error.to_string()))
        })?,
    );
    request.headers_mut().insert(
        HeaderName::from_static("impersonate-group"),
        HeaderValue::from_str(&format!("{MEMBER_ROLE_GROUP_PREFIX}{member_role}")).map_err(
            |error| ControllerError::Reconcile(ReconcileError::Authority(error.to_string())),
        )?,
    );
    client
        .request::<AgentRuntime>(request)
        .await
        .map(|_| ())
        .map_err(ControllerError::Kubernetes)
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

    fn grants_for_runtime<'a>(
        &'a self,
        runtime_uid: &'a str,
        member_role: &'a str,
        envelope_revision: i64,
    ) -> WebhookFuture<'a, Result<Vec<AdmissionDelta>, StoreError>>;
}

impl WebhookEnvelopeReader for PgStore {
    fn latest_envelope<'a>(
        &'a self,
        member_role: &'a str,
    ) -> WebhookFuture<'a, Result<Option<Envelope>, StoreError>> {
        Box::pin(async move { PgStore::latest_envelope(self, member_role).await })
    }

    fn grants_for_runtime<'a>(
        &'a self,
        runtime_uid: &'a str,
        member_role: &'a str,
        envelope_revision: i64,
    ) -> WebhookFuture<'a, Result<Vec<AdmissionDelta>, StoreError>> {
        Box::pin(async move {
            PgStore::grants_for_runtime(self, runtime_uid, member_role, envelope_revision).await
        })
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
    if request.operation == Operation::Update {
        let Some(old_runtime) = request.old_object.as_ref() else {
            return response.deny("AgentRuntime UPDATE admission request has no old object");
        };
        if old_runtime.spec.principal != runtime.spec.principal {
            return response
                .deny("AgentRuntime principal is immutable through the validating admission path");
        }
        match &old_runtime.spec.principal {
            steward_types::Principal::User { acting_user } if acting_user.0 == username => {}
            _ => {
                return response.deny(
                    "existing AgentRuntime acting user must match the authenticated Kubernetes username",
                );
            }
        }
    }
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
    let bound_role = runtime
        .annotations()
        .get(MEMBER_ROLE_ANNOTATION)
        .map(String::as_str);
    if bound_role != Some(member_role) {
        return response.deny(
            "AgentRuntime member-role annotation must match the authenticated member-role group",
        );
    }
    if request.operation == Operation::Update
        && request
            .old_object
            .as_ref()
            .and_then(|old| old.annotations().get(MEMBER_ROLE_ANNOTATION))
            .map(String::as_str)
            != Some(member_role)
    {
        return response.deny("AgentRuntime member-role binding is immutable");
    }
    let envelope = match envelopes.latest_envelope(member_role).await {
        Ok(Some(envelope)) => envelope,
        Ok(None) => return response.deny("no envelope exists for the authenticated member role"),
        Err(error) => {
            return response.deny(format!(
                "member-role envelope lookup failed closed: {error}"
            ));
        }
    };
    let grants = match runtime.metadata.uid.as_deref() {
        Some(runtime_uid) => match envelopes
            .grants_for_runtime(runtime_uid, member_role, envelope.revision)
            .await
        {
            Ok(grants) => grants,
            Err(error) => {
                return response.deny(format!("runtime grant lookup failed closed: {error}"));
            }
        },
        None => Vec::new(),
    };
    match evaluate_with_grants(&runtime.spec, &envelope, &grants) {
        Ok(AdmissionDecision::Admit) => response,
        Ok(decision @ AdmissionDecision::Reject { .. }) => response.deny(
            decision
                .counterexample()
                .unwrap_or_else(|| "envelope exceeded".to_owned()),
        ),
        Err(error) => response.deny(format!("AgentRuntime admission failed closed: {error:?}")),
    }
}

async fn validate_admission_for_controller<R: WebhookEnvelopeReader>(
    request: &AdmissionRequest<AgentRuntime>,
    envelopes: &R,
    controller_username: &str,
) -> AdmissionResponse {
    if is_controller_finalizer_update(request, controller_username) {
        return AdmissionResponse::from(request);
    }
    validate_admission(request, envelopes).await
}

fn is_controller_finalizer_update(
    request: &AdmissionRequest<AgentRuntime>,
    controller_username: &str,
) -> bool {
    if request.operation != Operation::Update
        || request.user_info.username.as_deref() != Some(controller_username)
    {
        return false;
    }
    let (Some(old_runtime), Some(runtime)) = (request.old_object.as_ref(), request.object.as_ref())
    else {
        return false;
    };
    if old_runtime.spec != runtime.spec || old_runtime.status != runtime.status {
        return false;
    }
    let mut old_metadata = old_runtime.metadata.clone();
    let mut metadata = runtime.metadata.clone();
    let mut old_finalizers = std::mem::take(&mut old_metadata.finalizers).unwrap_or_default();
    let mut finalizers = std::mem::take(&mut metadata.finalizers).unwrap_or_default();
    if old_metadata != metadata {
        return false;
    }
    old_finalizers.retain(|finalizer| finalizer != FINALIZER);
    finalizers.retain(|finalizer| finalizer != FINALIZER);
    old_finalizers == finalizers
}

#[derive(Clone)]
struct WebhookState<R> {
    envelopes: R,
    controller_username: Option<String>,
}

pub fn webhook_router<R: WebhookEnvelopeReader>(envelopes: R) -> Router {
    webhook_router_with_controller(envelopes, None)
}

pub fn webhook_router_for_controller<R: WebhookEnvelopeReader>(
    envelopes: R,
    controller_username: String,
) -> Router {
    webhook_router_with_controller(envelopes, Some(controller_username))
}

fn webhook_router_with_controller<R: WebhookEnvelopeReader>(
    envelopes: R,
    controller_username: Option<String>,
) -> Router {
    Router::new()
        .route("/validate-agent-runtime", post(webhook_handler::<R>))
        .with_state(WebhookState {
            envelopes,
            controller_username,
        })
}

async fn webhook_handler<R: WebhookEnvelopeReader>(
    State(state): State<WebhookState<R>>,
    Json(review): Json<kube::core::admission::AdmissionReview<AgentRuntime>>,
) -> Json<kube::core::admission::AdmissionReview<DynamicObject>> {
    let response = match review.try_into() {
        Ok(request) => match state.controller_username.as_deref() {
            Some(controller_username) => {
                validate_admission_for_controller(&request, &state.envelopes, controller_username)
                    .await
            }
            None => validate_admission(&request, &state.envelopes).await,
        },
        Err(error) => AdmissionResponse::invalid(error),
    };
    Json(response.into_review())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use steward_admission::{AdmissionDelta, Envelope, EnvelopeSpec};
    use steward_ports::{PortError, SandboxObservation, SandboxRequest, SandboxRuntime};
    use steward_store::GrantReversion;
    use steward_types::{
        AgentRuntime, AgentRuntimeSpec, AgentType, Budget, Duration, Email, ModelRef, Phase,
        Principal, RuntimeRefs,
    };

    use super::{
        AuthorityAction, ReconcileDecision, ReconcileIntent, authority_action,
        authority_application_action, reconcile_once, runtime_authority_action, status_merge_patch,
    };

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

    fn envelope(monthly_limit: &str) -> Envelope {
        Envelope {
            revision: 4,
            spec: EnvelopeSpec {
                llms: vec![ModelRef {
                    provider: "example".to_owned(),
                    model: "model-a".to_owned(),
                }],
                tools: Vec::new(),
                budget: Budget {
                    monthly_limit: monthly_limit.to_owned(),
                    currency: "USD".to_owned(),
                },
                ttl: Duration("1h".to_owned()),
            },
        }
    }

    fn grant_reversion(runtime: &AgentRuntime) -> GrantReversion {
        let mut proposed_spec = runtime.spec.clone();
        proposed_spec.budget.monthly_limit = "2.00".to_owned();
        GrantReversion {
            runtime_uid: "runtime-uid-a".to_owned(),
            runtime_namespace: "team-a".to_owned(),
            runtime_name: "runtime-a".to_owned(),
            actor: "alice@example.com".to_owned(),
            member_role: "engineer".to_owned(),
            base_spec: runtime.spec.clone(),
            proposed_spec,
        }
    }

    #[test]
    fn expired_grant_restores_the_exact_parked_base_spec() -> Result<(), String> {
        let mut runtime = fixture();
        let reversion = grant_reversion(&runtime);
        runtime.spec = reversion.proposed_spec.clone();
        let action = authority_action(&runtime, &reversion, &envelope("1.00"), &[])
            .map_err(|error| format!("authority evaluation failed: {error:?}"))?;
        let AuthorityAction::Restore(restored) = action else {
            return Err("an unchanged escalated spec must be restored after expiry".to_owned());
        };
        assert_eq!(restored.spec, reversion.base_spec);
        Ok(())
    }

    #[test]
    fn approved_grant_converges_after_a_transient_apply_failure() -> Result<(), String> {
        let runtime = fixture();
        let application = grant_reversion(&runtime);
        let action = authority_application_action(&runtime, &application)
            .map_err(|error| format!("authority application failed: {error:?}"))?;
        let AuthorityAction::Restore(proposed) = action else {
            return Err("an approved unapplied grant must remain durable work".to_owned());
        };
        assert_eq!(proposed.spec, application.proposed_spec);
        Ok(())
    }

    #[test]
    fn expired_grant_suspends_when_restoration_would_overwrite_or_exceed() -> Result<(), String> {
        let mut runtime = fixture();
        let reversion = grant_reversion(&runtime);
        runtime.spec = reversion.proposed_spec.clone();
        runtime.spec.ttl = Duration("30m".to_owned());
        assert!(
            matches!(
                authority_action(&runtime, &reversion, &envelope("1.00"), &[])
                    .map_err(|error| format!("authority evaluation failed: {error:?}"))?,
                AuthorityAction::Suspend
            ),
            "intervening desired-state changes must not be overwritten by an old snapshot",
        );

        let mut runtime = fixture();
        let mut reversion = grant_reversion(&runtime);
        reversion.base_spec.budget.monthly_limit = "1.00".to_owned();
        runtime.spec = reversion.proposed_spec.clone();
        assert!(
            matches!(
                authority_action(&runtime, &reversion, &envelope("0.50"), &[])
                    .map_err(|error| format!("authority evaluation failed: {error:?}"))?,
                AuthorityAction::Suspend
            ),
            "a narrowed envelope must suspend rather than restore a now-invalid base spec",
        );
        Ok(())
    }

    #[test]
    fn expired_newest_grant_restores_to_a_base_authorized_by_an_older_grant() -> Result<(), String>
    {
        let mut runtime = fixture();
        let mut reversion = grant_reversion(&runtime);
        reversion.base_spec.budget.monthly_limit = "2.00".to_owned();
        reversion.proposed_spec.budget.monthly_limit = "3.00".to_owned();
        runtime.spec = reversion.proposed_spec.clone();
        let action = authority_action(
            &runtime,
            &reversion,
            &envelope("1.00"),
            &[AdmissionDelta::Budget {
                requested: "2.00".to_owned(),
                ceiling: "1.00".to_owned(),
                currency: "USD".to_owned(),
            }],
        )
        .map_err(|error| format!("authority evaluation failed: {error:?}"))?;
        let AuthorityAction::Restore(restored) = action else {
            return Err("an older surviving grant must authorize the predecessor state".to_owned());
        };
        assert_eq!(restored.spec, reversion.base_spec);
        Ok(())
    }

    #[test]
    fn ordinary_runtime_outside_a_narrowed_envelope_suspends_without_approval_history()
    -> Result<(), String> {
        let runtime = fixture();
        assert!(
            matches!(
                runtime_authority_action(&runtime, &envelope("0.50"), &[])
                    .map_err(|error| format!("authority evaluation failed: {error:?}"))?,
                AuthorityAction::Suspend
            ),
            "periodic reconciliation must suspend an ordinary runtime after envelope narrowing"
        );
        Ok(())
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
    use std::collections::BTreeMap;

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use kube::core::admission::{AdmissionRequest, AdmissionReview};
    use steward_admission::{AdmissionDelta, Envelope, EnvelopeSpec};
    use steward_store::StoreError;
    use steward_types::{AgentRuntime, Budget, Duration, ModelRef};
    use tower::ServiceExt;

    use super::{
        FINALIZER, WebhookEnvelopeReader, WebhookFuture, validate_admission, webhook_router,
    };

    #[derive(Clone)]
    struct FakeEnvelopes {
        envelope: Envelope,
        grants: BTreeMap<String, Vec<AdmissionDelta>>,
    }

    impl WebhookEnvelopeReader for FakeEnvelopes {
        fn latest_envelope<'a>(
            &'a self,
            _member_role: &'a str,
        ) -> WebhookFuture<'a, Result<Option<Envelope>, StoreError>> {
            Box::pin(async move { Ok(Some(self.envelope.clone())) })
        }

        fn grants_for_runtime<'a>(
            &'a self,
            runtime_uid: &'a str,
            _member_role: &'a str,
            _envelope_revision: i64,
        ) -> WebhookFuture<'a, Result<Vec<AdmissionDelta>, StoreError>> {
            Box::pin(async move { Ok(self.grants.get(runtime_uid).cloned().unwrap_or_default()) })
        }
    }

    fn admission_review_value() -> serde_json::Value {
        let mut value = serde_json::json!({
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
                        "uid": "runtime-uid-a",
                        "annotations": {
                            "agents.apelogic.ai/member-role": "engineer"
                        }
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
        });
        value["request"]["oldObject"] = value["request"]["object"].clone();
        value
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
            grants: BTreeMap::new(),
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

    #[tokio::test]
    async fn webhook_applies_a_grant_only_to_its_bound_runtime_uid() -> Result<(), String> {
        let mut envelopes = fake_envelopes();
        envelopes.grants.insert(
            "runtime-uid-a".to_owned(),
            vec![AdmissionDelta::Budget {
                requested: "220.00".to_owned(),
                ceiling: "200.00".to_owned(),
                currency: "USD".to_owned(),
            }],
        );
        let review =
            serde_json::from_value::<AdmissionReview<AgentRuntime>>(admission_review_value())
                .map_err(|error| format!("failed to construct granted review: {error}"))?;
        let request: AdmissionRequest<AgentRuntime> = review
            .try_into()
            .map_err(|error| format!("failed to read granted request: {error}"))?;
        let response = validate_admission(&request, &envelopes).await;
        assert!(
            response.allowed,
            "the exact approved manifest must pass for runtime UID A: {}",
            response.result.message
        );

        let mut other_value = admission_review_value();
        other_value["request"]["object"]["metadata"]["uid"] = serde_json::json!("runtime-uid-b");
        let other_review = serde_json::from_value::<AdmissionReview<AgentRuntime>>(other_value)
            .map_err(|error| format!("failed to construct second-runtime review: {error}"))?;
        let other_request: AdmissionRequest<AgentRuntime> = other_review
            .try_into()
            .map_err(|error| format!("failed to read second-runtime request: {error}"))?;
        let other_response = validate_admission(&other_request, &envelopes).await;
        assert!(
            !other_response.allowed,
            "runtime UID B must not inherit runtime UID A's approved exception"
        );
        assert_eq!(
            other_response.result.message,
            "envelope exceeded: budget.monthlyLimit requested 220.00 USD, ceiling 200.00 USD"
        );
        Ok(())
    }

    #[tokio::test]
    async fn webhook_rejects_principal_takeover_on_update() -> Result<(), String> {
        let mut value = admission_review_value();
        value["request"]["object"]["spec"]["budget"]["monthlyLimit"] = serde_json::json!("100.00");
        let mut old_object = value["request"]["object"].clone();
        old_object["spec"]["principal"]["actingUser"] = serde_json::json!("bob@example.org");
        value["request"]["oldObject"] = old_object;
        let review = serde_json::from_value::<AdmissionReview<AgentRuntime>>(value)
            .map_err(|error| format!("failed to construct takeover review: {error}"))?;
        let request: AdmissionRequest<AgentRuntime> = review
            .try_into()
            .map_err(|error| format!("failed to read takeover request: {error}"))?;

        let response = validate_admission(&request, &fake_envelopes()).await;

        assert!(
            !response.allowed,
            "an update must not reassign its principal"
        );
        assert_eq!(
            response.result.message,
            "AgentRuntime principal is immutable through the validating admission path"
        );
        Ok(())
    }

    #[tokio::test]
    async fn webhook_allows_only_its_controller_to_change_its_finalizer() -> Result<(), String> {
        let mut value = admission_review_value();
        value["request"]["userInfo"] = serde_json::json!({
            "username": "system:serviceaccount:steward-system:steward-controller",
            "groups": ["system:serviceaccounts"]
        });
        value["request"]["object"]["metadata"]["finalizers"] = serde_json::json!([FINALIZER]);
        let review = serde_json::from_value::<AdmissionReview<AgentRuntime>>(value.clone())
            .map_err(|error| format!("failed to construct finalizer review: {error}"))?;
        let request: AdmissionRequest<AgentRuntime> = review
            .try_into()
            .map_err(|error| format!("failed to read finalizer request: {error}"))?;

        let response = super::validate_admission_for_controller(
            &request,
            &fake_envelopes(),
            "system:serviceaccount:steward-system:steward-controller",
        )
        .await;
        assert!(
            response.allowed,
            "the configured controller must be able to add Steward's finalizer: {}",
            response.result.message
        );

        value["request"]["object"]["spec"]["budget"]["monthlyLimit"] = serde_json::json!("100.00");
        let review = serde_json::from_value::<AdmissionReview<AgentRuntime>>(value.clone())
            .map_err(|error| format!("failed to construct controller spec edit: {error}"))?;
        let request: AdmissionRequest<AgentRuntime> = review
            .try_into()
            .map_err(|error| format!("failed to read controller spec edit: {error}"))?;
        let response = super::validate_admission_for_controller(
            &request,
            &fake_envelopes(),
            "system:serviceaccount:steward-system:steward-controller",
        )
        .await;
        assert!(
            !response.allowed,
            "controller identity must not exempt desired-state changes"
        );

        value["request"]["object"]["spec"]["budget"]["monthlyLimit"] =
            value["request"]["oldObject"]["spec"]["budget"]["monthlyLimit"].clone();
        value["request"]["object"]["metadata"]["finalizers"] =
            serde_json::json!([FINALIZER, "example.com/other"]);
        let review = serde_json::from_value::<AdmissionReview<AgentRuntime>>(value)
            .map_err(|error| format!("failed to construct foreign finalizer edit: {error}"))?;
        let request: AdmissionRequest<AgentRuntime> = review
            .try_into()
            .map_err(|error| format!("failed to read foreign finalizer edit: {error}"))?;
        let response = super::validate_admission_for_controller(
            &request,
            &fake_envelopes(),
            "system:serviceaccount:steward-system:steward-controller",
        )
        .await;
        assert!(
            !response.allowed,
            "controller identity must not exempt another controller's finalizer"
        );
        Ok(())
    }
}
