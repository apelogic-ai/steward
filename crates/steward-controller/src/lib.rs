//! Kubernetes reconciliation for `AgentRuntime` resources.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration as StdDuration;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::{HeaderName, HeaderValue};
use axum::routing::post;
use axum::{Json, Router};
use futures::StreamExt;
use kube::api::{Api, DeleteParams, Patch, PatchParams, PostParams};
use kube::core::Request as KubeRequest;
use kube::core::admission::{AdmissionRequest, AdmissionResponse, Operation};
use kube::core::{ApiResource, DynamicObject, GroupVersionKind};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::finalizer::{Event, finalizer};
use kube::runtime::watcher;
use kube::{Client, Resource, ResourceExt};
use sha2::{Digest, Sha256};
use steward_admission::{
    AdmissionDecision, AdmissionDelta, Envelope, budget_is_exhausted, duration_seconds,
    evaluate_with_grants,
};
use steward_ports::{
    InferenceCapabilities, InferenceCredential, InferenceObservation, InferencePlane,
    InferenceRequest, PortError, ProvisionedInference, SandboxObservation, SandboxRequest,
    SandboxRuntime,
};
use steward_store::{GrantReversion, PgStore, StoreError};
use steward_types::{
    AgentRuntime, AgentRuntimeStatus, Duration, PENDING_APPROVAL_ANNOTATION, Phase, RuntimeId,
    RuntimeRefs,
};

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
enum InferenceAction {
    Reprovision,
    Continue {
        reference: String,
        spend: steward_types::SpendSummary,
    },
    Suspend {
        reference: String,
        spend: steward_types::SpendSummary,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TtlAction {
    Continue { requeue_after: StdDuration },
    Terminate,
}

fn ttl_action(
    created_at_epoch_seconds: i64,
    ttl: &Duration,
    now_epoch_seconds: i64,
) -> Result<TtlAction, ReconcileError> {
    let ttl_seconds = duration_seconds(ttl).map_err(|error| ReconcileError::InvalidSpec {
        reason: format!("runtime TTL is invalid: {error:?}"),
    })?;
    let ttl_seconds = i64::try_from(ttl_seconds).map_err(|_| ReconcileError::InvalidSpec {
        reason: "runtime TTL exceeds the supported deadline range".to_owned(),
    })?;
    let deadline = created_at_epoch_seconds
        .checked_add(ttl_seconds)
        .ok_or_else(|| ReconcileError::InvalidSpec {
            reason: "runtime TTL deadline overflowed".to_owned(),
        })?;
    if now_epoch_seconds >= deadline {
        return Ok(TtlAction::Terminate);
    }
    let remaining =
        u64::try_from(deadline - now_epoch_seconds).map_err(|_| ReconcileError::InvalidSpec {
            reason: "runtime TTL deadline moved before the current time".to_owned(),
        })?;
    Ok(TtlAction::Continue {
        requeue_after: StdDuration::from_secs(remaining.min(60)),
    })
}

fn runtime_ttl_action(runtime: &AgentRuntime) -> Result<TtlAction, ReconcileError> {
    let created_at = runtime
        .metadata
        .creation_timestamp
        .as_ref()
        .ok_or_else(|| ReconcileError::InvalidSpec {
            reason: "persisted runtime has no creation timestamp".to_owned(),
        })?
        .0
        .as_second();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ReconcileError::InvalidSpec {
            reason: "system clock is before the Unix epoch".to_owned(),
        })?
        .as_secs();
    let now = i64::try_from(now).map_err(|_| ReconcileError::InvalidSpec {
        reason: "system clock exceeds the supported deadline range".to_owned(),
    })?;
    ttl_action(created_at, &runtime.spec.ttl, now)
}

fn inference_action(observation: steward_ports::InferenceObservation) -> InferenceAction {
    match observation {
        steward_ports::InferenceObservation::Absent => InferenceAction::Reprovision,
        steward_ports::InferenceObservation::Active { reference, spend } => {
            InferenceAction::Continue { reference, spend }
        }
        steward_ports::InferenceObservation::Exhausted { reference, spend } => {
            InferenceAction::Suspend { reference, spend }
        }
    }
}

fn spend_still_exhausts_runtime(
    runtime: &AgentRuntime,
    spend: steward_types::SpendSummary,
) -> Result<Option<steward_types::SpendSummary>, ReconcileError> {
    budget_is_exhausted(&spend, &runtime.spec.budget)
        .map(|exhausted| exhausted.then_some(spend))
        .map_err(|error| ReconcileError::InvalidSpec {
            reason: format!("runtime spend could not be compared with its budget: {error:?}"),
        })
}

fn exhausted_spend_to_preserve(
    runtime: &AgentRuntime,
) -> Result<Option<steward_types::SpendSummary>, ReconcileError> {
    let Some(spend) = runtime
        .status
        .as_ref()
        .filter(|status| matches!(status.phase, Phase::Terminating | Phase::Suspended))
        .and_then(|status| status.spend.clone())
    else {
        return Ok(None);
    };
    spend_still_exhausts_runtime(runtime, spend)
}

fn runtime_spec_digest(runtime: &AgentRuntime) -> Result<String, ReconcileError> {
    let serialized_spec =
        serde_json::to_vec(&runtime.spec).map_err(|error| ReconcileError::InvalidSpec {
            reason: error.to_string(),
        })?;
    let digest = Sha256::digest(serialized_spec);
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReconcileError {
    MissingNamespace,
    MissingRuntimeUid,
    InvalidSpec { reason: String },
    Runtime(PortError),
    Authority(String),
    DeletionPending,
    InferenceRevocationTimedOut,
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

struct ControllerContext<R, I> {
    client: Client,
    inference: I,
    sandbox_runtime: R,
    authority: Option<PgStore>,
}

#[derive(Clone, Copy)]
struct NoInferencePlane;

impl InferencePlane for NoInferencePlane {
    fn capabilities(&self) -> InferenceCapabilities {
        InferenceCapabilities::default()
    }

    async fn validate_configuration(
        &self,
        models: &[steward_types::ModelRef],
        _budget: &steward_types::Budget,
    ) -> Result<(), PortError> {
        if models.is_empty() {
            Ok(())
        } else {
            Err(PortError::Unsupported {
                operation: "inference model validation",
            })
        }
    }

    async fn provision(
        &self,
        _request: &InferenceRequest,
    ) -> Result<ProvisionedInference, PortError> {
        Err(PortError::Unsupported {
            operation: "inference credential provisioning",
        })
    }

    async fn reconcile_configuration(&self, _request: &InferenceRequest) -> Result<(), PortError> {
        Ok(())
    }

    async fn observe(
        &self,
        _request: &InferenceRequest,
    ) -> Result<InferenceObservation, PortError> {
        Ok(InferenceObservation::Absent)
    }

    async fn revoke(&self, _request: &InferenceRequest) -> Result<(), PortError> {
        Ok(())
    }
}

pub async fn reconcile_once<R: SandboxRuntime>(
    runtime: &AgentRuntime,
    intent: ReconcileIntent,
    sandbox_runtime: &R,
) -> Result<ReconcileDecision, ReconcileError> {
    if intent == ReconcileIntent::Ensure && is_pending_approval(runtime) {
        return Ok(ReconcileDecision::Status(pending_approval_status(runtime)?));
    }
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
        models: runtime.spec.llms.clone(),
        tools: runtime.spec.tools.clone(),
        refs: runtime
            .status
            .as_ref()
            .map(|status| status.refs.clone())
            .unwrap_or_default(),
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
    Ok(ReconcileDecision::Status(AgentRuntimeStatus {
        phase,
        observed_generation: runtime.metadata.generation.unwrap_or_default(),
        spec_digest: runtime_spec_digest(runtime)?,
        refs,
        conditions: Vec::new(),
        spend: None,
    }))
}

const FINALIZER: &str = "agents.apelogic.ai/runtime";
pub const MEMBER_ROLE_ANNOTATION: &str = "agents.apelogic.ai/member-role";

fn is_pending_approval(runtime: &AgentRuntime) -> bool {
    runtime
        .annotations()
        .get(PENDING_APPROVAL_ANNOTATION)
        .is_some_and(|digest| !digest.is_empty())
}

fn pending_approval_status(runtime: &AgentRuntime) -> Result<AgentRuntimeStatus, ReconcileError> {
    Ok(AgentRuntimeStatus {
        phase: Phase::Pending,
        observed_generation: runtime.metadata.generation.unwrap_or_default(),
        spec_digest: runtime_spec_digest(runtime)?,
        refs: RuntimeRefs::default(),
        conditions: Vec::new(),
        spend: None,
    })
}

pub async fn run_controller<R: SandboxRuntime>(client: Client, sandbox_runtime: R) {
    run_controller_inner(client, sandbox_runtime, NoInferencePlane, None).await;
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
    run_controller_inner(client, sandbox_runtime, NoInferencePlane, Some(authority)).await;
}

pub async fn run_controller_with_planes<R: SandboxRuntime, I: InferencePlane>(
    client: Client,
    sandbox_runtime: R,
    inference: I,
    authority: PgStore,
) {
    run_controller_inner(client, sandbox_runtime, inference, Some(authority)).await;
}

async fn run_controller_inner<R: SandboxRuntime, I: InferencePlane>(
    client: Client,
    sandbox_runtime: R,
    inference: I,
    authority: Option<PgStore>,
) {
    let runtimes = Api::<AgentRuntime>::all(client.clone());
    let context = Arc::new(ControllerContext {
        client,
        inference,
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

enum InferenceReconcile {
    Inactive,
    Active {
        reference: String,
        spend: steward_types::SpendSummary,
    },
    Exhausted {
        spend: steward_types::SpendSummary,
    },
}

fn inference_request(runtime: &AgentRuntime) -> Result<InferenceRequest, ReconcileError> {
    let runtime_id = runtime
        .metadata
        .uid
        .clone()
        .map(steward_types::RuntimeId)
        .ok_or(ReconcileError::MissingRuntimeUid)?;
    Ok(InferenceRequest {
        runtime: runtime_id,
        models: runtime.spec.llms.clone(),
        budget: runtime.spec.budget.clone(),
    })
}

fn secret_resource() -> ApiResource {
    ApiResource::from_gvk(&GroupVersionKind::gvk("", "v1", "Secret"))
}

fn runtime_secret_api(client: Client, namespace: &str) -> Api<DynamicObject> {
    Api::namespaced_with(client, namespace, &secret_resource())
}

fn credential_secret_is_bound(
    runtime: &AgentRuntime,
    secret: &DynamicObject,
) -> Result<(), ReconcileError> {
    let runtime_uid = runtime
        .metadata
        .uid
        .as_deref()
        .ok_or(ReconcileError::MissingRuntimeUid)?;
    let namespace = runtime
        .namespace()
        .ok_or(ReconcileError::MissingNamespace)?;
    let label_matches = secret
        .metadata
        .labels
        .as_ref()
        .and_then(|labels| labels.get("agents.apelogic.ai/runtime-uid"))
        .map(String::as_str)
        == Some(runtime_uid);
    let owner_matches = secret
        .metadata
        .owner_references
        .as_ref()
        .is_some_and(|owners| {
            owners.iter().any(|owner| {
                owner.api_version == "agents.apelogic.ai/v1alpha1"
                    && owner.kind == "AgentRuntime"
                    && owner.uid == runtime_uid
                    && owner.controller == Some(true)
            })
        });
    let credential_present = secret
        .data
        .get("data")
        .and_then(|data| data.get("access-token"))
        .is_some();
    if secret.namespace().as_deref() == Some(namespace.as_str())
        && label_matches
        && owner_matches
        && credential_present
    {
        Ok(())
    } else {
        Err(ReconcileError::Authority(
            "inference credential Secret is not bound to this runtime UID".to_owned(),
        ))
    }
}

async fn create_credential_secret(
    client: Client,
    runtime: &AgentRuntime,
    credential: &InferenceCredential,
) -> Result<(), ControllerError> {
    let namespace = runtime
        .namespace()
        .ok_or(ControllerError::Reconcile(ReconcileError::MissingNamespace))?;
    let runtime_uid = runtime
        .metadata
        .uid
        .as_deref()
        .ok_or(ControllerError::Reconcile(
            ReconcileError::MissingRuntimeUid,
        ))?;
    let owner = runtime.controller_owner_ref(&()).ok_or_else(|| {
        ControllerError::Reconcile(ReconcileError::Authority(
            "runtime identity is incomplete for credential ownership".to_owned(),
        ))
    })?;
    let mut secret = DynamicObject::new(runtime_uid, &secret_resource());
    secret.metadata.namespace = Some(namespace.clone());
    secret.metadata.owner_references = Some(vec![owner]);
    secret.metadata.labels = Some(std::collections::BTreeMap::from([(
        "agents.apelogic.ai/runtime-uid".to_owned(),
        runtime_uid.to_owned(),
    )]));
    secret.data = serde_json::json!({
        "type": "Opaque",
        "stringData": {
            "access-token": credential.expose_secret(),
        },
    });
    runtime_secret_api(client, &namespace)
        .create(&PostParams::default(), &secret)
        .await
        .map(|_| ())
        .map_err(ControllerError::Kubernetes)
}

async fn delete_credential_secret(
    client: Client,
    runtime: &AgentRuntime,
) -> Result<(), ControllerError> {
    let namespace = runtime
        .namespace()
        .ok_or(ControllerError::Reconcile(ReconcileError::MissingNamespace))?;
    let runtime_uid = runtime
        .metadata
        .uid
        .as_deref()
        .ok_or(ControllerError::Reconcile(
            ReconcileError::MissingRuntimeUid,
        ))?;
    match runtime_secret_api(client, &namespace)
        .delete(runtime_uid, &DeleteParams::default())
        .await
    {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(response)) if response.code == 404 => Ok(()),
        Err(error) => Err(ControllerError::Kubernetes(error)),
    }
}

async fn provision_inference<I: InferencePlane>(
    client: Client,
    runtime: &AgentRuntime,
    inference: &I,
    request: &InferenceRequest,
) -> Result<(), ControllerError> {
    let provisioned = inference
        .provision(request)
        .await
        .map_err(|error| ControllerError::Reconcile(ReconcileError::Runtime(error)))?;
    if let Err(error) = create_credential_secret(client, runtime, &provisioned.credential).await {
        let _ = inference.revoke(request).await;
        return Err(error);
    }
    Ok(())
}

async fn reconcile_inference<I: InferencePlane>(
    client: Client,
    runtime: &AgentRuntime,
    inference: &I,
) -> Result<InferenceReconcile, ControllerError> {
    let request = inference_request(runtime).map_err(ControllerError::Reconcile)?;
    let namespace = runtime
        .namespace()
        .ok_or(ControllerError::Reconcile(ReconcileError::MissingNamespace))?;
    let secret = runtime_secret_api(client.clone(), &namespace)
        .get_opt(&request.runtime.0)
        .await
        .map_err(ControllerError::Kubernetes)?;
    if request.models.is_empty() {
        if secret.is_some()
            || runtime
                .status
                .as_ref()
                .and_then(|status| status.refs.litellm_key.as_ref())
                .is_some()
        {
            inference
                .revoke(&request)
                .await
                .map_err(|error| ControllerError::Reconcile(ReconcileError::Runtime(error)))?;
            delete_credential_secret(client, runtime).await?;
        }
        return Ok(InferenceReconcile::Inactive);
    }

    if let Some(secret) = secret.as_ref() {
        credential_secret_is_bound(runtime, secret).map_err(ControllerError::Reconcile)?;
        inference
            .reconcile_configuration(&request)
            .await
            .map_err(|error| ControllerError::Reconcile(ReconcileError::Runtime(error)))?;
    } else {
        provision_inference(client.clone(), runtime, inference, &request).await?;
    }

    let mut observation = inference
        .observe(&request)
        .await
        .map_err(|error| ControllerError::Reconcile(ReconcileError::Runtime(error)))?;
    if observation == InferenceObservation::Absent {
        delete_credential_secret(client.clone(), runtime).await?;
        provision_inference(client.clone(), runtime, inference, &request).await?;
        observation = inference
            .observe(&request)
            .await
            .map_err(|error| ControllerError::Reconcile(ReconcileError::Runtime(error)))?;
    }
    match inference_action(observation) {
        InferenceAction::Continue { reference, spend } => {
            Ok(InferenceReconcile::Active { reference, spend })
        }
        InferenceAction::Suspend { spend, .. } => Ok(InferenceReconcile::Exhausted { spend }),
        InferenceAction::Reprovision => Err(ControllerError::Reconcile(ReconcileError::Runtime(
            PortError::Failed {
                reason: "provisioned inference key was not observable".to_owned(),
            },
        ))),
    }
}

async fn reconcile<R: SandboxRuntime, I: InferencePlane>(
    runtime: Arc<AgentRuntime>,
    context: Arc<ControllerContext<R, I>>,
) -> Result<Action, ControllerError> {
    let namespace = runtime
        .namespace()
        .ok_or(ControllerError::Reconcile(ReconcileError::MissingNamespace))?;
    let api = Api::<AgentRuntime>::namespaced(context.client.clone(), &namespace);
    finalizer(&api, FINALIZER, runtime, |event| async {
        match event {
            Event::Apply(runtime) => {
                let ttl_requeue =
                    match runtime_ttl_action(&runtime).map_err(ControllerError::Reconcile)? {
                        TtlAction::Continue { requeue_after } => requeue_after,
                        TtlAction::Terminate => {
                            match api
                                .delete(&runtime.name_any(), &DeleteParams::default())
                                .await
                            {
                                Ok(_) => {}
                                Err(kube::Error::Api(response)) if response.code == 404 => {}
                                Err(error) => return Err(ControllerError::Kubernetes(error)),
                            }
                            return Ok(Action::await_change());
                        }
                    };
                if is_pending_approval(&runtime) {
                    if let Some(authority) = &context.authority
                        && let Some(application) = authority
                            .grant_application(runtime.metadata.uid.as_deref().ok_or(
                                ControllerError::Reconcile(ReconcileError::MissingRuntimeUid),
                            )?)
                            .await
                            .map_err(|error| {
                                ControllerError::Reconcile(ReconcileError::Authority(
                                    error.to_string(),
                                ))
                            })?
                        && let AuthorityAction::Restore(mut proposed) =
                            authority_application_action(&runtime, &application)
                                .map_err(ControllerError::Reconcile)?
                    {
                        proposed.metadata = runtime.metadata.clone();
                        proposed
                            .metadata
                            .annotations
                            .get_or_insert_default()
                            .remove(PENDING_APPROVAL_ANNOTATION);
                        replace_as_authority(
                            &context.client,
                            &proposed,
                            &application.actor,
                            &application.member_role,
                        )
                        .await?;
                        return Ok(Action::requeue(StdDuration::from_secs(2)));
                    }
                    let status =
                        pending_approval_status(&runtime).map_err(ControllerError::Reconcile)?;
                    if runtime.status.as_ref() != Some(&status) {
                        let patch = status_merge_patch(&status);
                        api.patch_status(
                            &runtime.name_any(),
                            &PatchParams::default(),
                            &Patch::Merge(&patch),
                        )
                        .await
                        .map_err(ControllerError::Kubernetes)?;
                    }
                    return Ok(Action::requeue(ttl_requeue));
                }
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
                                return suspend_runtime_with_inference_cleanup(
                                    &runtime,
                                    &api,
                                    &context.sandbox_runtime,
                                    context.client.clone(),
                                    &context.inference,
                                    None,
                                )
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
                                proposed
                                    .metadata
                                    .annotations
                                    .get_or_insert_default()
                                    .remove(PENDING_APPROVAL_ANNOTATION);
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
                        return suspend_runtime_with_inference_cleanup(
                            &runtime,
                            &api,
                            &context.sandbox_runtime,
                            context.client.clone(),
                            &context.inference,
                            None,
                        )
                        .await;
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
                        return suspend_runtime_with_inference_cleanup(
                            &runtime,
                            &api,
                            &context.sandbox_runtime,
                            context.client.clone(),
                            &context.inference,
                            None,
                        )
                        .await;
                    }
                    let runtime_uid =
                        runtime
                            .metadata
                            .uid
                            .as_deref()
                            .ok_or(ControllerError::Reconcile(
                                ReconcileError::MissingRuntimeUid,
                            ))?;
                    if let Some(spend) =
                        authority
                            .inference_exhaustion(runtime_uid)
                            .await
                            .map_err(|error| {
                                ControllerError::Reconcile(ReconcileError::Authority(
                                    error.to_string(),
                                ))
                            })?
                        && let Some(spend) = spend_still_exhausts_runtime(&runtime, spend)
                            .map_err(ControllerError::Reconcile)?
                    {
                        return suspend_runtime_with_inference_cleanup(
                            &runtime,
                            &api,
                            &context.sandbox_runtime,
                            context.client.clone(),
                            &context.inference,
                            Some(spend),
                        )
                        .await;
                    }
                }
                if let Some(spend) =
                    exhausted_spend_to_preserve(&runtime).map_err(ControllerError::Reconcile)?
                {
                    return suspend_runtime_with_inference_cleanup(
                        &runtime,
                        &api,
                        &context.sandbox_runtime,
                        context.client.clone(),
                        &context.inference,
                        Some(spend),
                    )
                    .await;
                }
                let inference =
                    reconcile_inference(context.client.clone(), &runtime, &context.inference)
                        .await?;
                if let (Some(authority), Some((spend, exhausted))) = (
                    context.authority.as_ref(),
                    match &inference {
                        InferenceReconcile::Active { spend, .. } => Some((spend, false)),
                        InferenceReconcile::Exhausted { spend } => Some((spend, true)),
                        InferenceReconcile::Inactive => None,
                    },
                ) {
                    authority
                        .record_spend_observation(
                            runtime
                                .metadata
                                .uid
                                .as_deref()
                                .ok_or(ControllerError::Reconcile(
                                    ReconcileError::MissingRuntimeUid,
                                ))?,
                            runtime.metadata.generation.unwrap_or_default(),
                            &runtime_spec_digest(&runtime).map_err(ControllerError::Reconcile)?,
                            spend,
                            exhausted,
                        )
                        .await
                        .map_err(|error| {
                            ControllerError::Reconcile(ReconcileError::Authority(error.to_string()))
                        })?;
                }
                let inference_status = match inference {
                    InferenceReconcile::Exhausted { spend } => {
                        return suspend_runtime_with_inference_cleanup(
                            &runtime,
                            &api,
                            &context.sandbox_runtime,
                            context.client.clone(),
                            &context.inference,
                            Some(spend),
                        )
                        .await;
                    }
                    InferenceReconcile::Active { reference, spend } => Some((reference, spend)),
                    InferenceReconcile::Inactive => None,
                };
                let decision =
                    reconcile_once(&runtime, ReconcileIntent::Ensure, &context.sandbox_runtime)
                        .await
                        .map_err(ControllerError::Reconcile)?;
                let ReconcileDecision::Status(mut status) = decision else {
                    return Err(ControllerError::Reconcile(ReconcileError::DeletionPending));
                };
                if let Some((reference, spend)) = inference_status {
                    status.refs.litellm_key = Some(reference);
                    status.spend = Some(spend);
                }
                let running = status.phase == Phase::Running;
                if runtime.status.as_ref() != Some(&status) {
                    let name = runtime.name_any();
                    let patch = status_merge_patch(&status);
                    api.patch_status(&name, &PatchParams::default(), &Patch::Merge(&patch))
                        .await
                        .map_err(ControllerError::Kubernetes)?;
                }
                Ok(if running {
                    Action::requeue(ttl_requeue)
                } else {
                    Action::requeue(ttl_requeue.min(StdDuration::from_secs(2)))
                })
            }
            Event::Cleanup(runtime) => {
                let decision = cleanup_runtime(
                    &runtime,
                    context.client.clone(),
                    &context.inference,
                    &context.sandbox_runtime,
                )
                .await?;
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

async fn cleanup_runtime<R: SandboxRuntime, I: InferencePlane>(
    runtime: &AgentRuntime,
    client: Client,
    inference: &I,
    sandbox_runtime: &R,
) -> Result<ReconcileDecision, ControllerError> {
    let inference_request = inference_request(runtime).map_err(ControllerError::Reconcile)?;
    let sandbox_cleanup = async {
        reconcile_once(runtime, ReconcileIntent::Delete, sandbox_runtime)
            .await
            .map_err(ControllerError::Reconcile)
    };
    let (inference_result, credential_result, sandbox_result) = futures::join!(
        revoke_inference_with_timeout(inference, &inference_request),
        delete_credential_secret(client, runtime),
        sandbox_cleanup,
    );

    let mut decision = sandbox_result?;
    if let ReconcileDecision::Status(status) = &mut decision {
        if inference_result.is_err() {
            status.refs.litellm_key = runtime
                .status
                .as_ref()
                .and_then(|prior| prior.refs.litellm_key.clone());
        }
        return Ok(decision);
    }
    credential_result?;
    inference_result.map_err(ControllerError::Reconcile)?;
    Ok(decision)
}

const INFERENCE_REVOCATION_TIMEOUT: StdDuration = StdDuration::from_secs(5);

async fn revoke_inference_with_timeout<I: InferencePlane>(
    inference: &I,
    request: &InferenceRequest,
) -> Result<(), ReconcileError> {
    tokio::time::timeout(INFERENCE_REVOCATION_TIMEOUT, inference.revoke(request))
        .await
        .map_err(|_| ReconcileError::InferenceRevocationTimedOut)?
        .map_err(ReconcileError::Runtime)
}

async fn suspend_runtime<R: SandboxRuntime>(
    runtime: &AgentRuntime,
    api: &Api<AgentRuntime>,
    sandbox_runtime: &R,
    spend: Option<steward_types::SpendSummary>,
) -> Result<Action, ControllerError> {
    let decision = reconcile_once(runtime, ReconcileIntent::Delete, sandbox_runtime)
        .await
        .map_err(ControllerError::Reconcile)?;
    let mut status = match decision {
        ReconcileDecision::Deleted => suspended_status(runtime)?,
        ReconcileDecision::Status(status) => status,
    };
    status.spend = spend;
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

async fn suspend_runtime_with_inference_cleanup<R: SandboxRuntime, I: InferencePlane>(
    runtime: &AgentRuntime,
    api: &Api<AgentRuntime>,
    sandbox_runtime: &R,
    client: Client,
    inference: &I,
    spend: Option<steward_types::SpendSummary>,
) -> Result<Action, ControllerError> {
    let request = inference_request(runtime).map_err(ControllerError::Reconcile)?;
    let (revoke_result, credential_result, suspension_result) = futures::join!(
        revoke_inference_with_timeout(inference, &request),
        delete_credential_secret(client, runtime),
        suspend_runtime(runtime, api, sandbox_runtime, spend),
    );

    let action = suspension_result?;
    credential_result?;
    revoke_result.map_err(ControllerError::Reconcile)?;
    Ok(action)
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
    Ok(AgentRuntimeStatus {
        phase: Phase::Suspended,
        observed_generation: runtime.metadata.generation.unwrap_or_default(),
        spec_digest: runtime_spec_digest(runtime).map_err(ControllerError::Reconcile)?,
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

fn error_policy<R: SandboxRuntime, I: InferencePlane>(
    _runtime: Arc<AgentRuntime>,
    _error: &ControllerError,
    _context: Arc<ControllerContext<R, I>>,
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

pub trait WebhookModelCatalog: Clone + Send + Sync + 'static {
    fn validate_configuration<'a>(
        &'a self,
        models: &'a [steward_types::ModelRef],
        budget: &'a steward_types::Budget,
    ) -> WebhookFuture<'a, Result<(), PortError>>;
}

impl<T: steward_ports::InferencePlane + Clone> WebhookModelCatalog for T {
    fn validate_configuration<'a>(
        &'a self,
        models: &'a [steward_types::ModelRef],
        budget: &'a steward_types::Budget,
    ) -> WebhookFuture<'a, Result<(), PortError>> {
        Box::pin(async move {
            steward_ports::InferencePlane::validate_configuration(self, models, budget).await
        })
    }
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

pub async fn validate_admission_with_catalog<R: WebhookEnvelopeReader, C: WebhookModelCatalog>(
    request: &AdmissionRequest<AgentRuntime>,
    envelopes: &R,
    catalog: &C,
) -> AdmissionResponse {
    let response = validate_admission(request, envelopes).await;
    if !response.allowed {
        return response;
    }
    let Some(runtime) = request.object.as_ref() else {
        return response.deny("AgentRuntime admission request has no object");
    };
    if runtime.spec.llms.is_empty() {
        return response;
    }
    match catalog
        .validate_configuration(&runtime.spec.llms, &runtime.spec.budget)
        .await
    {
        Ok(()) => response,
        Err(PortError::Rejected { reason }) | Err(PortError::Failed { reason }) => response.deny(
            format!("AgentRuntime inference configuration validation failed closed: {reason}"),
        ),
        Err(PortError::Unsupported { operation }) => response.deny(format!(
            "AgentRuntime inference configuration validation failed closed: configured inference plane does not support {operation}"
        )),
        Err(_) => response.deny(
            "AgentRuntime inference configuration validation failed closed: unrecognized inference-plane failure",
        ),
    }
}

#[cfg(test)]
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
    old_metadata.managed_fields = None;
    metadata.managed_fields = None;
    if old_metadata != metadata {
        return false;
    }
    old_finalizers.retain(|finalizer| finalizer != FINALIZER);
    finalizers.retain(|finalizer| finalizer != FINALIZER);
    old_finalizers == finalizers
}

#[derive(Clone)]
struct AllowConfiguredModels;

impl WebhookModelCatalog for AllowConfiguredModels {
    fn validate_configuration<'a>(
        &'a self,
        _models: &'a [steward_types::ModelRef],
        _budget: &'a steward_types::Budget,
    ) -> WebhookFuture<'a, Result<(), PortError>> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Clone)]
struct WebhookState<R, C> {
    envelopes: R,
    catalog: C,
    controller_username: Option<String>,
}

pub fn webhook_router<R: WebhookEnvelopeReader>(envelopes: R) -> Router {
    webhook_router_with_controller(envelopes, AllowConfiguredModels, None)
}

pub fn webhook_router_for_controller<R: WebhookEnvelopeReader>(
    envelopes: R,
    controller_username: String,
) -> Router {
    webhook_router_with_controller(envelopes, AllowConfiguredModels, Some(controller_username))
}

pub fn webhook_router_for_controller_with_catalog<
    R: WebhookEnvelopeReader,
    C: WebhookModelCatalog,
>(
    envelopes: R,
    catalog: C,
    controller_username: String,
) -> Router {
    webhook_router_with_controller(envelopes, catalog, Some(controller_username))
}

fn webhook_router_with_controller<R: WebhookEnvelopeReader, C: WebhookModelCatalog>(
    envelopes: R,
    catalog: C,
    controller_username: Option<String>,
) -> Router {
    Router::new()
        .route("/validate-agent-runtime", post(webhook_handler::<R, C>))
        .with_state(WebhookState {
            envelopes,
            catalog,
            controller_username,
        })
}

async fn webhook_handler<R: WebhookEnvelopeReader, C: WebhookModelCatalog>(
    State(state): State<WebhookState<R, C>>,
    Json(review): Json<kube::core::admission::AdmissionReview<AgentRuntime>>,
) -> Json<kube::core::admission::AdmissionReview<DynamicObject>> {
    let response = match review.try_into() {
        Ok(request) => {
            if state
                .controller_username
                .as_deref()
                .is_some_and(|username| is_controller_finalizer_update(&request, username))
            {
                AdmissionResponse::from(&request)
            } else {
                validate_admission_with_catalog(&request, &state.envelopes, &state.catalog).await
            }
        }
        Err(error) => AdmissionResponse::invalid(error),
    };
    Json(response.into_review())
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration as StdDuration;

    use axum::body::Body;
    use axum::http::{Method, Request, Response, StatusCode};
    use kube::Client;
    use kube::client::Body as KubeBody;
    use steward_admission::{AdmissionDelta, Envelope, EnvelopeSpec};
    use steward_ports::{
        InferenceCapabilities, InferenceObservation, InferencePlane, InferenceRequest, PortError,
        ProvisionedInference, SandboxObservation, SandboxRequest, SandboxRuntime,
    };
    use steward_store::GrantReversion;
    use steward_types::{
        AgentRuntime, AgentRuntimeSpec, AgentRuntimeStatus, AgentType, Budget, Duration, Email,
        ModelRef, Phase, Principal, RuntimeRefs,
    };
    use tower::service_fn;

    use super::{
        AuthorityAction, InferenceAction, ReconcileDecision, ReconcileIntent, authority_action,
        authority_application_action, cleanup_runtime, exhausted_spend_to_preserve,
        inference_action, reconcile_once, runtime_authority_action, status_merge_patch,
        suspend_runtime_with_inference_cleanup, ttl_action,
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

    #[test]
    fn ttl_expiry_terminates_at_the_creation_time_boundary() -> Result<(), String> {
        assert_eq!(
            ttl_action(1_000, &Duration("60s".to_owned()), 1_059)
                .map_err(|error| format!("valid TTL must be schedulable: {error:?}"))?,
            super::TtlAction::Continue {
                requeue_after: StdDuration::from_secs(1),
            },
            "the controller must requeue at the remaining TTL rather than its ordinary poll"
        );
        assert_eq!(
            ttl_action(1_000, &Duration("60s".to_owned()), 1_060)
                .map_err(|error| format!("valid TTL must be schedulable: {error:?}"))?,
            super::TtlAction::Terminate,
            "authority must terminate exactly when the standing-delegation TTL expires"
        );
        Ok(())
    }

    #[test]
    fn exhausted_inference_requires_runtime_suspension() {
        let spend = steward_types::SpendSummary {
            observed_amount: "1.00".to_owned(),
            currency: "USD".to_owned(),
        };

        assert_eq!(
            inference_action(steward_ports::InferenceObservation::Exhausted {
                reference: "runtime-a".to_owned(),
                spend: spend.clone(),
            }),
            InferenceAction::Suspend {
                reference: "runtime-a".to_owned(),
                spend,
            },
            "budget exhaustion must select the non-human Running-to-Suspended transition"
        );
    }

    #[test]
    fn exhausted_runtime_cannot_reprovision_during_teardown() -> Result<(), String> {
        let mut runtime = fixture();
        let spend = steward_types::SpendSummary {
            observed_amount: "1.00".to_owned(),
            currency: "USD".to_owned(),
        };
        runtime.status = Some(steward_types::AgentRuntimeStatus {
            phase: Phase::Terminating,
            observed_generation: 3,
            spec_digest: "digest-a".to_owned(),
            refs: RuntimeRefs::default(),
            conditions: Vec::new(),
            spend: Some(spend.clone()),
        });

        assert_eq!(
            exhausted_spend_to_preserve(&runtime)
                .map_err(|error| format!("fixture exhaustion must be comparable: {error:?}"))?,
            Some(spend),
            "an exhausted runtime must finish suspension instead of provisioning a fresh key"
        );
        Ok(())
    }

    #[test]
    fn a_non_budget_spec_edit_cannot_clear_prior_budget_exhaustion() -> Result<(), String> {
        let mut runtime = fixture();
        runtime.metadata.generation = Some(4);
        runtime.spec.ttl = Duration("30m".to_owned());
        let spend = steward_types::SpendSummary {
            observed_amount: "1.00".to_owned(),
            currency: "USD".to_owned(),
        };
        runtime.status = Some(steward_types::AgentRuntimeStatus {
            phase: Phase::Suspended,
            observed_generation: 3,
            spec_digest: "prior-spec-digest".to_owned(),
            refs: RuntimeRefs::default(),
            conditions: Vec::new(),
            spend: Some(spend.clone()),
        });

        assert_eq!(
            exhausted_spend_to_preserve(&runtime)
                .map_err(|error| format!("fixture exhaustion must be comparable: {error:?}"))?,
            Some(spend),
            "a TTL-only edit must not provision a fresh monthly key after budget exhaustion"
        );
        Ok(())
    }

    #[test]
    fn a_higher_budget_can_clear_prior_budget_exhaustion() -> Result<(), String> {
        let mut runtime = fixture();
        runtime.metadata.generation = Some(4);
        runtime.spec.budget.monthly_limit = "2.00".to_owned();
        runtime.status = Some(steward_types::AgentRuntimeStatus {
            phase: Phase::Suspended,
            observed_generation: 3,
            spec_digest: "prior-spec-digest".to_owned(),
            refs: RuntimeRefs::default(),
            conditions: Vec::new(),
            spend: Some(steward_types::SpendSummary {
                observed_amount: "1.00".to_owned(),
                currency: "USD".to_owned(),
            }),
        });

        assert_eq!(
            exhausted_spend_to_preserve(&runtime).map_err(|error| {
                format!("fixture exhaustion must be comparable with raised budget: {error:?}")
            })?,
            None,
            "a budget raised above accumulated spend must allow reconciliation to resume"
        );
        Ok(())
    }

    struct FailingRevokeInference;

    impl InferencePlane for FailingRevokeInference {
        fn capabilities(&self) -> InferenceCapabilities {
            InferenceCapabilities::default()
        }

        async fn validate_configuration(
            &self,
            _models: &[ModelRef],
            _budget: &Budget,
        ) -> Result<(), PortError> {
            Ok(())
        }

        async fn provision(
            &self,
            _request: &InferenceRequest,
        ) -> Result<ProvisionedInference, PortError> {
            Err(PortError::Unsupported {
                operation: "test inference provisioning",
            })
        }

        async fn reconcile_configuration(
            &self,
            _request: &InferenceRequest,
        ) -> Result<(), PortError> {
            Ok(())
        }

        async fn observe(
            &self,
            _request: &InferenceRequest,
        ) -> Result<InferenceObservation, PortError> {
            Ok(InferenceObservation::Absent)
        }

        async fn revoke(&self, _request: &InferenceRequest) -> Result<(), PortError> {
            Err(PortError::Failed {
                reason: "fixture LiteLLM management outage".to_owned(),
            })
        }
    }

    struct PendingRevokeInference;

    impl InferencePlane for PendingRevokeInference {
        fn capabilities(&self) -> InferenceCapabilities {
            InferenceCapabilities::default()
        }

        async fn validate_configuration(
            &self,
            _models: &[ModelRef],
            _budget: &Budget,
        ) -> Result<(), PortError> {
            Ok(())
        }

        async fn provision(
            &self,
            _request: &InferenceRequest,
        ) -> Result<ProvisionedInference, PortError> {
            Err(PortError::Unsupported {
                operation: "test inference provisioning",
            })
        }

        async fn reconcile_configuration(
            &self,
            _request: &InferenceRequest,
        ) -> Result<(), PortError> {
            Ok(())
        }

        async fn observe(
            &self,
            _request: &InferenceRequest,
        ) -> Result<InferenceObservation, PortError> {
            Ok(InferenceObservation::Absent)
        }

        async fn revoke(&self, _request: &InferenceRequest) -> Result<(), PortError> {
            std::future::pending().await
        }
    }

    struct SignallingDeleteRuntime {
        deleted: Arc<AtomicBool>,
    }

    impl SandboxRuntime for SignallingDeleteRuntime {
        async fn ensure(&self, _request: &SandboxRequest) -> Result<SandboxObservation, PortError> {
            Err(PortError::Unsupported {
                operation: "test sandbox ensure",
            })
        }

        async fn delete(&self, _request: &SandboxRequest) -> Result<SandboxObservation, PortError> {
            self.deleted.store(true, Ordering::SeqCst);
            Ok(SandboxObservation::Absent)
        }
    }

    struct ProvisioningDeleteRuntime;

    impl SandboxRuntime for ProvisioningDeleteRuntime {
        async fn ensure(&self, _request: &SandboxRequest) -> Result<SandboxObservation, PortError> {
            Err(PortError::Unsupported {
                operation: "test sandbox ensure",
            })
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

    #[tokio::test]
    async fn authority_suspension_deletes_the_sandbox_during_a_litellm_outage() -> Result<(), String>
    {
        let runtime = fixture();
        let serialized_runtime = serde_json::to_vec(&runtime)
            .map_err(|error| format!("fixture runtime must be serializable: {error}"))?;
        let client = Client::new(
            service_fn(move |request: Request<KubeBody>| {
                let serialized_runtime = serialized_runtime.clone();
                async move {
                    let (status, body) = if request.method() == Method::DELETE
                        && request.uri().path().contains("/secrets/")
                    {
                        (
                            StatusCode::OK,
                            br#"{"apiVersion":"v1","kind":"Status","metadata":{},"status":"Success","code":200}"#
                                .to_vec(),
                        )
                    } else if request.method() == Method::PATCH
                        && request.uri().path().ends_with("/status")
                    {
                        (StatusCode::OK, serialized_runtime)
                    } else {
                        (
                            StatusCode::NOT_FOUND,
                            br#"{"apiVersion":"v1","kind":"Status","metadata":{},"status":"Failure","reason":"NotFound","code":404}"#
                                .to_vec(),
                        )
                    };
                    let mut response = Response::new(Body::from(body));
                    *response.status_mut() = status;
                    Ok::<_, Infallible>(response)
                }
            }),
            "team-a",
        );
        let sandbox = FakeSandboxRuntime {
            state: Mutex::new(FakeState {
                created: 1,
                deleted: 0,
                refs: Some(RuntimeRefs {
                    workspace: Some("workspace-a".to_owned()),
                    sandbox: Some("sandbox-a".to_owned()),
                    litellm_key: Some("key-a".to_owned()),
                }),
            }),
        };
        let api = kube::Api::<AgentRuntime>::namespaced(client.clone(), "team-a");

        let result = suspend_runtime_with_inference_cleanup(
            &runtime,
            &api,
            &sandbox,
            client,
            &FailingRevokeInference,
            None,
        )
        .await;

        assert!(
            result.is_err(),
            "failed credential revocation must remain retryable"
        );
        let deleted = sandbox
            .state
            .lock()
            .map_err(|_| "fixture sandbox lock must be readable".to_owned())?
            .deleted;
        assert_eq!(
            deleted, 1,
            "authority suspension must tear down the sandbox even when LiteLLM is unavailable"
        );
        Ok(())
    }

    #[tokio::test]
    async fn termination_deletes_the_sandbox_during_a_litellm_outage() -> Result<(), String> {
        let runtime = fixture();
        let client = Client::new(
            service_fn(|request: Request<KubeBody>| async move {
                let (status, body) = if request.method() == Method::DELETE
                    && request.uri().path().contains("/secrets/")
                {
                    (
                        StatusCode::OK,
                        br#"{"apiVersion":"v1","kind":"Status","metadata":{},"status":"Success","code":200}"#
                            .to_vec(),
                    )
                } else {
                    (
                        StatusCode::NOT_FOUND,
                        br#"{"apiVersion":"v1","kind":"Status","metadata":{},"status":"Failure","reason":"NotFound","code":404}"#
                            .to_vec(),
                    )
                };
                let mut response = Response::new(Body::from(body));
                *response.status_mut() = status;
                Ok::<_, Infallible>(response)
            }),
            "team-a",
        );
        let sandbox = FakeSandboxRuntime {
            state: Mutex::new(FakeState {
                created: 1,
                deleted: 0,
                refs: Some(RuntimeRefs {
                    workspace: Some("workspace-a".to_owned()),
                    sandbox: Some("sandbox-a".to_owned()),
                    litellm_key: Some("key-a".to_owned()),
                }),
            }),
        };

        let result = cleanup_runtime(&runtime, client, &FailingRevokeInference, &sandbox).await;

        assert!(
            result.is_err(),
            "failed inference revocation must keep the finalizer retryable"
        );
        assert_eq!(
            sandbox
                .state
                .lock()
                .map_err(|_| "fixture sandbox lock must be readable".to_owned())?
                .deleted,
            1,
            "termination must attempt sandbox teardown even when LiteLLM is unavailable"
        );
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn termination_starts_teardown_while_litellm_revocation_is_pending() -> Result<(), String>
    {
        let runtime = fixture();
        let secret_deleted = Arc::new(AtomicBool::new(false));
        let secret_deleted_for_service = secret_deleted.clone();
        let client = Client::new(
            service_fn(move |request: Request<KubeBody>| {
                if request.method() == Method::DELETE && request.uri().path().contains("/secrets/")
                {
                    secret_deleted_for_service.store(true, Ordering::SeqCst);
                }
                async move {
                    let mut response = Response::new(Body::from(
                        br#"{"apiVersion":"v1","kind":"Status","metadata":{},"status":"Success","code":200}"#
                            .to_vec(),
                    ));
                    *response.status_mut() = StatusCode::OK;
                    Ok::<_, Infallible>(response)
                }
            }),
            "team-a",
        );
        let sandbox_deleted = Arc::new(AtomicBool::new(false));
        let sandbox = SignallingDeleteRuntime {
            deleted: sandbox_deleted.clone(),
        };
        let result = tokio::time::timeout(
            StdDuration::from_secs(6),
            cleanup_runtime(&runtime, client, &PendingRevokeInference, &sandbox),
        )
        .await
        .map_err(|_| {
            "termination must requeue while LiteLLM revocation remains pending".to_owned()
        })?;

        assert!(
            result.is_err(),
            "pending inference revocation must keep finalizer cleanup retryable"
        );
        assert!(
            secret_deleted.load(Ordering::SeqCst),
            "termination must attempt credential deletion before requeueing"
        );
        assert!(
            sandbox_deleted.load(Ordering::SeqCst),
            "termination must attempt sandbox deletion before requeueing"
        );
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn authority_suspension_requeues_while_litellm_revocation_is_pending()
    -> Result<(), String> {
        let runtime = fixture();
        let serialized_runtime = serde_json::to_vec(&runtime)
            .map_err(|error| format!("fixture runtime must be serializable: {error}"))?;
        let secret_deleted = Arc::new(AtomicBool::new(false));
        let secret_deleted_for_service = secret_deleted.clone();
        let client = Client::new(
            service_fn(move |request: Request<KubeBody>| {
                let serialized_runtime = serialized_runtime.clone();
                if request.method() == Method::DELETE && request.uri().path().contains("/secrets/")
                {
                    secret_deleted_for_service.store(true, Ordering::SeqCst);
                }
                async move {
                    let body = if request.method() == Method::PATCH
                        && request.uri().path().ends_with("/status")
                    {
                        serialized_runtime
                    } else {
                        br#"{"apiVersion":"v1","kind":"Status","metadata":{},"status":"Success","code":200}"#
                            .to_vec()
                    };
                    let mut response = Response::new(Body::from(body));
                    *response.status_mut() = StatusCode::OK;
                    Ok::<_, Infallible>(response)
                }
            }),
            "team-a",
        );
        let sandbox_deleted = Arc::new(AtomicBool::new(false));
        let sandbox = SignallingDeleteRuntime {
            deleted: sandbox_deleted.clone(),
        };
        let api = kube::Api::<AgentRuntime>::namespaced(client.clone(), "team-a");

        let result = tokio::time::timeout(
            StdDuration::from_secs(6),
            suspend_runtime_with_inference_cleanup(
                &runtime,
                &api,
                &sandbox,
                client,
                &PendingRevokeInference,
                None,
            ),
        )
        .await
        .map_err(|_| {
            "authority suspension must requeue while LiteLLM revocation remains pending".to_owned()
        })?;

        assert!(
            result.is_err(),
            "pending inference revocation must keep authority suspension retryable"
        );
        assert!(
            secret_deleted.load(Ordering::SeqCst),
            "authority suspension must attempt credential deletion before requeueing"
        );
        assert!(
            sandbox_deleted.load(Ordering::SeqCst),
            "authority suspension must attempt sandbox deletion before requeueing"
        );
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn termination_reports_sandbox_progress_while_litellm_revocation_is_pending()
    -> Result<(), String> {
        let mut runtime = fixture();
        runtime.status = Some(AgentRuntimeStatus {
            phase: Phase::Running,
            observed_generation: 3,
            spec_digest: "fixture-digest".to_owned(),
            refs: RuntimeRefs {
                workspace: Some("workspace-a".to_owned()),
                sandbox: Some("sandbox-a".to_owned()),
                litellm_key: Some("key-a".to_owned()),
            },
            conditions: Vec::new(),
            spend: None,
        });
        let client = Client::new(
            service_fn(|_request: Request<KubeBody>| async move {
                let mut response = Response::new(Body::from(
                    br#"{"apiVersion":"v1","kind":"Status","metadata":{},"status":"Success","code":200}"#
                        .to_vec(),
                ));
                *response.status_mut() = StatusCode::OK;
                Ok::<_, Infallible>(response)
            }),
            "team-a",
        );

        let result = tokio::time::timeout(
            StdDuration::from_secs(6),
            cleanup_runtime(
                &runtime,
                client,
                &PendingRevokeInference,
                &ProvisioningDeleteRuntime,
            ),
        )
        .await
        .map_err(|_| {
            "termination must report sandbox progress while revocation remains pending".to_owned()
        })?
        .map_err(|error| format!("sandbox progress must remain observable: {error}"))?;

        let ReconcileDecision::Status(status) = result else {
            return Err("a pending sandbox deletion must return terminating status".to_owned());
        };
        assert_eq!(status.phase, Phase::Terminating);
        assert_eq!(
            status.refs.litellm_key.as_deref(),
            Some("key-a"),
            "terminating status must preserve the inference ref until revocation succeeds"
        );
        Ok(())
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

    #[tokio::test]
    async fn pending_initial_approval_does_not_provision_a_sandbox() -> Result<(), String> {
        let mut runtime = fixture();
        runtime.metadata.annotations.get_or_insert_default().insert(
            "agents.apelogic.ai/pending-approval".to_owned(),
            "request-digest".to_owned(),
        );
        let sandbox_runtime = FakeSandboxRuntime::default();

        let decision = reconcile_once(&runtime, ReconcileIntent::Ensure, &sandbox_runtime)
            .await
            .map_err(|error| format!("pending reconcile failed: {error:?}"))?;
        let ReconcileDecision::Status(status) = decision else {
            return Err("pending create must remain observable in status".to_owned());
        };
        assert_eq!(
            status.phase,
            Phase::Pending,
            "a parked initial create must remain inert until its approval is applied"
        );
        assert_eq!(
            sandbox_runtime
                .state
                .lock()
                .map_err(|_| "fake runtime state lock was poisoned")?
                .created,
            0,
            "a pending create must not allocate an OpenShell sandbox"
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
        FINALIZER, WebhookEnvelopeReader, WebhookFuture, WebhookModelCatalog, validate_admission,
        validate_admission_with_catalog, webhook_router,
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

    #[derive(Clone)]
    struct RejectUnpricedCatalog;

    impl WebhookModelCatalog for RejectUnpricedCatalog {
        fn validate_configuration<'a>(
            &'a self,
            _models: &'a [ModelRef],
            _budget: &'a Budget,
        ) -> WebhookFuture<'a, Result<(), steward_ports::PortError>> {
            Box::pin(async {
                Err(steward_ports::PortError::Rejected {
                    reason:
                        "models are absent from the priced inference catalog: provider-a/model-a"
                            .to_owned(),
                })
            })
        }
    }

    #[derive(Clone)]
    struct UsdOnlyCatalog;

    impl WebhookModelCatalog for UsdOnlyCatalog {
        fn validate_configuration<'a>(
            &'a self,
            _models: &'a [ModelRef],
            budget: &'a Budget,
        ) -> WebhookFuture<'a, Result<(), steward_ports::PortError>> {
            Box::pin(async move {
                if budget.currency == "USD" {
                    Ok(())
                } else {
                    Err(steward_ports::PortError::Rejected {
                        reason: format!(
                            "configured inference plane cannot enforce {} budgets",
                            budget.currency
                        ),
                    })
                }
            })
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
    async fn webhook_rejects_a_model_without_registered_cost() -> Result<(), String> {
        let mut value = admission_review_value();
        value["request"]["object"]["spec"]["budget"]["monthlyLimit"] = serde_json::json!("100.00");
        value["request"]["oldObject"] = serde_json::Value::Null;
        value["request"]["operation"] = serde_json::json!("CREATE");
        let review = serde_json::from_value::<AdmissionReview<AgentRuntime>>(value)
            .map_err(|error| format!("failed to construct unpriced-model review: {error}"))?;
        let request: AdmissionRequest<AgentRuntime> = review
            .try_into()
            .map_err(|error| format!("failed to read unpriced-model request: {error}"))?;

        let response =
            validate_admission_with_catalog(&request, &fake_envelopes(), &RejectUnpricedCatalog)
                .await;

        assert!(
            !response.allowed,
            "a model without registered cost must fail closed at admission"
        );
        assert_eq!(
            response.result.message,
            "AgentRuntime inference configuration validation failed closed: models are absent from the priced inference catalog: provider-a/model-a"
        );
        Ok(())
    }

    #[tokio::test]
    async fn webhook_does_not_require_inference_configuration_without_models() -> Result<(), String>
    {
        let mut value = admission_review_value();
        value["request"]["object"]["spec"]["llms"] = serde_json::json!([]);
        value["request"]["object"]["spec"]["budget"]["monthlyLimit"] = serde_json::json!("0");
        value["request"]["oldObject"] = serde_json::Value::Null;
        value["request"]["operation"] = serde_json::json!("CREATE");
        let review = serde_json::from_value::<AdmissionReview<AgentRuntime>>(value)
            .map_err(|error| format!("failed to construct model-free review: {error}"))?;
        let request: AdmissionRequest<AgentRuntime> = review
            .try_into()
            .map_err(|error| format!("failed to read model-free request: {error}"))?;

        let response =
            validate_admission_with_catalog(&request, &fake_envelopes(), &RejectUnpricedCatalog)
                .await;

        assert!(
            response.allowed,
            "a model-free inert runtime must not require a LiteLLM budget or catalog entry: {}",
            response.result.message
        );
        Ok(())
    }

    #[tokio::test]
    async fn webhook_rejects_unsupported_budget_currency_before_persistence() -> Result<(), String>
    {
        let mut value = admission_review_value();
        value["request"]["object"]["spec"]["budget"]["monthlyLimit"] = serde_json::json!("100.00");
        value["request"]["object"]["spec"]["budget"]["currency"] = serde_json::json!("EUR");
        value["request"]["oldObject"] = serde_json::Value::Null;
        value["request"]["operation"] = serde_json::json!("CREATE");
        let review = serde_json::from_value::<AdmissionReview<AgentRuntime>>(value)
            .map_err(|error| format!("failed to construct non-USD review: {error}"))?;
        let request: AdmissionRequest<AgentRuntime> = review
            .try_into()
            .map_err(|error| format!("failed to read non-USD request: {error}"))?;
        let mut envelopes = fake_envelopes();
        envelopes.envelope.spec.budget.currency = "EUR".to_owned();

        let response = validate_admission_with_catalog(&request, &envelopes, &UsdOnlyCatalog).await;

        assert!(
            !response.allowed,
            "a budget the configured inference plane cannot enforce must not be persisted"
        );
        assert_eq!(
            response.result.message,
            "AgentRuntime inference configuration validation failed closed: configured inference plane cannot enforce EUR budgets"
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
        value["request"]["object"]["metadata"]["managedFields"] = serde_json::json!([{
            "apiVersion": "agents.apelogic.ai/v1alpha1",
            "fieldsType": "FieldsV1",
            "manager": "steward-controller",
            "operation": "Apply"
        }]);
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
