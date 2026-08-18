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
use kube::api::{Api, DeleteParams, Patch, PatchParams, PostParams, Preconditions};
use kube::core::Request as KubeRequest;
use kube::core::admission::{AdmissionRequest, AdmissionResponse, Operation};
use kube::core::{ApiResource, DynamicObject, GroupVersionKind};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::finalizer::{Event, finalizer};
use kube::runtime::watcher;
use kube::{Client, Resource, ResourceExt};
use sha2::{Digest, Sha256};
use steward_admission::{
    AdmissionDecision, AdmissionDelta, Envelope, EnvelopeScopeKind, budget_is_exhausted,
    duration_seconds, evaluate_with_grants,
};
use steward_ports::{
    InferenceCapabilities, InferenceCredential, InferenceObservation, InferencePlane,
    InferenceRequest, MAX_TASK_OUTPUT_ARCHIVE_BYTES, ProvisionedInference, SandboxTaskOutput,
    SandboxTaskRequest, SandboxTaskRuntime,
};
pub use steward_ports::{PortError, SandboxObservation, SandboxRequest, SandboxRuntime};
use steward_store::{GrantReversion, PgStore, StoreError, TaskRecord};
use steward_types::{
    AgentRuntime, AgentRuntimeSpec, AgentRuntimeStatus, Duration, PENDING_APPROVAL_ANNOTATION,
    Phase, RuntimeId, RuntimeOwnership, RuntimeRefs, TaskPhase, runtime_activated_condition,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TaskRuntimeAction {
    Wait,
    CreateRuntime,
    Release,
    Execute,
    DeleteRuntime,
    MarkFinalized,
}

fn task_runtime_action(
    phase: TaskPhase,
    ownership: RuntimeOwnership,
    finalize_requested: bool,
    runtime_spec: &AgentRuntimeSpec,
    runtime: Option<&AgentRuntime>,
) -> TaskRuntimeAction {
    if finalize_requested {
        return match (ownership, runtime) {
            (RuntimeOwnership::Adopted, _) | (RuntimeOwnership::Provisioned, None) => {
                TaskRuntimeAction::MarkFinalized
            }
            (RuntimeOwnership::Provisioned, Some(runtime)) if runtime.spec == *runtime_spec => {
                TaskRuntimeAction::DeleteRuntime
            }
            (RuntimeOwnership::Provisioned, Some(_)) => TaskRuntimeAction::Wait,
        };
    }
    let Some(runtime) = runtime else {
        return if ownership == RuntimeOwnership::Provisioned
            && matches!(phase, TaskPhase::Submitted | TaskPhase::Queued)
        {
            TaskRuntimeAction::CreateRuntime
        } else {
            TaskRuntimeAction::Wait
        };
    };
    if runtime.spec != *runtime_spec
        || runtime
            .annotations()
            .contains_key(PENDING_APPROVAL_ANNOTATION)
    {
        return TaskRuntimeAction::Wait;
    }
    match phase {
        TaskPhase::Parked => TaskRuntimeAction::Release,
        TaskPhase::Queued
            if runtime
                .status
                .as_ref()
                .is_some_and(|status| status.phase == Phase::Running) =>
        {
            TaskRuntimeAction::Execute
        }
        _ => TaskRuntimeAction::Wait,
    }
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
    if is_pending_approval(runtime) {
        return Ok(TtlAction::Continue {
            requeue_after: StdDuration::from_secs(2),
        });
    }
    let activated_at = runtime.status.as_ref().and_then(|status| {
        status
            .conditions
            .iter()
            .find(|condition| condition.type_ == "Activated" && condition.status == "True")
            .map(|condition| condition.last_transition_time.0.as_second())
    });
    let created_at = activated_at.unwrap_or_else(|| {
        runtime
            .metadata
            .creation_timestamp
            .as_ref()
            .map(|created_at| created_at.0.as_second())
            .unwrap_or_default()
    });
    if activated_at.is_none() && runtime.metadata.creation_timestamp.is_none() {
        return Err(ReconcileError::InvalidSpec {
            reason: "persisted runtime has no creation timestamp".to_owned(),
        });
    }
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
    spec_digest(&runtime.spec)
}

fn spec_digest(spec: &AgentRuntimeSpec) -> Result<String, ReconcileError> {
    let serialized_spec =
        serde_json::to_vec(spec).map_err(|error| ReconcileError::InvalidSpec {
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
        conditions: runtime
            .status
            .as_ref()
            .map(|status| status.conditions.clone())
            .unwrap_or_default(),
        spend: None,
    }))
}

const FINALIZER: &str = "agents.apelogic.ai/runtime";
pub const MEMBER_ROLE_ANNOTATION: &str = "agents.apelogic.ai/member-role";
pub const SERVICE_PRINCIPAL_ANNOTATION: &str = "agents.apelogic.ai/service-principal";

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

fn has_activation_condition(runtime: &AgentRuntime) -> bool {
    runtime.status.as_ref().is_some_and(|status| {
        status
            .conditions
            .iter()
            .any(|condition| condition.type_ == "Activated" && condition.status == "True")
    })
}

fn activated_status(runtime: &AgentRuntime) -> Result<AgentRuntimeStatus, ReconcileError> {
    let observed_generation = runtime.metadata.generation.unwrap_or_default();
    let mut conditions = runtime
        .status
        .as_ref()
        .map(|status| status.conditions.clone())
        .unwrap_or_default();
    conditions.retain(|condition| condition.type_ != "Activated");
    conditions.push(runtime_activated_condition(observed_generation));
    Ok(AgentRuntimeStatus {
        phase: Phase::Admitted,
        observed_generation,
        spec_digest: runtime_spec_digest(runtime)?,
        refs: RuntimeRefs::default(),
        conditions,
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

pub async fn run_controller_with_planes<
    R: SandboxRuntime + SandboxTaskRuntime + Clone,
    I: InferencePlane,
>(
    client: Client,
    sandbox_runtime: R,
    inference: I,
    authority: PgStore,
) {
    let task_controller =
        run_task_controller(client.clone(), sandbox_runtime.clone(), authority.clone());
    let runtime_controller =
        run_controller_inner(client, sandbox_runtime, inference, Some(authority));
    tokio::select! {
        () = task_controller => eprintln!("task controller exited"),
        () = runtime_controller => eprintln!("runtime controller exited"),
    }
}

async fn run_task_controller<R: SandboxTaskRuntime>(
    client: Client,
    sandbox_runtime: R,
    authority: PgStore,
) {
    loop {
        match authority.task_work_items().await {
            Ok(tasks) => {
                for task in tasks {
                    if let Err(error) =
                        reconcile_task(&client, &sandbox_runtime, &authority, &task).await
                    {
                        eprintln!("task reconcile error: {error}");
                    }
                }
            }
            Err(error) => eprintln!("task queue read failed: {error}"),
        }
        tokio::time::sleep(StdDuration::from_secs(1)).await;
    }
}

async fn reconcile_task<R: SandboxTaskRuntime>(
    client: &Client,
    sandbox_runtime: &R,
    authority: &PgStore,
    task: &TaskRecord,
) -> Result<(), TaskControllerError> {
    let runtime = task_runtime(client, task).await?;
    match task_runtime_action(
        task.phase,
        task.runtime_ownership,
        task.finalize_requested,
        &task.runtime_spec,
        runtime.as_ref(),
    ) {
        TaskRuntimeAction::Wait => Ok(()),
        TaskRuntimeAction::CreateRuntime => create_task_runtime(client, authority, task).await,
        TaskRuntimeAction::Release => authority
            .release_parked_task(task.task_uid)
            .await
            .map(|_| ())
            .map_err(TaskControllerError::Store),
        TaskRuntimeAction::Execute => {
            if !authority
                .claim_task_execution(task.task_uid)
                .await
                .map_err(TaskControllerError::Store)?
            {
                return Ok(());
            }
            let runtime = runtime.ok_or_else(|| {
                TaskControllerError::InvalidState("claimed task runtime disappeared".to_owned())
            })?;
            let refs = runtime
                .status
                .as_ref()
                .map(|status| status.refs.clone())
                .ok_or_else(|| {
                    TaskControllerError::InvalidState(
                        "claimed task runtime has no observed references".to_owned(),
                    )
                })?;
            let input = task.input_archive.as_deref().ok_or_else(|| {
                TaskControllerError::InvalidState("queued task has no input archive".to_owned())
            })?;
            let result = sandbox_runtime
                .run_task(
                    &SandboxTaskRequest {
                        runtime: RuntimeId(task.runtime_uid.clone().ok_or_else(|| {
                            TaskControllerError::InvalidState(
                                "task has no bound runtime UID".to_owned(),
                            )
                        })?),
                        refs,
                        command: task.agent_command.clone(),
                    },
                    input,
                )
                .await;
            match result {
                Ok(SandboxTaskOutput { archive }) => {
                    if let Some(reason) = task_output_archive_failure(archive.len()) {
                        authority
                            .fail_task_execution(task.task_uid, reason)
                            .await
                            .map_err(TaskControllerError::Store)
                    } else {
                        authority
                            .complete_task_execution(task.task_uid, &archive)
                            .await
                            .map_err(TaskControllerError::Store)
                    }
                }
                Err(error) => {
                    let reason = task_failure_reason(&error);
                    authority
                        .fail_task_execution(task.task_uid, &reason)
                        .await
                        .map_err(TaskControllerError::Store)
                }
            }
        }
        TaskRuntimeAction::DeleteRuntime => {
            let runtime = runtime.ok_or_else(|| {
                TaskControllerError::InvalidState("task runtime disappeared".to_owned())
            })?;
            let namespace = runtime.namespace().ok_or_else(|| {
                TaskControllerError::InvalidState("task runtime has no namespace".to_owned())
            })?;
            let uid = runtime.metadata.uid.clone().ok_or_else(|| {
                TaskControllerError::InvalidState("task runtime has no UID".to_owned())
            })?;
            Api::<AgentRuntime>::namespaced(client.clone(), &namespace)
                .delete(
                    &runtime.name_any(),
                    &DeleteParams {
                        preconditions: Some(Preconditions {
                            uid: Some(uid),
                            resource_version: None,
                        }),
                        ..DeleteParams::default()
                    },
                )
                .await
                .map(|_| ())
                .map_err(TaskControllerError::Kubernetes)
        }
        TaskRuntimeAction::MarkFinalized => authority
            .mark_task_finalized(task.task_uid)
            .await
            .map_err(TaskControllerError::Store),
    }
}

async fn create_task_runtime(
    client: &Client,
    authority: &PgStore,
    task: &TaskRecord,
) -> Result<(), TaskControllerError> {
    let runtime = task_runtime_manifest(task)?;
    let namespace = runtime.namespace().ok_or_else(|| {
        TaskControllerError::InvalidState(
            "server-authored task runtime has no namespace".to_owned(),
        )
    })?;
    let api = Api::<AgentRuntime>::namespaced(client.clone(), &namespace);
    let created = match api.create(&PostParams::default(), &runtime).await {
        Ok(created) => created,
        Err(kube::Error::Api(response)) if response.code == 409 => {
            let existing = api
                .get(&runtime.name_any())
                .await
                .map_err(TaskControllerError::Kubernetes)?;
            if existing.spec != runtime.spec || existing.annotations() != runtime.annotations() {
                return Err(TaskControllerError::InvalidState(
                    "task runtime name is bound to unrelated desired state".to_owned(),
                ));
            }
            existing
        }
        Err(error) => return Err(TaskControllerError::Kubernetes(error)),
    };
    let runtime_uid = created.metadata.uid.as_deref().ok_or_else(|| {
        TaskControllerError::InvalidState("created task runtime has no UID".to_owned())
    })?;
    authority
        .bind_task_runtime(task.task_uid, runtime_uid, task.phase)
        .await
        .map(|_| ())
        .map_err(TaskControllerError::Store)
}

fn task_runtime_manifest(task: &TaskRecord) -> Result<AgentRuntime, TaskControllerError> {
    if task.runtime_ownership != RuntimeOwnership::Provisioned || task.runtime_uid.is_some() {
        return Err(TaskControllerError::InvalidState(
            "only an unbound provisioned task may create a runtime".to_owned(),
        ));
    }
    server_task_runtime_manifest(TaskRuntimeBinding::from(task))
}

struct TaskRuntimeBinding<'a> {
    runtime_spec: &'a AgentRuntimeSpec,
    submitter_service: &'a str,
    acting_user: Option<&'a str>,
    acting_user_id: Option<&'a str>,
    owner: &'a str,
    owner_user_id: Option<&'a str>,
    identity_binding_state: &'a str,
    runtime_namespace: &'a str,
    runtime_name: &'a str,
}

impl<'a> From<&'a TaskRecord> for TaskRuntimeBinding<'a> {
    fn from(task: &'a TaskRecord) -> Self {
        Self {
            runtime_spec: &task.runtime_spec,
            submitter_service: &task.submitter_service,
            acting_user: task.acting_user.as_deref(),
            acting_user_id: task.acting_user_id.as_deref(),
            owner: &task.owner,
            owner_user_id: task.owner_user_id.as_deref(),
            identity_binding_state: &task.identity_binding_state,
            runtime_namespace: &task.runtime_namespace,
            runtime_name: &task.runtime_name,
        }
    }
}

fn server_task_runtime_manifest(
    task: TaskRuntimeBinding<'_>,
) -> Result<AgentRuntime, TaskControllerError> {
    if task.identity_binding_state != "bound" {
        return Err(TaskControllerError::InvalidState(
            "task identity binding is not server-verified".to_owned(),
        ));
    }
    let owner_user_id = task.owner_user_id.ok_or_else(|| {
        TaskControllerError::InvalidState("task has no canonical owner".to_owned())
    })?;
    let authority = task
        .runtime_spec
        .canonical_authority
        .as_ref()
        .ok_or_else(|| {
            TaskControllerError::InvalidState("task runtime has no canonical authority".to_owned())
        })?;
    if authority.owner_user_id.as_str() != owner_user_id
        || authority.acting_user_id.as_ref().map(|id| id.as_str()) != task.acting_user_id
        || task.runtime_spec.owner.0 != task.owner
    {
        return Err(TaskControllerError::InvalidState(
            "task runtime authority does not match its server-authored owner".to_owned(),
        ));
    }
    match &task.runtime_spec.principal {
        steward_types::Principal::Service {
            name,
            acting_user: service_acting_user,
        } if name == task.submitter_service
            && service_acting_user.as_ref().map(|email| email.0.as_str()) == task.acting_user => {}
        _ => {
            return Err(TaskControllerError::InvalidState(
                "task runtime principal does not match its submitting service".to_owned(),
            ));
        }
    }
    if task.runtime_namespace.is_empty() || task.runtime_name.is_empty() {
        return Err(TaskControllerError::InvalidState(
            "task runtime name and namespace must be server-authored".to_owned(),
        ));
    }
    let mut runtime = AgentRuntime::new(task.runtime_name, task.runtime_spec.clone());
    runtime.metadata.namespace = Some(task.runtime_namespace.to_owned());
    runtime.metadata.annotations = Some(std::collections::BTreeMap::from([(
        SERVICE_PRINCIPAL_ANNOTATION.to_owned(),
        task.submitter_service.to_owned(),
    )]));
    Ok(runtime)
}

fn task_output_archive_failure(archive_bytes: usize) -> Option<&'static str> {
    (archive_bytes > MAX_TASK_OUTPUT_ARCHIVE_BYTES)
        .then_some("Task output archive exceeds the 64 MiB limit")
}

fn task_failure_reason(error: &PortError) -> String {
    match error {
        PortError::Unsupported { operation } => {
            format!("sandbox does not support task operation {operation}")
        }
        PortError::Rejected { reason } | PortError::Failed { reason } => reason.clone(),
        _ => "sandbox task execution failed".to_owned(),
    }
}

async fn task_runtime(
    client: &Client,
    task: &TaskRecord,
) -> Result<Option<AgentRuntime>, TaskControllerError> {
    let Some(expected_uid) = task.runtime_uid.as_deref() else {
        return Ok(None);
    };
    let runtime = Api::<AgentRuntime>::namespaced(client.clone(), &task.runtime_namespace)
        .get_opt(&task.runtime_name)
        .await
        .map_err(TaskControllerError::Kubernetes)?;
    Ok(runtime.filter(|runtime| runtime.metadata.uid.as_deref() == Some(expected_uid)))
}

#[derive(Debug)]
enum TaskControllerError {
    Kubernetes(kube::Error),
    Store(StoreError),
    InvalidState(String),
}

impl fmt::Display for TaskControllerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Kubernetes(error) => {
                write!(formatter, "Kubernetes task operation failed: {error}")
            }
            Self::Store(error) => write!(formatter, "task store operation failed: {error}"),
            Self::InvalidState(reason) => write!(formatter, "task state is invalid: {reason}"),
        }
    }
}

impl Error for TaskControllerError {}

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
                if !is_pending_approval(&runtime) && !has_activation_condition(&runtime) {
                    let released_hold = runtime
                        .status
                        .as_ref()
                        .is_some_and(|status| status.phase == Phase::Pending)
                        || if let Some(authority) = &context.authority {
                            authority
                                .grant_application(runtime.metadata.uid.as_deref().ok_or(
                                    ControllerError::Reconcile(ReconcileError::MissingRuntimeUid),
                                )?)
                                .await
                                .map_err(|error| {
                                    ControllerError::Reconcile(ReconcileError::Authority(
                                        error.to_string(),
                                    ))
                                })?
                                .is_some_and(|application| {
                                    application
                                        .application
                                        .base_pending_approval_digest
                                        .is_some()
                                })
                        } else {
                            false
                        };
                    if released_hold {
                        let status =
                            activated_status(&runtime).map_err(ControllerError::Reconcile)?;
                        api.patch_status(
                            &runtime.name_any(),
                            &PatchParams::default(),
                            &Patch::Merge(&status_merge_patch(&status)),
                        )
                        .await
                        .map_err(ControllerError::Kubernetes)?;
                        return Ok(Action::requeue(StdDuration::from_secs(2)));
                    }
                }
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
                    if let Some(authority) = &context.authority {
                        let runtime_uid =
                            runtime
                                .metadata
                                .uid
                                .as_deref()
                                .ok_or(ControllerError::Reconcile(
                                    ReconcileError::MissingRuntimeUid,
                                ))?;
                        if let Some(application) = authority
                            .grant_application(runtime.metadata.uid.as_deref().ok_or(
                                ControllerError::Reconcile(ReconcileError::MissingRuntimeUid),
                            )?)
                            .await
                            .map_err(|error| {
                                ControllerError::Reconcile(ReconcileError::Authority(
                                    error.to_string(),
                                ))
                            })?
                            && let Some(active_application) = authority
                                .retire_pending_approval_if_superseded(
                                    application.approval_id,
                                    application.approval_id,
                                    runtime_uid,
                                    "steward-controller",
                                    "active approval validated during pending convergence",
                                )
                                .await
                                .map_err(|error| {
                                    ControllerError::Reconcile(ReconcileError::Authority(
                                        error.to_string(),
                                    ))
                                })?
                            && let AuthorityAction::Restore(mut proposed) =
                                authority_application_action(&runtime, &active_application)
                                    .map_err(ControllerError::Reconcile)?
                        {
                            proposed.metadata = runtime.metadata.clone();
                            proposed
                                .metadata
                                .annotations
                                .get_or_insert_default()
                                .remove(PENDING_APPROVAL_ANNOTATION);
                            replace_pending_as_controller(&context.client, &proposed).await?;
                            return Ok(Action::requeue(StdDuration::from_secs(2)));
                        }
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
                        let (scope_kind, scope_ref) = authority_envelope_scope(
                            &reversion.proposed_spec,
                            &reversion.member_role,
                        )
                        .map_err(ControllerError::Reconcile)?;
                        let latest_envelope = authority
                            .latest_scoped_envelope(scope_kind, scope_ref)
                            .await
                            .map_err(|error| {
                                ControllerError::Reconcile(ReconcileError::Authority(
                                    error.to_string(),
                                ))
                            })?
                            .ok_or_else(|| {
                                ControllerError::Reconcile(ReconcileError::Authority(
                                    "grant principal no longer has an envelope".to_owned(),
                                ))
                            })?;
                        let surviving_grants = authority
                            .grants_for_runtime_scoped(
                                runtime.metadata.uid.as_deref().ok_or(
                                    ControllerError::Reconcile(ReconcileError::MissingRuntimeUid),
                                )?,
                                scope_kind,
                                scope_ref,
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
                                replace_grant_as_authority(
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
                        match authority_application_action(&runtime, &application.application)
                            .map_err(ControllerError::Reconcile)?
                        {
                            AuthorityAction::Restore(mut proposed) => {
                                proposed.metadata = runtime.metadata.clone();
                                proposed
                                    .metadata
                                    .annotations
                                    .get_or_insert_default()
                                    .remove(PENDING_APPROVAL_ANNOTATION);
                                replace_grant_as_authority(
                                    &context.client,
                                    &proposed,
                                    &application.application.actor,
                                    &application.application.member_role,
                                )
                                .await?;
                                return Ok(Action::requeue(StdDuration::from_secs(2)));
                            }
                            AuthorityAction::Continue | AuthorityAction::Suspend => {}
                        }
                    }
                    let Ok((scope_kind, scope_ref)) = runtime_envelope_scope(&runtime) else {
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
                        .latest_scoped_envelope(scope_kind, scope_ref)
                        .await
                        .map_err(|error| {
                            ControllerError::Reconcile(ReconcileError::Authority(error.to_string()))
                        })?
                        .ok_or_else(|| {
                            ControllerError::Reconcile(ReconcileError::Authority(
                                "runtime principal no longer has an envelope".to_owned(),
                            ))
                        })?;
                    let grants =
                        authority
                            .grants_for_runtime_scoped(
                                runtime.metadata.uid.as_deref().ok_or(
                                    ControllerError::Reconcile(ReconcileError::MissingRuntimeUid),
                                )?,
                                scope_kind,
                                scope_ref,
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
        revoke_inference_if_required(runtime, inference, &inference_request),
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

async fn revoke_inference_if_required<I: InferencePlane>(
    runtime: &AgentRuntime,
    inference: &I,
    request: &InferenceRequest,
) -> Result<(), ReconcileError> {
    let has_cached_reference = runtime
        .status
        .as_ref()
        .and_then(|status| status.refs.litellm_key.as_ref())
        .is_some();
    if request.models.is_empty() && !has_cached_reference {
        return Ok(());
    }
    revoke_inference_with_timeout(inference, request).await
}

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
        revoke_inference_if_required(runtime, inference, &request),
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
    if matches_stored_authority_spec(&runtime.spec, &reversion.base_spec) && base_is_admitted {
        return Ok(AuthorityAction::Continue);
    }
    if matches_stored_authority_spec(&runtime.spec, &reversion.proposed_spec) && base_is_admitted {
        if runtime
            .annotations()
            .contains_key(PENDING_APPROVAL_ANNOTATION)
        {
            return Err(ReconcileError::Authority(
                "applied grant unexpectedly retained a pending-approval marker".to_owned(),
            ));
        }
        let mut restored = runtime.clone();
        restored.spec = reversion.base_spec.clone();
        if let Some(pending_digest) = &reversion.base_pending_approval_digest {
            restored
                .metadata
                .annotations
                .get_or_insert_default()
                .insert(
                    PENDING_APPROVAL_ANNOTATION.to_owned(),
                    pending_digest.clone(),
                );
        }
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
    let pending_digest = runtime
        .annotations()
        .get(PENDING_APPROVAL_ANNOTATION)
        .map(String::as_str);
    if matches_stored_authority_spec(&runtime.spec, &application.base_spec) {
        validate_pending_application_provenance(pending_digest, application)?;
        let mut proposed = runtime.clone();
        proposed.spec = application.proposed_spec.clone();
        if let Some(canonical_authority) = runtime.spec.canonical_authority.clone() {
            proposed.spec.canonical_authority = Some(canonical_authority);
        }
        Ok(AuthorityAction::Restore(Box::new(proposed)))
    } else if matches_stored_authority_spec(&runtime.spec, &application.proposed_spec)
        && pending_digest.is_some()
    {
        validate_pending_application_provenance(pending_digest, application)?;
        Ok(AuthorityAction::Restore(Box::new(runtime.clone())))
    } else {
        Ok(AuthorityAction::Continue)
    }
}

fn matches_stored_authority_spec(
    runtime_spec: &AgentRuntimeSpec,
    stored_spec: &AgentRuntimeSpec,
) -> bool {
    let runtime_authority = runtime_spec.canonical_authority.as_ref();
    if let Some(stored_authority) = stored_spec.canonical_authority.as_ref()
        && runtime_authority != Some(stored_authority)
    {
        return false;
    }
    let mut runtime_without_authority = runtime_spec.clone();
    runtime_without_authority.canonical_authority = None;
    let mut stored_without_authority = stored_spec.clone();
    stored_without_authority.canonical_authority = None;
    runtime_without_authority == stored_without_authority
}

fn validate_pending_application_provenance(
    pending_digest: Option<&str>,
    application: &GrantReversion,
) -> Result<(), ReconcileError> {
    if pending_digest != application.base_pending_approval_digest.as_deref() {
        return Err(ReconcileError::Authority(
            "pending marker does not match approved request provenance".to_owned(),
        ));
    }
    if let Some(pending_digest) = pending_digest
        && spec_digest(&application.proposed_spec)? != pending_digest
    {
        return Err(ReconcileError::Authority(
            "pending marker does not match the approved proposed spec".to_owned(),
        ));
    }
    Ok(())
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
    let runtime_scope = runtime_envelope_scope(runtime)?;
    let base_scope = authority_envelope_scope(&authority.base_spec, &authority.member_role)?;
    let proposed_scope =
        authority_envelope_scope(&authority.proposed_spec, &authority.member_role)?;
    if runtime_scope != base_scope || runtime_scope != proposed_scope {
        return Err(ReconcileError::Authority(
            "grant authority envelope scope does not match the runtime binding".to_owned(),
        ));
    }
    if principal_actor(&authority.base_spec) != authority.actor
        || principal_actor(&authority.proposed_spec) != authority.actor
    {
        return Err(ReconcileError::Authority(
            "grant authority actor does not match its stored runtime specs".to_owned(),
        ));
    }
    Ok(())
}

fn principal_actor(spec: &AgentRuntimeSpec) -> &str {
    match &spec.principal {
        steward_types::Principal::User { acting_user } => &acting_user.0,
        steward_types::Principal::Service { name, .. } => name,
    }
}

fn authority_envelope_scope<'a>(
    spec: &AgentRuntimeSpec,
    scope_ref: &'a str,
) -> Result<(EnvelopeScopeKind, &'a str), ReconcileError> {
    match &spec.principal {
        steward_types::Principal::User { .. } => Ok((EnvelopeScopeKind::MemberRole, scope_ref)),
        steward_types::Principal::Service { name, .. } if name == scope_ref => {
            Ok((EnvelopeScopeKind::Service, scope_ref))
        }
        steward_types::Principal::Service { .. } => Err(ReconcileError::Authority(
            "service grant scope does not match its principal name".to_owned(),
        )),
    }
}

fn runtime_envelope_scope(
    runtime: &AgentRuntime,
) -> Result<(EnvelopeScopeKind, &str), ReconcileError> {
    match &runtime.spec.principal {
        steward_types::Principal::User { .. }
            if runtime
                .annotations()
                .contains_key(SERVICE_PRINCIPAL_ANNOTATION) =>
        {
            Err(ReconcileError::Authority(
                "user runtime carries a service envelope binding".to_owned(),
            ))
        }
        steward_types::Principal::User { .. } => runtime
            .annotations()
            .get(MEMBER_ROLE_ANNOTATION)
            .filter(|scope_ref| !scope_ref.is_empty())
            .map(|scope_ref| (EnvelopeScopeKind::MemberRole, scope_ref.as_str()))
            .ok_or_else(|| ReconcileError::Authority("runtime member role is missing".to_owned())),
        steward_types::Principal::Service { .. }
            if runtime.annotations().contains_key(MEMBER_ROLE_ANNOTATION) =>
        {
            Err(ReconcileError::Authority(
                "service runtime carries a member-role envelope binding".to_owned(),
            ))
        }
        steward_types::Principal::Service { name, .. } => runtime
            .annotations()
            .get(SERVICE_PRINCIPAL_ANNOTATION)
            .filter(|scope_ref| !scope_ref.is_empty() && scope_ref.as_str() == name)
            .map(|scope_ref| (EnvelopeScopeKind::Service, scope_ref.as_str()))
            .ok_or_else(|| {
                ReconcileError::Authority(
                    "runtime service envelope binding does not match its principal".to_owned(),
                )
            }),
    }
}

fn suspended_status(runtime: &AgentRuntime) -> Result<AgentRuntimeStatus, ControllerError> {
    Ok(AgentRuntimeStatus {
        phase: Phase::Suspended,
        observed_generation: runtime.metadata.generation.unwrap_or_default(),
        spec_digest: runtime_spec_digest(runtime).map_err(ControllerError::Reconcile)?,
        refs: RuntimeRefs::default(),
        conditions: runtime
            .status
            .as_ref()
            .map(|status| status.conditions.clone())
            .unwrap_or_default(),
        spend: None,
    })
}

async fn replace_as_authority(
    client: &Client,
    runtime: &AgentRuntime,
    actor: &str,
    member_role: &str,
) -> Result<(), ControllerError> {
    if is_pending_approval(runtime) {
        return replace_pending_as_controller(client, runtime).await;
    }
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

async fn replace_grant_as_authority(
    client: &Client,
    runtime: &AgentRuntime,
    actor: &str,
    scope_ref: &str,
) -> Result<(), ControllerError> {
    match &runtime.spec.principal {
        steward_types::Principal::User { .. } => {
            replace_as_authority(client, runtime, actor, scope_ref).await
        }
        steward_types::Principal::Service { .. } => {
            replace_pending_as_controller(client, runtime).await
        }
    }
}

async fn replace_pending_as_controller(
    client: &Client,
    runtime: &AgentRuntime,
) -> Result<(), ControllerError> {
    let namespace = runtime
        .namespace()
        .ok_or(ControllerError::Reconcile(ReconcileError::MissingNamespace))?;
    Api::<AgentRuntime>::namespaced(client.clone(), &namespace)
        .replace(&runtime.name_any(), &PostParams::default(), runtime)
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
        scope_kind: EnvelopeScopeKind,
        scope_ref: &'a str,
    ) -> WebhookFuture<'a, Result<Option<Envelope>, StoreError>>;

    fn grants_for_runtime<'a>(
        &'a self,
        runtime_uid: &'a str,
        scope_kind: EnvelopeScopeKind,
        scope_ref: &'a str,
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
        scope_kind: EnvelopeScopeKind,
        scope_ref: &'a str,
    ) -> WebhookFuture<'a, Result<Option<Envelope>, StoreError>> {
        Box::pin(async move { PgStore::latest_scoped_envelope(self, scope_kind, scope_ref).await })
    }

    fn grants_for_runtime<'a>(
        &'a self,
        runtime_uid: &'a str,
        scope_kind: EnvelopeScopeKind,
        scope_ref: &'a str,
        envelope_revision: i64,
    ) -> WebhookFuture<'a, Result<Vec<AdmissionDelta>, StoreError>> {
        Box::pin(async move {
            PgStore::grants_for_runtime_scoped(
                self,
                runtime_uid,
                scope_kind,
                scope_ref,
                envelope_revision,
            )
            .await
        })
    }
}

pub async fn validate_admission<R: WebhookEnvelopeReader>(
    request: &AdmissionRequest<AgentRuntime>,
    envelopes: &R,
) -> AdmissionResponse {
    validate_admission_with_trusted_writers(request, envelopes, &BTreeSet::new()).await
}

async fn validate_admission_with_trusted_writers<R: WebhookEnvelopeReader>(
    request: &AdmissionRequest<AgentRuntime>,
    envelopes: &R,
    trusted_writer_usernames: &BTreeSet<String>,
) -> AdmissionResponse {
    let response = AdmissionResponse::from(request);
    if request.operation == Operation::Delete {
        let Some(old_runtime) = request.old_object.as_ref() else {
            return response.deny("AgentRuntime DELETE admission request has no old object");
        };
        if !is_pending_approval(old_runtime) {
            return response;
        }
        let Some(username) = request.user_info.username.as_deref() else {
            return response.deny(
                "authenticated Kubernetes username is required to delete a pending AgentRuntime",
            );
        };
        if trusted_writer_usernames.contains(username) {
            return response;
        }
        return response.deny("pending AgentRuntime deletion requires a trusted Steward writer");
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
    if request.operation == Operation::Create
        && runtime.spec.canonical_authority.is_some()
        && !trusted_writer_usernames.contains(username)
    {
        return response
            .deny("canonical runtime authority may be set only by a trusted Steward writer");
    }
    if request.operation == Operation::Update {
        let Some(old_runtime) = request.old_object.as_ref() else {
            return response.deny("AgentRuntime UPDATE admission request has no old object");
        };
        if old_runtime.spec.canonical_authority != runtime.spec.canonical_authority {
            return response.deny("canonical runtime authority is immutable");
        }
    }
    let trusted_service_write = matches!(
        &runtime.spec.principal,
        steward_types::Principal::Service { .. }
    ) && trusted_writer_usernames.contains(username);
    let trusted_canonical_user_write = matches!(
        &runtime.spec.principal,
        steward_types::Principal::User { .. }
    ) && runtime.spec.canonical_authority.is_some()
        && trusted_writer_usernames.contains(username);
    let pending = runtime
        .annotations()
        .get(PENDING_APPROVAL_ANNOTATION)
        .map(String::as_str);
    let mut trusted_pending_transition = request.operation == Operation::Create
        && pending.is_some()
        && trusted_writer_usernames.contains(username);
    if request.operation == Operation::Create && pending.is_some() && !trusted_pending_transition {
        return response.deny(
            "agents.apelogic.ai/pending-approval may be set only by a trusted Steward writer",
        );
    }
    if request.operation == Operation::Update {
        let Some(old_runtime) = request.old_object.as_ref() else {
            return response.deny("AgentRuntime UPDATE admission request has no old object");
        };
        if old_runtime.spec.principal != runtime.spec.principal {
            return response
                .deny("AgentRuntime principal is immutable through the validating admission path");
        }
        let old_pending = old_runtime
            .annotations()
            .get(PENDING_APPROVAL_ANNOTATION)
            .map(String::as_str);
        let trusted_pending_writer =
            old_pending.is_some() && trusted_writer_usernames.contains(username);
        trusted_pending_transition =
            pending != old_pending && trusted_writer_usernames.contains(username);
        if pending != old_pending && pending.is_some() && !trusted_pending_transition {
            return response
                .deny("agents.apelogic.ai/pending-approval cannot be added or changed on UPDATE");
        }
        if pending != old_pending && !trusted_pending_transition {
            return response.deny(
                "agents.apelogic.ai/pending-approval may be removed only by a trusted Steward writer",
            );
        }
        if old_pending.is_some() && old_runtime.spec != runtime.spec && !trusted_pending_writer {
            return response
                .deny("pending AgentRuntime spec may be changed only by a trusted Steward writer");
        }
        trusted_pending_transition |= trusted_pending_writer;
        if !trusted_pending_transition && !trusted_service_write && !trusted_canonical_user_write {
            match &old_runtime.spec.principal {
                steward_types::Principal::User { acting_user } if acting_user.0 == username => {}
                _ => {
                    return response.deny(
                        "existing AgentRuntime acting user must match the authenticated Kubernetes username",
                    );
                }
            }
        }
    }
    if !trusted_pending_transition && !trusted_service_write && !trusted_canonical_user_write {
        match &runtime.spec.principal {
            steward_types::Principal::User { acting_user } if acting_user.0 == username => {}
            _ => {
                return response.deny(
                    "AgentRuntime acting user must match the authenticated Kubernetes username",
                );
            }
        }
    }
    let bound_role = runtime
        .annotations()
        .get(MEMBER_ROLE_ANNOTATION)
        .map(String::as_str);
    let bound_service = runtime
        .annotations()
        .get(SERVICE_PRINCIPAL_ANNOTATION)
        .map(String::as_str);
    let (scope_kind, scope_ref) = match &runtime.spec.principal {
        steward_types::Principal::User { .. } => {
            if bound_service.is_some() {
                return response
                    .deny("user AgentRuntime must not carry a service-principal annotation");
            }
            let member_role = if trusted_pending_transition || trusted_canonical_user_write {
                let Some(member_role) = bound_role.filter(|role| !role.is_empty()) else {
                    return response.deny("AgentRuntime member-role annotation is required");
                };
                member_role
            } else {
                let roles = request
                    .user_info
                    .groups
                    .iter()
                    .flatten()
                    .filter_map(|group| group.strip_prefix(MEMBER_ROLE_GROUP_PREFIX))
                    .filter(|role| !role.is_empty())
                    .collect::<BTreeSet<_>>();
                let Some(member_role) = roles.iter().next().copied().filter(|_| roles.len() == 1)
                else {
                    return response
                        .deny("exactly one authenticated member-role group is required");
                };
                member_role
            };
            if bound_role != Some(member_role) {
                return response.deny(
                    "AgentRuntime member-role annotation must match the authenticated member-role group",
                );
            }
            (EnvelopeScopeKind::MemberRole, member_role)
        }
        steward_types::Principal::Service { name, .. } => {
            if !trusted_service_write {
                return response
                    .deny("service AgentRuntime may be written only by a trusted Steward writer");
            }
            if name.is_empty() || bound_service != Some(name.as_str()) {
                return response.deny(
                    "AgentRuntime service-principal annotation must match the service principal name",
                );
            }
            if bound_role.is_some() {
                return response
                    .deny("service AgentRuntime must not carry a member-role annotation");
            }
            (EnvelopeScopeKind::Service, name.as_str())
        }
    };
    if request.operation == Operation::Update {
        let old = request.old_object.as_ref();
        let old_scope_binding = match scope_kind {
            EnvelopeScopeKind::MemberRole => old
                .and_then(|runtime| runtime.annotations().get(MEMBER_ROLE_ANNOTATION))
                .map(String::as_str),
            EnvelopeScopeKind::Service => old
                .and_then(|runtime| runtime.annotations().get(SERVICE_PRINCIPAL_ANNOTATION))
                .map(String::as_str),
        };
        if old_scope_binding != Some(scope_ref) {
            return response.deny("AgentRuntime envelope scope binding is immutable");
        }
    }
    let envelope = match envelopes.latest_envelope(scope_kind, scope_ref).await {
        Ok(Some(envelope)) => envelope,
        Ok(None) => return response.deny("no envelope exists for the authenticated principal"),
        Err(error) => {
            return response.deny(format!("principal envelope lookup failed closed: {error}"));
        }
    };
    let grants = match runtime.metadata.uid.as_deref() {
        Some(runtime_uid) => match envelopes
            .grants_for_runtime(runtime_uid, scope_kind, scope_ref, envelope.revision)
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
    validate_admission_with_catalog_for_writers(request, envelopes, catalog, &BTreeSet::new()).await
}

async fn validate_admission_with_catalog_for_writers<
    R: WebhookEnvelopeReader,
    C: WebhookModelCatalog,
>(
    request: &AdmissionRequest<AgentRuntime>,
    envelopes: &R,
    catalog: &C,
    trusted_writer_usernames: &BTreeSet<String>,
) -> AdmissionResponse {
    let response =
        validate_admission_with_trusted_writers(request, envelopes, trusted_writer_usernames).await;
    if !response.allowed {
        return response;
    }
    if request.operation == Operation::Delete {
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
    validate_admission_with_trusted_writers(
        request,
        envelopes,
        &BTreeSet::from([controller_username.to_owned()]),
    )
    .await
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
    trusted_writer_usernames: BTreeSet<String>,
}

pub fn webhook_router<R: WebhookEnvelopeReader>(envelopes: R) -> Router {
    webhook_router_with_controller(envelopes, AllowConfiguredModels, None, BTreeSet::new())
}

pub fn webhook_router_for_controller<R: WebhookEnvelopeReader>(
    envelopes: R,
    controller_username: String,
) -> Router {
    webhook_router_with_controller(
        envelopes,
        AllowConfiguredModels,
        Some(controller_username.clone()),
        BTreeSet::from([controller_username]),
    )
}

pub fn webhook_router_for_trusted_writer<R: WebhookEnvelopeReader>(
    envelopes: R,
    writer_username: String,
) -> Router {
    webhook_router_with_controller(
        envelopes,
        AllowConfiguredModels,
        None,
        BTreeSet::from([writer_username]),
    )
}

pub fn webhook_router_for_controller_with_catalog<
    R: WebhookEnvelopeReader,
    C: WebhookModelCatalog,
>(
    envelopes: R,
    catalog: C,
    controller_username: String,
    apiserver_username: String,
) -> Router {
    webhook_router_with_controller(
        envelopes,
        catalog,
        Some(controller_username.clone()),
        BTreeSet::from([controller_username, apiserver_username]),
    )
}

fn webhook_router_with_controller<R: WebhookEnvelopeReader, C: WebhookModelCatalog>(
    envelopes: R,
    catalog: C,
    controller_username: Option<String>,
    trusted_writer_usernames: BTreeSet<String>,
) -> Router {
    Router::new()
        .route("/validate-agent-runtime", post(webhook_handler::<R, C>))
        .with_state(WebhookState {
            envelopes,
            catalog,
            controller_username,
            trusted_writer_usernames,
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
                validate_admission_with_catalog_for_writers(
                    &request,
                    &state.envelopes,
                    &state.catalog,
                    &state.trusted_writer_usernames,
                )
                .await
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
    use kube::client::Body as KubeBody;
    use kube::{Client, ResourceExt};
    use steward_admission::{AdmissionDelta, Envelope, EnvelopeSpec};
    use steward_ports::{
        InferenceCapabilities, InferenceObservation, InferencePlane, InferenceRequest,
        MAX_TASK_OUTPUT_ARCHIVE_BYTES, PortError, ProvisionedInference, SandboxObservation,
        SandboxRequest, SandboxRuntime,
    };
    use steward_store::GrantReversion;
    use steward_types::{
        AgentRuntime, AgentRuntimeSpec, AgentRuntimeStatus, AgentType, Budget,
        CanonicalAuthorityBinding, CanonicalUserId, Duration, Email, ModelRef,
        PENDING_APPROVAL_ANNOTATION, Phase, Principal, RuntimeOwnership, RuntimeRefs, TaskPhase,
    };
    use tower::service_fn;

    use super::{
        AuthorityAction, InferenceAction, MEMBER_ROLE_ANNOTATION, ReconcileDecision,
        ReconcileIntent, SERVICE_PRINCIPAL_ANNOTATION, TaskRuntimeAction, TaskRuntimeBinding,
        authority_action, authority_application_action, cleanup_runtime,
        exhausted_spend_to_preserve, inference_action, reconcile_once, replace_as_authority,
        runtime_authority_action, runtime_ttl_action, server_task_runtime_manifest,
        status_merge_patch, suspend_runtime_with_inference_cleanup, task_output_archive_failure,
        task_runtime_action, ttl_action,
    };

    #[test]
    fn task_state_table_releases_holds_executes_running_runtimes_and_preserves_ownership()
    -> Result<(), String> {
        let mut runtime = fixture();
        runtime.status = Some(AgentRuntimeStatus {
            phase: Phase::Running,
            observed_generation: 3,
            spec_digest: "runtime-spec-digest".to_owned(),
            refs: RuntimeRefs {
                workspace: Some("workspace-a".to_owned()),
                sandbox: Some("sandbox-a".to_owned()),
                litellm_key: None,
            },
            conditions: Vec::new(),
            spend: None,
        });
        assert_eq!(
            task_runtime_action(
                TaskPhase::Queued,
                RuntimeOwnership::Provisioned,
                false,
                &runtime.spec,
                Some(&runtime),
            ),
            TaskRuntimeAction::Execute
        );

        runtime.metadata.annotations.get_or_insert_default().insert(
            PENDING_APPROVAL_ANNOTATION.to_owned(),
            "request-digest".to_owned(),
        );
        assert_eq!(
            task_runtime_action(
                TaskPhase::Parked,
                RuntimeOwnership::Provisioned,
                false,
                &runtime.spec,
                Some(&runtime),
            ),
            TaskRuntimeAction::Wait
        );
        runtime
            .metadata
            .annotations
            .get_or_insert_default()
            .remove(PENDING_APPROVAL_ANNOTATION);
        assert_eq!(
            task_runtime_action(
                TaskPhase::Parked,
                RuntimeOwnership::Provisioned,
                false,
                &runtime.spec,
                Some(&runtime),
            ),
            TaskRuntimeAction::Release
        );

        assert_eq!(
            task_runtime_action(
                TaskPhase::Cancelled,
                RuntimeOwnership::Provisioned,
                true,
                &runtime.spec,
                Some(&runtime),
            ),
            TaskRuntimeAction::DeleteRuntime
        );
        assert_eq!(
            task_runtime_action(
                TaskPhase::Cancelled,
                RuntimeOwnership::Adopted,
                true,
                &runtime.spec,
                Some(&runtime),
            ),
            TaskRuntimeAction::MarkFinalized
        );

        let mut other_owner_spec = runtime.spec.clone();
        other_owner_spec.canonical_authority = Some(
            CanonicalAuthorityBinding::new(
                CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")
                    .map_err(|error| error.to_string())?,
                Some(
                    CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")
                        .map_err(|error| error.to_string())?,
                ),
            )
            .map_err(|error| error.to_string())?,
        );
        runtime.spec.canonical_authority = Some(
            CanonicalAuthorityBinding::new(
                CanonicalUserId::parse("usr_abcdef0123456789abcdef0123456789")
                    .map_err(|error| error.to_string())?,
                Some(
                    CanonicalUserId::parse("usr_abcdef0123456789abcdef0123456789")
                        .map_err(|error| error.to_string())?,
                ),
            )
            .map_err(|error| error.to_string())?,
        );
        assert_eq!(
            task_runtime_action(
                TaskPhase::Cancelled,
                RuntimeOwnership::Provisioned,
                true,
                &other_owner_spec,
                Some(&runtime),
            ),
            TaskRuntimeAction::Wait,
            "a corrupted task record must never delete another canonical owner's runtime"
        );
        Ok(())
    }

    #[test]
    fn provisioned_submitted_task_without_a_runtime_is_not_left_waiting() {
        let runtime = fixture();

        assert_eq!(
            task_runtime_action(
                TaskPhase::Submitted,
                RuntimeOwnership::Provisioned,
                false,
                &runtime.spec,
                None,
            ),
            TaskRuntimeAction::CreateRuntime,
            "a server-authored provisioned task with no bound runtime must drive controller creation rather than wait forever"
        );
    }

    #[test]
    fn task_runtime_manifest_rejects_a_record_with_tampered_owner_authority() -> Result<(), String>
    {
        let mut runtime = fixture();
        runtime.spec.principal = Principal::Service {
            name: "steward-run".to_owned(),
            acting_user: Some(Email("alice@example.com".to_owned())),
        };
        runtime.spec.canonical_authority = Some(CanonicalAuthorityBinding::new(
            CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?,
            Some(CanonicalUserId::parse(
                "usr_0123456789abcdef0123456789abcdef",
            )?),
        )?);
        assert!(
            server_task_runtime_manifest(TaskRuntimeBinding {
                runtime_spec: &runtime.spec,
                submitter_service: "steward-run",
                acting_user: Some("alice@example.com"),
                acting_user_id: Some("usr_0123456789abcdef0123456789abcdef"),
                owner: "alice@example.com",
                owner_user_id: Some("usr_0123456789abcdef0123456789abcdef"),
                identity_binding_state: "bound",
                runtime_namespace: "team-a",
                runtime_name: "task-a",
            },)
            .is_ok(),
            "the controller must accept the exact server-authored Task record"
        );
        assert!(
            server_task_runtime_manifest(TaskRuntimeBinding {
                runtime_spec: &runtime.spec,
                submitter_service: "steward-run",
                acting_user: Some("alice@example.com"),
                acting_user_id: Some("usr_abcdef0123456789abcdef0123456789"),
                owner: "alice@example.com",
                owner_user_id: Some("usr_abcdef0123456789abcdef0123456789"),
                identity_binding_state: "bound",
                runtime_namespace: "team-a",
                runtime_name: "task-a",
            },)
            .is_err(),
            "a task record whose owner differs from the canonical runtime authority must fail before the controller creates any runtime"
        );
        Ok(())
    }

    #[test]
    fn oversized_task_output_is_rejected_before_persistence() {
        assert_eq!(
            task_output_archive_failure(MAX_TASK_OUTPUT_ARCHIVE_BYTES),
            None,
            "the documented 64 MiB boundary must remain usable"
        );
        assert_eq!(
            task_output_archive_failure(MAX_TASK_OUTPUT_ARCHIVE_BYTES + 1),
            Some("Task output archive exceeds the 64 MiB limit"),
            "adapter output over the contract limit must fail before Postgres persistence"
        );
    }

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
    fn expired_ttl_does_not_terminate_a_pending_approval_placeholder() -> Result<(), String> {
        let mut runtime = fixture();
        runtime.metadata.creation_timestamp = Some(
            serde_json::from_value(serde_json::json!("1970-01-01T00:00:00Z"))
                .map_err(|error| format!("failed to construct old creation timestamp: {error}"))?,
        );
        runtime.metadata.annotations.get_or_insert_default().insert(
            "agents.apelogic.ai/pending-approval".to_owned(),
            "request-digest".to_owned(),
        );

        assert_eq!(
            runtime_ttl_action(&runtime).map_err(|error| format!(
                "pending placeholder must remain schedulable: {error:?}"
            ))?,
            super::TtlAction::Continue {
                requeue_after: StdDuration::from_secs(2),
            },
            "a pending approval is a governance hold and cannot enter TTL deletion"
        );
        Ok(())
    }

    #[test]
    fn approved_runtime_ttl_uses_its_controller_owned_activation_time() -> Result<(), String> {
        let mut runtime = fixture();
        runtime.spec.ttl = Duration("60s".to_owned());
        runtime.metadata.creation_timestamp = Some(
            serde_json::from_value(serde_json::json!("1970-01-01T00:00:00Z"))
                .map_err(|error| format!("failed to construct old creation timestamp: {error}"))?,
        );
        runtime.status = Some(AgentRuntimeStatus {
            phase: Phase::Admitted,
            observed_generation: 3,
            spec_digest: "approved-spec-digest".to_owned(),
            refs: RuntimeRefs::default(),
            conditions: vec![
                serde_json::from_value(serde_json::json!({
                    "type": "Activated",
                    "status": "True",
                    "observedGeneration": 3,
                    "lastTransitionTime": "2999-01-01T00:00:00Z",
                    "reason": "PendingApprovalReleased",
                    "message": "standing delegation TTL starts at hold release"
                }))
                .map_err(|error| format!("failed to construct activation condition: {error}"))?,
            ],
            spend: None,
        });

        assert_eq!(
            runtime_ttl_action(&runtime)
                .map_err(|error| format!("activated runtime TTL must be readable: {error:?}"))?,
            super::TtlAction::Continue {
                requeue_after: StdDuration::from_secs(60),
            },
            "placeholder age must not consume the approved standing delegation TTL"
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

    fn running_model_free_runtime(litellm_key: Option<&str>) -> AgentRuntime {
        let mut runtime = fixture();
        runtime.spec.llms.clear();
        runtime.status = Some(AgentRuntimeStatus {
            phase: Phase::Running,
            observed_generation: 3,
            spec_digest: "fixture-digest".to_owned(),
            refs: RuntimeRefs {
                workspace: Some("workspace-a".to_owned()),
                sandbox: Some("sandbox-a".to_owned()),
                litellm_key: litellm_key.map(str::to_owned),
            },
            conditions: Vec::new(),
            spend: None,
        });
        runtime
    }

    fn successful_cleanup_client(
        runtime: &AgentRuntime,
    ) -> Result<(Client, Arc<AtomicBool>), String> {
        let serialized_runtime = serde_json::to_vec(runtime)
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
        Ok((client, secret_deleted))
    }

    #[tokio::test]
    async fn authority_suspension_of_model_free_runtime_without_inference_ref_skips_revocation()
    -> Result<(), String> {
        let runtime = running_model_free_runtime(None);
        let (client, secret_deleted) = successful_cleanup_client(&runtime)?;
        let sandbox_deleted = Arc::new(AtomicBool::new(false));
        let sandbox = SignallingDeleteRuntime {
            deleted: sandbox_deleted.clone(),
        };
        let api = kube::Api::<AgentRuntime>::namespaced(client.clone(), "team-a");

        suspend_runtime_with_inference_cleanup(
            &runtime,
            &api,
            &sandbox,
            client,
            &FailingRevokeInference,
            None,
        )
        .await
        .map_err(|error| {
            format!(
                "model-free suspension without an inference reference must not depend on inference revocation: {error}"
            )
        })?;

        assert!(
            sandbox_deleted.load(Ordering::SeqCst),
            "model-free suspension must still delete the sandbox"
        );
        assert!(
            secret_deleted.load(Ordering::SeqCst),
            "model-free suspension must still delete any credential Secret"
        );
        Ok(())
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
    async fn termination_of_model_free_runtime_without_inference_ref_skips_revocation()
    -> Result<(), String> {
        let runtime = running_model_free_runtime(None);
        let (client, secret_deleted) = successful_cleanup_client(&runtime)?;
        let sandbox_deleted = Arc::new(AtomicBool::new(false));
        let sandbox = SignallingDeleteRuntime {
            deleted: sandbox_deleted.clone(),
        };

        let decision = cleanup_runtime(&runtime, client, &FailingRevokeInference, &sandbox)
            .await
            .map_err(|error| {
                format!(
                    "model-free termination without an inference reference must not depend on inference revocation: {error}"
                )
            })?;

        assert_eq!(decision, ReconcileDecision::Deleted);
        assert!(
            sandbox_deleted.load(Ordering::SeqCst),
            "model-free termination must still delete the sandbox"
        );
        assert!(
            secret_deleted.load(Ordering::SeqCst),
            "model-free termination must still delete any credential Secret"
        );
        Ok(())
    }

    #[tokio::test]
    async fn termination_with_removed_models_and_cached_inference_ref_still_revokes()
    -> Result<(), String> {
        let runtime = running_model_free_runtime(Some("key-a"));
        let (client, secret_deleted) = successful_cleanup_client(&runtime)?;
        let sandbox_deleted = Arc::new(AtomicBool::new(false));
        let sandbox = SignallingDeleteRuntime {
            deleted: sandbox_deleted.clone(),
        };

        let result = cleanup_runtime(&runtime, client, &FailingRevokeInference, &sandbox).await;

        assert!(
            result.is_err(),
            "a cached inference reference must keep revocation retryable after models are removed"
        );
        assert!(
            sandbox_deleted.load(Ordering::SeqCst),
            "inference revocation failure must not prevent sandbox teardown"
        );
        assert!(
            secret_deleted.load(Ordering::SeqCst),
            "inference revocation failure must not prevent credential Secret deletion"
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
                canonical_authority: None,
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
                runner: steward_types::RunnerRequirements::default(),
                bindings: None,
            },
        );
        runtime.metadata.namespace = Some("team-a".to_owned());
        runtime.metadata.uid = Some("runtime-uid-a".to_owned());
        runtime.metadata.generation = Some(3);
        runtime.metadata.annotations = Some(std::collections::BTreeMap::from([(
            MEMBER_ROLE_ANNOTATION.to_owned(),
            "engineer".to_owned(),
        )]));
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
                runner: steward_types::RunnerRequirements::default(),
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
            base_pending_approval_digest: None,
        }
    }

    #[test]
    fn service_grant_authority_is_bound_to_its_service_annotation_and_actor() -> Result<(), String>
    {
        let mut runtime = fixture();
        runtime.spec.principal = Principal::Service {
            name: "scheduled-scanner".to_owned(),
            acting_user: None,
        };
        runtime.metadata.annotations = Some(std::collections::BTreeMap::from([(
            SERVICE_PRINCIPAL_ANNOTATION.to_owned(),
            "scheduled-scanner".to_owned(),
        )]));
        let mut application = grant_reversion(&runtime);
        application.actor = "scheduled-scanner".to_owned();
        application.member_role = "scheduled-scanner".to_owned();

        let action = authority_application_action(&runtime, &application)
            .map_err(|error| format!("matching service grant must apply: {error:?}"))?;
        assert!(matches!(action, AuthorityAction::Restore(_)));

        runtime.metadata.annotations.get_or_insert_default().insert(
            SERVICE_PRINCIPAL_ANNOTATION.to_owned(),
            "different-service".to_owned(),
        );
        assert!(
            authority_application_action(&runtime, &application).is_err(),
            "a service grant must not cross its annotated service scope"
        );
        Ok(())
    }

    #[test]
    fn runtime_scope_rejects_cross_kind_annotations() {
        let mut user_runtime = fixture();
        user_runtime
            .metadata
            .annotations
            .get_or_insert_default()
            .insert(
                SERVICE_PRINCIPAL_ANNOTATION.to_owned(),
                "scheduled-scanner".to_owned(),
            );
        assert!(
            super::runtime_envelope_scope(&user_runtime).is_err(),
            "a user runtime must not reconcile with a service envelope binding"
        );

        let mut service_runtime = fixture();
        service_runtime.spec.principal = Principal::Service {
            name: "scheduled-scanner".to_owned(),
            acting_user: None,
        };
        service_runtime
            .metadata
            .annotations
            .get_or_insert_default()
            .insert(
                SERVICE_PRINCIPAL_ANNOTATION.to_owned(),
                "scheduled-scanner".to_owned(),
            );
        assert!(
            super::runtime_envelope_scope(&service_runtime).is_err(),
            "a service runtime must not reconcile with a member-role binding"
        );
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
        assert!(
            !restored
                .annotations()
                .contains_key("agents.apelogic.ai/pending-approval"),
            "an edit-grant reversion must not invent a pending hold"
        );
        Ok(())
    }

    #[test]
    fn expired_initial_create_grant_restores_its_exact_pending_marker() -> Result<(), String> {
        let mut runtime = fixture();
        let mut reversion = grant_reversion(&runtime);
        reversion.base_pending_approval_digest = Some("request-digest".to_owned());
        runtime.spec = reversion.proposed_spec.clone();

        let action = authority_action(&runtime, &reversion, &envelope("1.00"), &[])
            .map_err(|error| format!("authority evaluation failed: {error:?}"))?;
        let AuthorityAction::Restore(restored) = action else {
            return Err("expired initial-create authority must restore its hold".to_owned());
        };
        assert_eq!(restored.spec, reversion.base_spec);
        assert_eq!(
            restored
                .annotations()
                .get("agents.apelogic.ai/pending-approval")
                .map(String::as_str),
            Some("request-digest"),
            "initial-create reversion must restore the stored marker verbatim"
        );
        Ok(())
    }

    #[test]
    fn expired_canonical_initial_create_restores_its_pending_marker() -> Result<(), String> {
        let mut runtime = fixture();
        let canonical_user_id = CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?;
        runtime.spec.canonical_authority = Some(
            CanonicalAuthorityBinding::new(canonical_user_id.clone(), Some(canonical_user_id))
                .map_err(|error| format!("failed to construct canonical authority: {error}"))?,
        );
        let mut reversion = grant_reversion(&runtime);
        reversion.proposed_spec.canonical_authority = None;
        reversion.base_pending_approval_digest = Some("request-digest".to_owned());
        runtime.spec = reversion.proposed_spec.clone();
        runtime.spec.canonical_authority = reversion.base_spec.canonical_authority.clone();

        let action = authority_action(&runtime, &reversion, &envelope("1.00"), &[])
            .map_err(|error| format!("canonical authority reversion failed: {error:?}"))?;
        let AuthorityAction::Restore(restored) = action else {
            return Err("expired canonical initial create must restore its hold".to_owned());
        };
        assert_eq!(restored.spec, reversion.base_spec);
        assert_eq!(
            restored
                .annotations()
                .get("agents.apelogic.ai/pending-approval")
                .map(String::as_str),
            Some("request-digest"),
        );
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
    fn approved_initial_create_converges_only_with_matching_provenance() -> Result<(), String> {
        let mut runtime = fixture();
        let mut application = grant_reversion(&runtime);
        let digest = super::spec_digest(&application.proposed_spec)
            .map_err(|error| format!("failed to digest proposed spec: {error:?}"))?;
        runtime.metadata.annotations.get_or_insert_default().insert(
            "agents.apelogic.ai/pending-approval".to_owned(),
            digest.clone(),
        );
        application.base_pending_approval_digest = Some(digest);

        let action = authority_application_action(&runtime, &application)
            .map_err(|error| format!("matching authority failed validation: {error:?}"))?;
        let AuthorityAction::Restore(proposed) = action else {
            return Err("matching initial-create authority did not converge".to_owned());
        };
        assert_eq!(proposed.spec, application.proposed_spec);
        Ok(())
    }

    #[test]
    fn approved_canonical_initial_create_retains_its_immutable_authority() -> Result<(), String> {
        let mut runtime = fixture();
        let canonical_user_id = CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?;
        runtime.spec.canonical_authority = Some(
            CanonicalAuthorityBinding::new(canonical_user_id.clone(), Some(canonical_user_id))
                .map_err(|error| format!("failed to construct canonical authority: {error}"))?,
        );
        let mut application = grant_reversion(&runtime);
        application.proposed_spec.canonical_authority = None;
        let digest = super::spec_digest(&application.proposed_spec)
            .map_err(|error| format!("failed to digest proposed canonical spec: {error:?}"))?;
        runtime.metadata.annotations.get_or_insert_default().insert(
            "agents.apelogic.ai/pending-approval".to_owned(),
            digest.clone(),
        );
        application.base_pending_approval_digest = Some(digest);

        let action = authority_application_action(&runtime, &application)
            .map_err(|error| format!("canonical authority application failed: {error:?}"))?;
        let AuthorityAction::Restore(proposed) = action else {
            return Err("approved canonical initial create did not converge".to_owned());
        };
        assert_eq!(
            proposed.spec.canonical_authority,
            runtime.spec.canonical_authority
        );
        Ok(())
    }

    #[test]
    fn approved_spec_with_its_pending_marker_still_releases_the_hold() -> Result<(), String> {
        let mut runtime = fixture();
        let mut application = grant_reversion(&runtime);
        let digest = super::spec_digest(&application.proposed_spec)
            .map_err(|error| format!("failed to digest proposed spec: {error:?}"))?;
        runtime.spec = application.proposed_spec.clone();
        runtime.metadata.annotations.get_or_insert_default().insert(
            "agents.apelogic.ai/pending-approval".to_owned(),
            digest.clone(),
        );
        application.base_pending_approval_digest = Some(digest);

        let action = authority_application_action(&runtime, &application)
            .map_err(|error| format!("matching authority failed validation: {error:?}"))?;
        let AuthorityAction::Restore(proposed) = action else {
            return Err(
                "an already-applied approved spec must still remove its pending hold".to_owned(),
            );
        };
        assert_eq!(proposed.spec, application.proposed_spec);
        Ok(())
    }

    #[tokio::test]
    async fn pending_marker_restoration_uses_the_controller_identity() -> Result<(), String> {
        let mut runtime = fixture();
        runtime.metadata.annotations.get_or_insert_default().insert(
            "agents.apelogic.ai/pending-approval".to_owned(),
            "request-digest".to_owned(),
        );
        let serialized_runtime = serde_json::to_vec(&runtime)
            .map_err(|error| format!("fixture runtime must be serializable: {error}"))?;
        let impersonated = Arc::new(AtomicBool::new(false));
        let impersonated_for_service = impersonated.clone();
        let client = Client::new(
            service_fn(move |request: Request<KubeBody>| {
                let serialized_runtime = serialized_runtime.clone();
                if request.headers().contains_key("impersonate-user") {
                    impersonated_for_service.store(true, Ordering::SeqCst);
                }
                async move {
                    let mut response = Response::new(Body::from(serialized_runtime));
                    *response.status_mut() = StatusCode::OK;
                    Ok::<_, Infallible>(response)
                }
            }),
            "team-a",
        );

        replace_as_authority(&client, &runtime, "alice@example.com", "engineer")
            .await
            .map_err(|error| format!("pending restoration must be writable: {error:?}"))?;

        assert!(
            !impersonated.load(Ordering::SeqCst),
            "a pending marker must be restored by the trusted controller identity"
        );
        Ok(())
    }

    #[test]
    fn approved_grant_cannot_release_a_placeholder_with_a_mismatched_request_digest() {
        let mut runtime = fixture();
        runtime.metadata.annotations.get_or_insert_default().insert(
            "agents.apelogic.ai/pending-approval".to_owned(),
            "different-request-digest".to_owned(),
        );
        let application = grant_reversion(&runtime);
        let application = GrantReversion {
            base_pending_approval_digest: Some("different-request-digest".to_owned()),
            ..application
        };

        assert!(
            authority_application_action(&runtime, &application).is_err(),
            "controller convergence must bind the pending marker to the approved proposed spec"
        );
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
    use std::collections::{BTreeMap, BTreeSet};

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use kube::core::admission::{AdmissionRequest, AdmissionReview};
    use steward_admission::{AdmissionDelta, Envelope, EnvelopeScopeKind, EnvelopeSpec};
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
            _scope_kind: EnvelopeScopeKind,
            _scope_ref: &'a str,
        ) -> WebhookFuture<'a, Result<Option<Envelope>, StoreError>> {
            Box::pin(async move { Ok(Some(self.envelope.clone())) })
        }

        fn grants_for_runtime<'a>(
            &'a self,
            runtime_uid: &'a str,
            _scope_kind: EnvelopeScopeKind,
            _scope_ref: &'a str,
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
                    runner: steward_types::RunnerRequirements::default(),
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
    async fn webhook_allows_only_a_trusted_writer_to_create_a_service_principal()
    -> Result<(), String> {
        let controller_username = "system:serviceaccount:steward-system:steward-controller";
        let mut value = admission_review_value();
        value["request"]["operation"] = serde_json::json!("CREATE");
        value["request"]["oldObject"] = serde_json::Value::Null;
        value["request"]["object"]["spec"]["budget"]["monthlyLimit"] = serde_json::json!("100.00");
        value["request"]["object"]["spec"]["principal"] = serde_json::json!({
            "kind": "service",
            "name": "scheduled-scanner"
        });
        value["request"]["object"]["spec"]["owner"] = serde_json::json!("alice@example.com");
        value["request"]["object"]["spec"]["canonicalAuthority"] = serde_json::json!({
            "schemaVersion": "steward/canonical-authority-binding/v1",
            "ownerUserId": "usr_0123456789abcdef0123456789abcdef"
        });
        value["request"]["object"]["metadata"]["annotations"] = serde_json::json!({
            "agents.apelogic.ai/service-principal": "scheduled-scanner"
        });

        let ordinary_review =
            serde_json::from_value::<AdmissionReview<AgentRuntime>>(value.clone())
                .map_err(|error| format!("failed to construct service CREATE review: {error}"))?;
        let ordinary_request: AdmissionRequest<AgentRuntime> = ordinary_review
            .try_into()
            .map_err(|error| format!("failed to read service CREATE request: {error}"))?;
        let ordinary = validate_admission(&ordinary_request, &fake_envelopes()).await;
        assert!(
            !ordinary.allowed,
            "an ordinary user must not self-assert canonical runtime authority"
        );
        assert_eq!(
            ordinary.result.message,
            "canonical runtime authority may be set only by a trusted Steward writer"
        );

        value["request"]["userInfo"] = serde_json::json!({"username": controller_username});
        let trusted_review = serde_json::from_value::<AdmissionReview<AgentRuntime>>(value)
            .map_err(|error| format!("failed to construct trusted service CREATE: {error}"))?;
        let trusted_request: AdmissionRequest<AgentRuntime> = trusted_review
            .try_into()
            .map_err(|error| format!("failed to read trusted service CREATE: {error}"))?;
        let trusted = super::validate_admission_with_trusted_writers(
            &trusted_request,
            &fake_envelopes(),
            &BTreeSet::from([controller_username.to_owned()]),
        )
        .await;
        assert!(
            trusted.allowed,
            "the trusted Steward writer must admit a service runtime through its service envelope: {}",
            trusted.result.message
        );
        Ok(())
    }

    #[tokio::test]
    async fn webhook_allows_a_trusted_writer_to_author_a_canonical_user_runtime()
    -> Result<(), String> {
        let writer_username = "system:serviceaccount:steward-system:steward-poc-api";
        let mut value = admission_review_value();
        value["request"]["operation"] = serde_json::json!("CREATE");
        value["request"]["oldObject"] = serde_json::Value::Null;
        value["request"]["userInfo"] = serde_json::json!({"username": writer_username});
        value["request"]["object"]["spec"]["budget"]["monthlyLimit"] = serde_json::json!("100.00");
        value["request"]["object"]["spec"]["canonicalAuthority"] = serde_json::json!({
            "schemaVersion": "steward/canonical-authority-binding/v1",
            "ownerUserId": "usr_0123456789abcdef0123456789abcdef",
            "actingUserId": "usr_0123456789abcdef0123456789abcdef"
        });

        let review = serde_json::from_value::<AdmissionReview<AgentRuntime>>(value)
            .map_err(|error| format!("failed to construct trusted user CREATE review: {error}"))?;
        let request: AdmissionRequest<AgentRuntime> = review
            .try_into()
            .map_err(|error| format!("failed to read trusted user CREATE request: {error}"))?;
        let response = super::validate_admission_with_trusted_writers(
            &request,
            &fake_envelopes(),
            &BTreeSet::from([writer_username.to_owned()]),
        )
        .await;
        assert!(
            response.allowed,
            "the trusted API writer must be able to author the server-derived canonical user binding: {}",
            response.result.message
        );
        Ok(())
    }

    #[tokio::test]
    async fn webhook_rejects_canonical_authority_mutation_even_by_a_trusted_writer()
    -> Result<(), String> {
        let controller_username = "system:serviceaccount:steward-system:steward-controller";
        let mut value = admission_review_value();
        value["request"]["userInfo"] = serde_json::json!({"username": controller_username});
        value["request"]["object"]["spec"]["canonicalAuthority"] = serde_json::json!({
            "schemaVersion": "steward/canonical-authority-binding/v1",
            "ownerUserId": "usr_0123456789abcdef0123456789abcdef",
            "actingUserId": "usr_0123456789abcdef0123456789abcdef"
        });
        let review = serde_json::from_value::<AdmissionReview<AgentRuntime>>(value)
            .map_err(|error| format!("failed to construct authority mutation review: {error}"))?;
        let request: AdmissionRequest<AgentRuntime> = review
            .try_into()
            .map_err(|error| format!("failed to read authority mutation request: {error}"))?;

        let response = super::validate_admission_with_trusted_writers(
            &request,
            &fake_envelopes(),
            &BTreeSet::from([controller_username.to_owned()]),
        )
        .await;

        assert!(
            !response.allowed,
            "canonical authority mutation was admitted"
        );
        assert_eq!(
            response.result.message,
            "canonical runtime authority is immutable"
        );
        Ok(())
    }

    #[tokio::test]
    async fn webhook_rejects_a_service_principal_annotation_mismatch() -> Result<(), String> {
        let controller_username = "system:serviceaccount:steward-system:steward-controller";
        let mut value = admission_review_value();
        value["request"]["operation"] = serde_json::json!("CREATE");
        value["request"]["oldObject"] = serde_json::Value::Null;
        value["request"]["userInfo"] = serde_json::json!({"username": controller_username});
        value["request"]["object"]["spec"]["budget"]["monthlyLimit"] = serde_json::json!("100.00");
        value["request"]["object"]["spec"]["principal"] = serde_json::json!({
            "kind": "service",
            "name": "scheduled-scanner"
        });
        value["request"]["object"]["metadata"]["annotations"] = serde_json::json!({
            "agents.apelogic.ai/service-principal": "different-service"
        });
        let review = serde_json::from_value::<AdmissionReview<AgentRuntime>>(value)
            .map_err(|error| format!("failed to construct mismatched service review: {error}"))?;
        let request: AdmissionRequest<AgentRuntime> = review
            .try_into()
            .map_err(|error| format!("failed to read mismatched service review: {error}"))?;

        let response = super::validate_admission_with_trusted_writers(
            &request,
            &fake_envelopes(),
            &BTreeSet::from([controller_username.to_owned()]),
        )
        .await;
        assert!(
            !response.allowed,
            "a service name cannot cross envelope scopes"
        );
        assert_eq!(
            response.result.message,
            "AgentRuntime service-principal annotation must match the service principal name"
        );
        Ok(())
    }

    #[tokio::test]
    async fn webhook_rejects_user_added_pending_approval_marker() -> Result<(), String> {
        let mut value = admission_review_value();
        value["request"]["object"]["spec"]["budget"]["monthlyLimit"] = serde_json::json!("100.00");
        value["request"]["oldObject"]["spec"]["budget"]["monthlyLimit"] =
            serde_json::json!("100.00");
        value["request"]["object"]["metadata"]["annotations"]["agents.apelogic.ai/pending-approval"] =
            serde_json::json!("forged-request-digest");
        let review =
            serde_json::from_value::<AdmissionReview<AgentRuntime>>(value).map_err(|error| {
                format!("failed to construct forged pending-marker review: {error}")
            })?;
        let request: AdmissionRequest<AgentRuntime> = review
            .try_into()
            .map_err(|error| format!("failed to read forged pending-marker request: {error}"))?;

        let response = validate_admission(&request, &fake_envelopes()).await;

        assert!(
            !response.allowed,
            "a runtime writer must not be able to place a live runtime into controller-owned pending state"
        );
        assert_eq!(
            response.result.message,
            "agents.apelogic.ai/pending-approval cannot be added or changed on UPDATE"
        );
        Ok(())
    }

    #[tokio::test]
    async fn webhook_rejects_user_spec_patch_while_pending_marker_is_unchanged()
    -> Result<(), String> {
        let mut value = admission_review_value();
        value["request"]["oldObject"]["spec"]["budget"]["monthlyLimit"] = serde_json::json!("0");
        value["request"]["object"]["spec"]["budget"]["monthlyLimit"] = serde_json::json!("100.00");
        value["request"]["oldObject"]["metadata"]["annotations"]["agents.apelogic.ai/pending-approval"] =
            serde_json::json!("request-digest");
        value["request"]["object"]["metadata"]["annotations"]["agents.apelogic.ai/pending-approval"] =
            serde_json::json!("request-digest");
        let review = serde_json::from_value::<AdmissionReview<AgentRuntime>>(value)
            .map_err(|error| format!("failed to construct held spec PATCH review: {error}"))?;
        let request: AdmissionRequest<AgentRuntime> = review
            .try_into()
            .map_err(|error| format!("failed to read held spec PATCH request: {error}"))?;

        let response = validate_admission(&request, &fake_envelopes()).await;

        assert!(
            !response.allowed,
            "an ordinary user must not drift a held anchor"
        );
        assert_eq!(
            response.result.message,
            "pending AgentRuntime spec may be changed only by a trusted Steward writer"
        );
        Ok(())
    }

    #[tokio::test]
    async fn webhook_rejects_user_created_pending_approval_marker() -> Result<(), String> {
        let mut value = admission_review_value();
        value["request"]["operation"] = serde_json::json!("CREATE");
        value["request"]["oldObject"] = serde_json::Value::Null;
        value["request"]["object"]["spec"]["budget"]["monthlyLimit"] = serde_json::json!("100.00");
        value["request"]["object"]["metadata"]["annotations"]["agents.apelogic.ai/pending-approval"] =
            serde_json::json!("forged-request-digest");
        let review =
            serde_json::from_value::<AdmissionReview<AgentRuntime>>(value).map_err(|error| {
                format!("failed to construct forged pending CREATE review: {error}")
            })?;
        let request: AdmissionRequest<AgentRuntime> = review
            .try_into()
            .map_err(|error| format!("failed to read forged pending CREATE request: {error}"))?;

        let response = validate_admission(&request, &fake_envelopes()).await;

        assert!(
            !response.allowed,
            "an ordinary runtime writer must not create a controller-owned pending marker"
        );
        assert_eq!(
            response.result.message,
            "agents.apelogic.ai/pending-approval may be set only by a trusted Steward writer"
        );
        Ok(())
    }

    #[tokio::test]
    async fn webhook_rejects_user_removed_pending_approval_marker() -> Result<(), String> {
        let mut value = admission_review_value();
        value["request"]["object"]["spec"]["budget"]["monthlyLimit"] = serde_json::json!("100.00");
        value["request"]["oldObject"]["spec"]["budget"]["monthlyLimit"] =
            serde_json::json!("100.00");
        value["request"]["oldObject"]["metadata"]["annotations"]["agents.apelogic.ai/pending-approval"] =
            serde_json::json!("request-digest");
        let review =
            serde_json::from_value::<AdmissionReview<AgentRuntime>>(value).map_err(|error| {
                format!("failed to construct removed pending-marker review: {error}")
            })?;
        let request: AdmissionRequest<AgentRuntime> = review
            .try_into()
            .map_err(|error| format!("failed to read removed pending-marker request: {error}"))?;

        let response = validate_admission(&request, &fake_envelopes()).await;

        assert!(
            !response.allowed,
            "a runtime writer must not be able to release an unapproved placeholder"
        );
        assert_eq!(
            response.result.message,
            "agents.apelogic.ai/pending-approval may be removed only by a trusted Steward writer"
        );
        Ok(())
    }

    #[tokio::test]
    async fn webhook_rejects_user_deletion_of_pending_approval_placeholder() -> Result<(), String> {
        let mut value = admission_review_value();
        value["request"]["operation"] = serde_json::json!("DELETE");
        value["request"]["object"] = serde_json::Value::Null;
        value["request"]["oldObject"]["metadata"]["annotations"]["agents.apelogic.ai/pending-approval"] =
            serde_json::json!("request-digest");
        let review = serde_json::from_value::<AdmissionReview<AgentRuntime>>(value)
            .map_err(|error| format!("failed to construct pending DELETE review: {error}"))?;
        let request: AdmissionRequest<AgentRuntime> = review
            .try_into()
            .map_err(|error| format!("failed to read pending DELETE request: {error}"))?;

        let response = validate_admission(&request, &fake_envelopes()).await;

        assert!(
            !response.allowed,
            "an ordinary user must not delete a durable pending-approval anchor"
        );
        assert_eq!(
            response.result.message,
            "pending AgentRuntime deletion requires a trusted Steward writer"
        );
        Ok(())
    }

    #[tokio::test]
    async fn webhook_fails_closed_when_delete_has_no_old_object() -> Result<(), String> {
        let mut value = admission_review_value();
        value["request"]["operation"] = serde_json::json!("DELETE");
        value["request"]["object"] = serde_json::Value::Null;
        value["request"]["oldObject"] = serde_json::Value::Null;
        let review = serde_json::from_value::<AdmissionReview<AgentRuntime>>(value)
            .map_err(|error| format!("failed to construct incomplete DELETE review: {error}"))?;
        let request: AdmissionRequest<AgentRuntime> = review
            .try_into()
            .map_err(|error| format!("failed to read incomplete DELETE request: {error}"))?;

        let response = validate_admission(&request, &fake_envelopes()).await;

        assert!(
            !response.allowed,
            "DELETE without oldObject must fail closed"
        );
        assert_eq!(
            response.result.message,
            "AgentRuntime DELETE admission request has no old object"
        );
        Ok(())
    }

    #[tokio::test]
    async fn webhook_fails_closed_when_pending_delete_has_no_username() -> Result<(), String> {
        let mut value = admission_review_value();
        value["request"]["operation"] = serde_json::json!("DELETE");
        value["request"]["object"] = serde_json::Value::Null;
        value["request"]["userInfo"] = serde_json::json!({});
        value["request"]["oldObject"]["metadata"]["annotations"]["agents.apelogic.ai/pending-approval"] =
            serde_json::json!("request-digest");
        let review =
            serde_json::from_value::<AdmissionReview<AgentRuntime>>(value).map_err(|error| {
                format!("failed to construct unauthenticated DELETE review: {error}")
            })?;
        let request: AdmissionRequest<AgentRuntime> = review
            .try_into()
            .map_err(|error| format!("failed to read unauthenticated DELETE request: {error}"))?;

        let response = validate_admission(&request, &fake_envelopes()).await;

        assert!(
            !response.allowed,
            "pending DELETE without a username must fail closed"
        );
        assert_eq!(
            response.result.message,
            "authenticated Kubernetes username is required to delete a pending AgentRuntime"
        );
        Ok(())
    }

    #[tokio::test]
    async fn webhook_allows_trusted_writer_to_delete_pending_placeholder() -> Result<(), String> {
        let controller_username = "system:serviceaccount:steward-system:steward-controller";
        let mut value = admission_review_value();
        value["request"]["operation"] = serde_json::json!("DELETE");
        value["request"]["object"] = serde_json::Value::Null;
        value["request"]["userInfo"] = serde_json::json!({"username": controller_username});
        value["request"]["oldObject"]["metadata"]["annotations"]["agents.apelogic.ai/pending-approval"] =
            serde_json::json!("request-digest");
        let review = serde_json::from_value::<AdmissionReview<AgentRuntime>>(value)
            .map_err(|error| format!("failed to construct trusted DELETE review: {error}"))?;
        let request: AdmissionRequest<AgentRuntime> = review
            .try_into()
            .map_err(|error| format!("failed to read trusted DELETE request: {error}"))?;

        let response = super::validate_admission_with_trusted_writers(
            &request,
            &fake_envelopes(),
            &BTreeSet::from([controller_username.to_owned()]),
        )
        .await;

        assert!(
            response.allowed,
            "the configured trusted writer must retain a controlled cleanup path: {}",
            response.result.message
        );
        Ok(())
    }

    #[tokio::test]
    async fn webhook_preserves_normal_deletion_for_non_pending_runtime() -> Result<(), String> {
        let mut value = admission_review_value();
        value["request"]["operation"] = serde_json::json!("DELETE");
        value["request"]["object"] = serde_json::Value::Null;
        let review = serde_json::from_value::<AdmissionReview<AgentRuntime>>(value)
            .map_err(|error| format!("failed to construct ordinary DELETE review: {error}"))?;
        let request: AdmissionRequest<AgentRuntime> = review
            .try_into()
            .map_err(|error| format!("failed to read ordinary DELETE request: {error}"))?;

        let response = validate_admission(&request, &fake_envelopes()).await;

        assert!(
            response.allowed,
            "ordinary runtime deletion must remain allowed"
        );
        Ok(())
    }

    #[tokio::test]
    async fn webhook_allows_its_controller_to_apply_an_approved_pending_runtime()
    -> Result<(), String> {
        let mut value = admission_review_value();
        value["request"]["oldObject"]["metadata"]["annotations"]["agents.apelogic.ai/pending-approval"] =
            serde_json::json!("request-digest");
        value["request"]["oldObject"]["spec"]["llms"] = serde_json::json!([]);
        value["request"]["oldObject"]["spec"]["budget"]["monthlyLimit"] = serde_json::json!("0");
        value["request"]["userInfo"] = serde_json::json!({
            "username": "system:serviceaccount:steward-system:steward-controller",
            "groups": ["system:serviceaccounts"]
        });
        let review = serde_json::from_value::<AdmissionReview<AgentRuntime>>(value)
            .map_err(|error| format!("failed to construct trusted pending transition: {error}"))?;
        let request: AdmissionRequest<AgentRuntime> = review
            .try_into()
            .map_err(|error| format!("failed to read trusted pending transition: {error}"))?;
        let mut envelopes = fake_envelopes();
        envelopes.grants.insert(
            "runtime-uid-a".to_owned(),
            vec![AdmissionDelta::Budget {
                requested: "220.00".to_owned(),
                ceiling: "200.00".to_owned(),
                currency: "USD".to_owned(),
            }],
        );

        let response = super::validate_admission_for_controller(
            &request,
            &envelopes,
            "system:serviceaccount:steward-system:steward-controller",
        )
        .await;

        assert!(
            response.allowed,
            "the trusted controller must be able to apply the active approved spec: {}",
            response.result.message
        );
        Ok(())
    }

    #[tokio::test]
    async fn webhook_allows_trusted_writer_to_restore_pending_marker() -> Result<(), String> {
        let controller_username = "system:serviceaccount:steward-system:steward-controller";
        let mut value = admission_review_value();
        value["request"]["object"]["spec"]["budget"]["monthlyLimit"] = serde_json::json!("100.00");
        value["request"]["oldObject"]["spec"]["budget"]["monthlyLimit"] =
            serde_json::json!("220.00");
        value["request"]["object"]["metadata"]["annotations"]["agents.apelogic.ai/pending-approval"] =
            serde_json::json!("request-digest");
        value["request"]["userInfo"] = serde_json::json!({
            "username": controller_username,
            "groups": ["system:serviceaccounts"]
        });
        let review = serde_json::from_value::<AdmissionReview<AgentRuntime>>(value)
            .map_err(|error| format!("failed to construct trusted hold restoration: {error}"))?;
        let request: AdmissionRequest<AgentRuntime> = review
            .try_into()
            .map_err(|error| format!("failed to read trusted hold restoration: {error}"))?;

        let response = super::validate_admission_with_trusted_writers(
            &request,
            &fake_envelopes(),
            &BTreeSet::from([controller_username.to_owned()]),
        )
        .await;

        assert!(
            response.allowed,
            "the authority writer must be able to restore a revoked initial-create hold: {}",
            response.result.message
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
