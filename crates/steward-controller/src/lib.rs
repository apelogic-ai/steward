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
use steward_admission::internal_authorities::steward_connections_v1;
use steward_admission::{
    AdmissionDecision, AdmissionDelta, Envelope, EnvelopeScopeKind, budget_is_exhausted,
    duration_seconds, evaluate, evaluate_with_grants,
};
use steward_ports::{
    DecisionChannel, DecisionRequest, InferenceCapabilities, InferenceCredential,
    InferenceObservation, InferencePlane, InferenceRequest, MAX_TASK_OUTPUT_ARCHIVE_BYTES,
    ProvisionedInference, SandboxExecutionClass, SandboxTaskObservation, SandboxTaskRequest,
    SandboxTaskRuntime, TaskAttemptId,
};
pub use steward_ports::{PortError, SandboxObservation, SandboxRequest, SandboxRuntime};
use steward_store::{
    ApprovalDeliveryTransition, ConnectionOperationKind, ConnectionOperationRecord, GrantReversion,
    PgStore, StoreError, TaskActivationObservation, TaskCleanupCause, TaskCleanupObservation,
    TaskExecutionAttemptState, TaskExecutionObservation, TaskExecutionTransition,
    TaskOrchestrationMode, TaskOrchestrationState, TaskOrchestrationWorkItem, TaskRecord,
    TaskRuntimeOwnership,
};
#[cfg(test)]
use steward_types::RuntimeOwnership;
use steward_types::{
    AgentRuntime, AgentRuntimeSpec, AgentRuntimeStatus, DisposableExecutionBinding, Duration,
    PENDING_APPROVAL_ANNOTATION, Phase, RuntimeId, RuntimeRefs, TASK_EXECUTION_BINDING_ANNOTATION,
    TaskExecutionBinding, TaskPhase, runtime_activated_condition,
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
#[cfg(test)]
enum TaskRuntimeAction {
    Wait,
    CreateRuntime,
    Release,
    Execute,
    DeleteRuntime,
    MarkFinalized,
}

#[cfg(test)]
fn task_runtime_action(
    phase: TaskPhase,
    ownership: RuntimeOwnership,
    execution_binding: Option<&TaskExecutionBinding>,
    finalize_requested: bool,
    runtime_is_bound: bool,
    runtime_spec: &AgentRuntimeSpec,
    runtime: Option<&AgentRuntime>,
) -> TaskRuntimeAction {
    let task_owns_runtime = execution_binding
        .map(TaskExecutionBinding::task_owns_runtime)
        .unwrap_or(ownership == RuntimeOwnership::Provisioned);
    if finalize_requested {
        return match (task_owns_runtime, runtime) {
            (false, _) | (true, None) => TaskRuntimeAction::MarkFinalized,
            (true, Some(runtime))
                if !runtime_is_bound
                    || task_runtime_matches_cleanup_authority(runtime_spec, runtime) =>
            {
                TaskRuntimeAction::DeleteRuntime
            }
            (true, Some(_)) => TaskRuntimeAction::Wait,
        };
    }
    let Some(runtime) = runtime else {
        return if task_owns_runtime && matches!(phase, TaskPhase::Submitted | TaskPhase::Queued) {
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

#[cfg(test)]
fn task_runtime_matches_cleanup_authority(
    task_spec: &AgentRuntimeSpec,
    runtime: &AgentRuntime,
) -> bool {
    runtime.spec == *task_spec
        || spec_digest(task_spec).is_ok_and(|digest| {
            runtime.annotations().get(PENDING_APPROVAL_ANNOTATION) == Some(&digest)
        })
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
    let intent = if intent == ReconcileIntent::Ensure && is_pending_approval(runtime) {
        if runtime_has_provisioned_authority(runtime) {
            ReconcileIntent::Delete
        } else {
            return Ok(ReconcileDecision::Status(pending_approval_status(runtime)?));
        }
    } else {
        intent
    };
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
    let execution_binding = if intent == ReconcileIntent::Ensure {
        runtime
            .annotations()
            .get(TASK_EXECUTION_BINDING_ANNOTATION)
            .map(|serialized| {
                let binding = serde_json::from_str::<DisposableExecutionBinding>(serialized)
                    .map_err(|error| ReconcileError::InvalidSpec {
                        reason: format!("invalid task execution binding annotation: {error}"),
                    })?;
                binding
                    .validate()
                    .map_err(|reason| ReconcileError::InvalidSpec { reason })?;
                if binding.agent_ref != runtime.spec.agent_type.name {
                    return Err(ReconcileError::InvalidSpec {
                        reason: "task execution binding does not match runtime agent type"
                            .to_owned(),
                    });
                }
                Ok(binding)
            })
            .transpose()?
    } else {
        None
    };
    let request = SandboxRequest {
        runtime: runtime_id,
        workspace_key,
        execution_class: sandbox_execution_class(&runtime.spec),
        agent_type: runtime.spec.agent_type.clone(),
        models: runtime.spec.llms.clone(),
        tools: runtime.spec.tools.clone(),
        refs: runtime
            .status
            .as_ref()
            .map(|status| status.refs.clone())
            .unwrap_or_default(),
        execution_binding,
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

fn runtime_has_provisioned_authority(runtime: &AgentRuntime) -> bool {
    runtime.status.as_ref().is_some_and(|status| {
        status.refs.workspace.is_some()
            || status.refs.sandbox.is_some()
            || status.refs.litellm_key.is_some()
    })
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
    task_orchestration_mode: TaskOrchestrationMode,
) {
    if task_orchestration_mode.is_active() {
        let task_controller =
            run_task_controller(client.clone(), sandbox_runtime.clone(), authority.clone());
        let runtime_controller =
            run_controller_inner(client, sandbox_runtime, inference, Some(authority));
        tokio::select! {
            () = task_controller => eprintln!("task controller exited"),
            () = runtime_controller => eprintln!("runtime controller exited"),
        }
    } else {
        run_controller_inner(client, sandbox_runtime, inference, Some(authority)).await;
    }
}

async fn run_task_controller<R: SandboxTaskRuntime>(
    client: Client,
    sandbox_runtime: R,
    authority: PgStore,
) {
    loop {
        match authority.task_orchestration_work_items().await {
            Ok(work) => {
                for item in work {
                    if let Err(error) = reconcile_task_orchestration_work_item(
                        &client,
                        &sandbox_runtime,
                        &authority,
                        &item,
                    )
                    .await
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

/// Reconciles one immutable Task operation by at most one durable lifecycle step.
///
/// This bounded entry point is shared by the production loop and fault-injection tests. Safety
/// does not depend on one caller: immutable external identities and store generations arbitrate
/// concurrent invocations.
pub async fn reconcile_task_orchestration_work_item<R: SandboxTaskRuntime>(
    client: &Client,
    sandbox_runtime: &R,
    authority: &PgStore,
    work: &TaskOrchestrationWorkItem,
) -> Result<(), TaskControllerError> {
    reconcile_task_operation(client, sandbox_runtime, authority, work).await
}

const TASK_UID_ANNOTATION: &str = "agents.apelogic.ai/task-uid";
const TASK_OPERATION_ANNOTATION: &str = "agents.apelogic.ai/orchestration-id";
const TASK_MANIFEST_DIGEST_ANNOTATION: &str = "agents.apelogic.ai/manifest-digest";
const TASK_RUNTIME_MODE_ANNOTATION: &str = "agents.apelogic.ai/runtime-mode";
const TASK_ORCHESTRATOR_ACTOR: &str = "task-orchestrator";

#[derive(Debug)]
pub enum ApprovalDispatcherError {
    Store(StoreError),
    InvalidApproval(String),
    Delivery(steward_ports::PortError),
}

impl fmt::Display for ApprovalDispatcherError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "approval outbox store failed: {error}"),
            Self::InvalidApproval(reason) => {
                write!(formatter, "approval outbox is invalid: {reason}")
            }
            Self::Delivery(error) => write!(formatter, "approval delivery failed: {error:?}"),
        }
    }
}

impl Error for ApprovalDispatcherError {}

pub async fn dispatch_one_task_approval<D: DecisionChannel>(
    authority: &PgStore,
    decisions: &D,
    worker: &str,
) -> Result<bool, ApprovalDispatcherError> {
    let Some(mut work) = authority
        .claim_approval_delivery(worker, 30)
        .await
        .map_err(ApprovalDispatcherError::Store)?
    else {
        return Ok(false);
    };
    let counterexample = AdmissionDecision::Reject {
        deltas: work.deltas.clone(),
    }
    .counterexample()
    .ok_or_else(|| {
        ApprovalDispatcherError::InvalidApproval(
            "runtime-bound approval has no rejection counterexample".to_owned(),
        )
    })?;
    let request = DecisionRequest {
        request_id: work.approval_id.to_string(),
        runtime_uid: work.runtime_uid.clone(),
        actor: work.actor.clone(),
        member_role: work.member_role.clone(),
        counterexample,
    };
    let observation = if work.delivery_invoked {
        decisions.observe_request(&request.request_id).await
    } else {
        let Some(generation) = authority
            .authorize_approval_delivery_invocation(&work, worker)
            .await
            .map_err(ApprovalDispatcherError::Store)?
        else {
            authority
                .retry_approval_delivery(
                    work.effect_id,
                    work.generation,
                    worker,
                    "delivery_not_authorized",
                )
                .await
                .map_err(ApprovalDispatcherError::Store)?;
            return Ok(false);
        };
        work.generation = generation;
        decisions.request(&request).await.map(Some)
    };
    let reference = match observation {
        Ok(Some(reference)) => reference,
        Ok(None) => {
            authority
                .retry_approval_delivery(
                    work.effect_id,
                    work.generation,
                    worker,
                    "delivery_outcome_unobserved",
                )
                .await
                .map_err(ApprovalDispatcherError::Store)?;
            return Ok(false);
        }
        Err(error) => {
            authority
                .retry_approval_delivery(
                    work.effect_id,
                    work.generation,
                    worker,
                    "decision_channel_unavailable",
                )
                .await
                .map_err(ApprovalDispatcherError::Store)?;
            return Err(ApprovalDispatcherError::Delivery(error));
        }
    };
    match authority
        .complete_approval_delivery(
            work.effect_id,
            work.generation,
            worker,
            &reference.key,
            &reference.evidence_url,
        )
        .await
        .map_err(ApprovalDispatcherError::Store)?
    {
        ApprovalDeliveryTransition::Applied
        | ApprovalDeliveryTransition::AlreadyApplied
        | ApprovalDeliveryTransition::Superseded => Ok(true),
    }
}

/// Delivers durable Task approval outbox rows through the configured decision channel.
///
/// A successful delivery is immediately followed by another claim so a backlog drains without a
/// fixed per-item delay. Empty queues and failures back off. The immutable invocation fence,
/// not the scheduling lease, prevents successors from repeating an ambiguous create.
pub async fn run_task_approval_dispatcher<D: DecisionChannel>(authority: PgStore, decisions: D) {
    const WORKER: &str = "task-approval-dispatcher";
    loop {
        let should_back_off = match dispatch_one_task_approval(&authority, &decisions, WORKER).await
        {
            Ok(delivered) => !delivered,
            Err(error) => {
                eprintln!("task approval delivery failed: {error}");
                true
            }
        };
        if should_back_off {
            tokio::time::sleep(StdDuration::from_secs(1)).await;
        }
    }
}

async fn reconcile_task_operation<R: SandboxTaskRuntime>(
    client: &Client,
    sandbox_runtime: &R,
    authority: &PgStore,
    work: &TaskOrchestrationWorkItem,
) -> Result<(), TaskControllerError> {
    let task = &work.task;
    let operation = &work.operation;
    if operation.state == TaskOrchestrationState::Finalized {
        return reconcile_quarantined_execution(client, sandbox_runtime, authority, work).await;
    }
    if (task.cancel_requested
        || task.finalize_requested
        || operation.state == TaskOrchestrationState::CleanupPending)
        && let Some(attempt) = authority
            .task_execution_attempt(task.task_uid)
            .await
            .map_err(TaskControllerError::Store)?
        && !attempt.state.is_terminal()
        && attempt.start_invoked_at.is_some()
    {
        reconcile_task_cancellation(client, sandbox_runtime, authority, task, attempt).await?;
        return Ok(());
    }
    if (task.cancel_requested || task.finalize_requested)
        && operation.state != TaskOrchestrationState::CleanupPending
    {
        let cause = if task.cancel_requested {
            TaskCleanupCause::Cancelled
        } else {
            TaskCleanupCause::FinalizationRequested
        };
        authority
            .enter_task_cleanup(
                task.task_uid,
                operation.generation,
                cause,
                TASK_ORCHESTRATOR_ACTOR,
            )
            .await
            .map_err(TaskControllerError::Store)?;
        return Ok(());
    }
    match operation.state {
        TaskOrchestrationState::IntentRecorded => {
            if operation.runtime_ownership != TaskRuntimeOwnership::Provisioned {
                return reconcile_shared_runtime_observation(client, authority, work).await;
            }
            authority
                .authorize_task_runtime_creation(
                    task.task_uid,
                    operation.generation,
                    TASK_ORCHESTRATOR_ACTOR,
                )
                .await
                .map(|_| ())
                .map_err(TaskControllerError::Store)
        }
        TaskOrchestrationState::RuntimeCreatePending => {
            reconcile_runtime_creation(client, authority, work).await
        }
        TaskOrchestrationState::RuntimeObserved => {
            let (envelope, envelope_digest) = task_authority_snapshot(authority, task).await?;
            authority
                .decide_task_runtime_authority(
                    task.task_uid,
                    operation.generation,
                    &envelope,
                    &envelope_digest,
                    TASK_ORCHESTRATOR_ACTOR,
                )
                .await
                .map(|_| ())
                .map_err(TaskControllerError::Store)
        }
        TaskOrchestrationState::ApprovalPending => authority
            .authorize_task_activation_from_approval(
                task.task_uid,
                operation.generation,
                TASK_ORCHESTRATOR_ACTOR,
            )
            .await
            .map(|_| ())
            .map_err(TaskControllerError::Store),
        TaskOrchestrationState::ActivationPending => {
            let (envelope, envelope_digest) = task_authority_snapshot(authority, task).await?;
            match authority
                .decide_task_runtime_authority(
                    task.task_uid,
                    operation.generation,
                    &envelope,
                    &envelope_digest,
                    TASK_ORCHESTRATOR_ACTOR,
                )
                .await
                .map_err(TaskControllerError::Store)?
            {
                steward_store::TaskOperationTransition::AlreadyApplied(current)
                    if current.activation_effect_authorized_at.is_some() =>
                {
                    reconcile_runtime_activation(client, authority, work).await
                }
                steward_store::TaskOperationTransition::Applied(_)
                | steward_store::TaskOperationTransition::AlreadyApplied(_)
                | steward_store::TaskOperationTransition::Superseded(_)
                | steward_store::TaskOperationTransition::AuthorityInactive { .. }
                | steward_store::TaskOperationTransition::InvariantViolation { .. } => Ok(()),
            }
        }
        TaskOrchestrationState::Active => {
            match authority
                .revalidate_active_task_authority(
                    task.task_uid,
                    operation.generation,
                    TASK_ORCHESTRATOR_ACTOR,
                )
                .await
                .map_err(TaskControllerError::Store)?
            {
                steward_store::TaskOperationTransition::AlreadyApplied(_) => {}
                steward_store::TaskOperationTransition::Applied(_)
                | steward_store::TaskOperationTransition::Superseded(_)
                | steward_store::TaskOperationTransition::AuthorityInactive { .. }
                | steward_store::TaskOperationTransition::InvariantViolation { .. } => {
                    return Ok(());
                }
            }
            if task.execute_requested
                && matches!(task.phase, TaskPhase::Queued | TaskPhase::Running)
            {
                reconcile_task_execution(client, sandbox_runtime, authority, work).await
            } else {
                Ok(())
            }
        }
        TaskOrchestrationState::CleanupPending => {
            reconcile_runtime_cleanup(client, authority, work).await
        }
        TaskOrchestrationState::Finalized => Ok(()),
    }
}

/// Finalization retires Task-owned effects, not a possibly live shared execution.
/// Only late terminal evidence for the exact attempt may release its quarantine.
async fn reconcile_quarantined_execution<R: SandboxTaskRuntime>(
    client: &Client,
    sandbox_runtime: &R,
    authority: &PgStore,
    work: &TaskOrchestrationWorkItem,
) -> Result<(), TaskControllerError> {
    let Some(attempt) = authority
        .task_execution_attempt(work.task.task_uid)
        .await
        .map_err(TaskControllerError::Store)?
    else {
        return Ok(());
    };
    if attempt.state != TaskExecutionAttemptState::OutcomeUnknown
        || !authority
            .task_execution_holds_runtime_lease(attempt.attempt_id)
            .await
            .map_err(TaskControllerError::Store)?
    {
        return Ok(());
    }
    let api = Api::<AgentRuntime>::namespaced(client.clone(), &work.operation.runtime_namespace);
    let Some(runtime) = api
        .get_opt(&work.operation.runtime_name)
        .await
        .map_err(TaskControllerError::Kubernetes)?
    else {
        return Ok(());
    };
    if runtime.metadata.uid.as_deref() != Some(attempt.runtime_uid.as_str()) {
        return Ok(());
    }
    let Some(refs) = runtime
        .status
        .as_ref()
        .map(|status| &status.refs)
        .filter(|refs| {
            refs.workspace
                .as_ref()
                .is_some_and(|reference| !reference.is_empty())
                && refs
                    .sandbox
                    .as_ref()
                    .is_some_and(|reference| !reference.is_empty())
        })
    else {
        return Ok(());
    };
    let request = sandbox_task_request(&work.task, attempt.runtime_uid.clone(), refs.clone());
    let observation = sandbox_runtime
        .observe_task(&TaskAttemptId(attempt.attempt_id.to_string()), &request)
        .await
        .map_err(TaskControllerError::Sandbox)?;
    if matches!(
        observation,
        SandboxTaskObservation::Succeeded { .. } | SandboxTaskObservation::Failed { .. }
    ) {
        persist_execution_observation(authority, &attempt, attempt.generation, observation).await?;
    }
    Ok(())
}

async fn task_authority_snapshot(
    authority: &PgStore,
    task: &TaskRecord,
) -> Result<(Envelope, String), TaskControllerError> {
    if task.internal_authority_id.as_deref() == Some(steward_connections_v1::SERVICE)
        && task.internal_authority_version == Some(steward_connections_v1::AUTHORITY_VERSION)
        && task.internal_authority_digest.as_deref()
            == Some(steward_connections_v1::AUTHORITY_DIGEST)
    {
        return Ok((
            steward_connections_v1::envelope(),
            task.service_envelope_digest.clone().ok_or_else(|| {
                TaskControllerError::InvalidState(
                    "internal Task has no authority-envelope digest".to_owned(),
                )
            })?,
        ));
    }
    let envelope = authority
        .latest_service_envelope(&task.submitter_service)
        .await
        .map_err(TaskControllerError::Store)?
        .ok_or_else(|| {
            TaskControllerError::InvalidState("Task service has no current Envelope".to_owned())
        })?;
    let digest = bytes_digest(&serde_json::to_vec(&envelope).map_err(|error| {
        TaskControllerError::InvalidState(format!(
            "current Task Envelope cannot be digested: {error}"
        ))
    })?);
    Ok((envelope, digest))
}

async fn reconcile_task_execution<R: SandboxTaskRuntime>(
    client: &Client,
    sandbox_runtime: &R,
    authority: &PgStore,
    work: &TaskOrchestrationWorkItem,
) -> Result<(), TaskControllerError> {
    let task = &work.task;
    if let Some(connection) = authority
        .connection_operation_for_task(task.task_uid)
        .await
        .map_err(TaskControllerError::Store)?
    {
        let current = sandbox_runtime.provider_control_bindings();
        if !connection_operation_bindings_match(&connection, task, current.as_ref()) {
            authority
                .fail_connection_operation(connection.operation_id, "binding_mismatch")
                .await
                .map_err(TaskControllerError::Store)?;
            authority
                .enter_task_cleanup(
                    task.task_uid,
                    work.operation.generation,
                    TaskCleanupCause::Failed("execution_binding_mismatch"),
                    TASK_ORCHESTRATOR_ACTOR,
                )
                .await
                .map_err(TaskControllerError::Store)?;
            return Ok(());
        }
    }
    if matches!(
        task.execution_binding,
        Some(TaskExecutionBinding::Resident(_))
    ) {
        return Err(TaskControllerError::InvalidState(
            "resident Task dispatch protocol is not implemented".to_owned(),
        ));
    }
    let runtime_uid = work.operation.runtime_uid.as_deref().ok_or_else(|| {
        TaskControllerError::InvalidState("active Task has no exact runtime UID".to_owned())
    })?;
    let api = Api::<AgentRuntime>::namespaced(client.clone(), &work.operation.runtime_namespace);
    let runtime = api
        .get_opt(&work.operation.runtime_name)
        .await
        .map_err(TaskControllerError::Kubernetes)?;
    let runtime_matches = match runtime.as_ref() {
        Some(runtime) if work.operation.runtime_ownership == TaskRuntimeOwnership::Provisioned => {
            runtime.metadata.uid.as_deref() == Some(runtime_uid)
                && runtime_identity_matches(runtime, work)
        }
        Some(runtime) => shared_runtime_binding_matches(runtime, work)?,
        None => false,
    };
    let Some(runtime) = runtime.filter(|_| runtime_matches) else {
        authority
            .enter_task_cleanup(
                task.task_uid,
                work.operation.generation,
                TaskCleanupCause::Failed("active_runtime_identity_disappeared"),
                TASK_ORCHESTRATOR_ACTOR,
            )
            .await
            .map_err(TaskControllerError::Store)?;
        return Ok(());
    };
    if !task_runtime_observation_is_ready(&runtime)? {
        return Ok(());
    }
    let refs = runtime
        .status
        .as_ref()
        .filter(|status| status.phase == Phase::Running)
        .map(|status| status.refs.clone())
        .ok_or_else(|| {
            TaskControllerError::InvalidState(
                "active Task runtime has no ready observed references".to_owned(),
            )
        })?;
    let input = task.input_archive.as_deref().ok_or_else(|| {
        TaskControllerError::InvalidState("queued Task has no input archive".to_owned())
    })?;
    let request = sandbox_task_request(task, runtime_uid.to_owned(), refs);
    let command_digest =
        bytes_digest(&serde_json::to_vec(&task.agent_command).map_err(|error| {
            TaskControllerError::InvalidState(format!("Task command cannot be digested: {error}"))
        })?);
    let input_digest = bytes_digest(input);
    let attempt = match authority
        .claim_task_execution_attempt(
            task.task_uid,
            &command_digest,
            &input_digest,
            TASK_ORCHESTRATOR_ACTOR,
        )
        .await
        .map_err(TaskControllerError::Store)?
    {
        TaskExecutionTransition::Created(_) => return Ok(()),
        TaskExecutionTransition::AlreadyApplied(attempt) => attempt,
        TaskExecutionTransition::Applied(_)
        | TaskExecutionTransition::Superseded(_)
        | TaskExecutionTransition::AuthorityInactive { .. }
        | TaskExecutionTransition::InvariantViolation { .. } => return Ok(()),
    };
    if attempt.state.is_terminal() {
        return Ok(());
    }
    let attempt_id = TaskAttemptId(attempt.attempt_id.to_string());
    if attempt.state == TaskExecutionAttemptState::StartPending
        && attempt.start_invoked_at.is_none()
    {
        authority
            .authorize_task_execution_start(
                attempt.attempt_id,
                attempt.generation,
                TASK_ORCHESTRATOR_ACTOR,
            )
            .await
            .map_err(TaskControllerError::Store)?;
        return Ok(());
    }
    if attempt.state != TaskExecutionAttemptState::StartPending {
        return observe_execution_attempt(sandbox_runtime, authority, &request, attempt).await;
    }
    let observation = sandbox_runtime
        .start_task(&attempt_id, &request, input)
        .await
        .unwrap_or_else(|error| SandboxTaskObservation::OutcomeUnknown {
            reason: task_failure_reason(&error),
        });
    persist_execution_observation(authority, &attempt, attempt.generation, observation).await
}

async fn reconcile_shared_runtime_observation(
    client: &Client,
    authority: &PgStore,
    work: &TaskOrchestrationWorkItem,
) -> Result<(), TaskControllerError> {
    let expected_uid = work
        .operation
        .expected_runtime_uid
        .as_deref()
        .ok_or_else(|| {
            TaskControllerError::InvalidState(
                "shared Task runtime binding has no server-resolved UID".to_owned(),
            )
        })?;
    let api = Api::<AgentRuntime>::namespaced(client.clone(), &work.operation.runtime_namespace);
    let Some(runtime) = api
        .get_opt(&work.operation.runtime_name)
        .await
        .map_err(TaskControllerError::Kubernetes)?
    else {
        authority
            .enter_task_cleanup(
                work.task.task_uid,
                work.operation.generation,
                TaskCleanupCause::Failed("shared_runtime_absent"),
                TASK_ORCHESTRATOR_ACTOR,
            )
            .await
            .map_err(TaskControllerError::Store)?;
        return Ok(());
    };
    if !shared_runtime_binding_matches(&runtime, work)? {
        authority
            .enter_task_cleanup(
                work.task.task_uid,
                work.operation.generation,
                TaskCleanupCause::Failed("shared_runtime_binding_mismatch"),
                TASK_ORCHESTRATOR_ACTOR,
            )
            .await
            .map_err(TaskControllerError::Store)?;
        return Ok(());
    }
    if !task_runtime_observation_is_ready(&runtime)? {
        return Ok(());
    }
    let resource_version = runtime
        .metadata
        .resource_version
        .as_deref()
        .ok_or_else(|| {
            TaskControllerError::InvalidState(
                "observed shared Task runtime has no resource version".to_owned(),
            )
        })?;
    authority
        .record_task_runtime_observed(
            work.task.task_uid,
            work.operation.generation,
            expected_uid,
            resource_version,
            TASK_ORCHESTRATOR_ACTOR,
        )
        .await
        .map(|_| ())
        .map_err(TaskControllerError::Store)
}

fn shared_runtime_binding_matches(
    runtime: &AgentRuntime,
    work: &TaskOrchestrationWorkItem,
) -> Result<bool, TaskControllerError> {
    let Some(expected_uid) = work.operation.expected_runtime_uid.as_deref() else {
        return Ok(false);
    };
    let runtime_uid_matches = runtime.metadata.uid.as_deref() == Some(expected_uid);
    let owner_matches = runtime.spec.owner == work.task.runtime_spec.owner
        && runtime.spec.canonical_authority == work.task.runtime_spec.canonical_authority;
    let binding_matches = match (
        work.operation.runtime_ownership,
        work.task.execution_binding.as_ref(),
    ) {
        (TaskRuntimeOwnership::Resident, Some(TaskExecutionBinding::Resident(binding))) => {
            spec_digest(&runtime.spec).map_err(|error| {
                TaskControllerError::InvalidState(format!(
                    "shared Task runtime spec cannot be digested: {error}"
                ))
            })? == binding.runtime_spec_digest
        }
        (TaskRuntimeOwnership::Adopted, None) => runtime.spec == work.task.runtime_spec,
        _ => false,
    };
    Ok(runtime_uid_matches && owner_matches && binding_matches)
}

fn task_runtime_observation_is_ready(runtime: &AgentRuntime) -> Result<bool, TaskControllerError> {
    let digest = spec_digest(&runtime.spec).map_err(|error| {
        TaskControllerError::InvalidState(format!("Task runtime spec cannot be digested: {error}"))
    })?;
    Ok(runtime.status.as_ref().is_some_and(|status| {
        status.phase == Phase::Running
            && status.observed_generation == runtime.metadata.generation.unwrap_or_default()
            && status.spec_digest == digest
            && status
                .refs
                .workspace
                .as_ref()
                .is_some_and(|reference| !reference.is_empty())
            && status
                .refs
                .sandbox
                .as_ref()
                .is_some_and(|reference| !reference.is_empty())
    }))
}

async fn reconcile_task_cancellation<R: SandboxTaskRuntime>(
    client: &Client,
    sandbox_runtime: &R,
    authority: &PgStore,
    task: &TaskRecord,
    attempt: steward_store::TaskExecutionAttemptRecord,
) -> Result<(), TaskControllerError> {
    let attempt = if attempt.state == TaskExecutionAttemptState::CancelPending {
        attempt
    } else {
        match authority
            .authorize_task_execution_cancel(
                attempt.attempt_id,
                attempt.generation,
                TASK_ORCHESTRATOR_ACTOR,
            )
            .await
            .map_err(TaskControllerError::Store)?
        {
            TaskExecutionTransition::Applied(attempt)
            | TaskExecutionTransition::AlreadyApplied(attempt) => attempt,
            TaskExecutionTransition::Superseded(_)
            | TaskExecutionTransition::Created(_)
            | TaskExecutionTransition::AuthorityInactive { .. }
            | TaskExecutionTransition::InvariantViolation { .. } => return Ok(()),
        }
    };
    let runtime = Api::<AgentRuntime>::namespaced(client.clone(), &task.runtime_namespace)
        .get_opt(&task.runtime_name)
        .await
        .map_err(TaskControllerError::Kubernetes)?;
    let Some(runtime) = runtime
        .filter(|runtime| runtime.metadata.uid.as_deref() == Some(attempt.runtime_uid.as_str()))
    else {
        let observation = if authority
            .task_execution_start_observation_expired(attempt.attempt_id)
            .await
            .map_err(TaskControllerError::Store)?
        {
            SandboxTaskObservation::OutcomeUnknown {
                reason: "runtime disappeared before cancellation was observed".to_owned(),
            }
        } else {
            return Ok(());
        };
        return persist_execution_observation(authority, &attempt, attempt.generation, observation)
            .await;
    };
    let refs = runtime.status.as_ref().map(|status| status.refs.clone());
    let refs = refs.filter(|refs| {
        refs.workspace
            .as_ref()
            .is_some_and(|value| !value.is_empty())
            && refs.sandbox.as_ref().is_some_and(|value| !value.is_empty())
    });
    let deadline_expired = authority
        .task_execution_start_observation_expired(attempt.attempt_id)
        .await
        .map_err(TaskControllerError::Store)?;
    let observation = if let Some(refs) = refs {
        let request = sandbox_task_request(task, attempt.runtime_uid.clone(), refs);
        match sandbox_runtime
            .cancel_task(&TaskAttemptId(attempt.attempt_id.to_string()), &request)
            .await
        {
            Ok(observation) => observation,
            Err(error) if !deadline_expired => return Err(TaskControllerError::Sandbox(error)),
            Err(_) => SandboxTaskObservation::OutcomeUnknown {
                reason: "cancellation could not be observed before its deadline".to_owned(),
            },
        }
    } else if deadline_expired {
        SandboxTaskObservation::OutcomeUnknown {
            reason: "runtime references disappeared before cancellation was observed".to_owned(),
        }
    } else {
        return Ok(());
    };
    let Some(observation) = terminalize_expired_attempt_observation(
        authority,
        &attempt,
        observation,
        "cancelled execution has no durable adapter observation",
    )
    .await?
    else {
        return Ok(());
    };
    match observation {
        SandboxTaskObservation::Accepted { .. } | SandboxTaskObservation::Running { .. } => Ok(()),
        observation => {
            persist_execution_observation(authority, &attempt, attempt.generation, observation)
                .await
        }
    }
}

async fn reconcile_runtime_creation(
    client: &Client,
    authority: &PgStore,
    work: &TaskOrchestrationWorkItem,
) -> Result<(), TaskControllerError> {
    let envelope = authority
        .service_envelope_revision(&work.task.submitter_service, work.task.envelope_revision)
        .await
        .map_err(TaskControllerError::Store)?
        .ok_or_else(|| {
            TaskControllerError::InvalidState(
                "Task's immutable Envelope revision is unavailable".to_owned(),
            )
        })?;
    let expected = orchestrated_task_runtime_manifest(work, Some(&envelope), false)?;
    let api = Api::<AgentRuntime>::namespaced(client.clone(), &work.operation.runtime_namespace);
    match api.create(&PostParams::default(), &expected).await {
        Ok(_) => {}
        Err(kube::Error::Api(response)) if response.code == 409 => {}
        Err(_) => {
            // The create result is ambiguous. Observation below, not compensation,
            // determines what happened.
        }
    }
    let Some(observed) = api
        .get_opt(&work.operation.runtime_name)
        .await
        .map_err(TaskControllerError::Kubernetes)?
    else {
        return Ok(());
    };
    if !runtime_matches_orchestration(&observed, &expected, work, "inert") {
        authority
            .enter_task_cleanup(
                work.task.task_uid,
                work.operation.generation,
                TaskCleanupCause::Failed("runtime_identity_collision"),
                TASK_ORCHESTRATOR_ACTOR,
            )
            .await
            .map_err(TaskControllerError::Store)?;
        return Ok(());
    }
    let uid = observed.metadata.uid.as_deref().ok_or_else(|| {
        TaskControllerError::InvalidState("observed Task runtime has no UID".to_owned())
    })?;
    let resource_version = observed
        .metadata
        .resource_version
        .as_deref()
        .ok_or_else(|| {
            TaskControllerError::InvalidState(
                "observed Task runtime has no resource version".to_owned(),
            )
        })?;
    authority
        .record_task_runtime_observed(
            work.task.task_uid,
            work.operation.generation,
            uid,
            resource_version,
            TASK_ORCHESTRATOR_ACTOR,
        )
        .await
        .map(|_| ())
        .map_err(TaskControllerError::Store)
}

async fn reconcile_runtime_activation(
    client: &Client,
    authority: &PgStore,
    work: &TaskOrchestrationWorkItem,
) -> Result<(), TaskControllerError> {
    let expected_uid = work.operation.runtime_uid.as_deref().ok_or_else(|| {
        TaskControllerError::InvalidState("activation has no exact runtime UID".to_owned())
    })?;
    let api = Api::<AgentRuntime>::namespaced(client.clone(), &work.operation.runtime_namespace);
    if work.operation.runtime_ownership != TaskRuntimeOwnership::Provisioned {
        let Some(observed) = api
            .get_opt(&work.operation.runtime_name)
            .await
            .map_err(TaskControllerError::Kubernetes)?
        else {
            authority
                .enter_task_cleanup(
                    work.task.task_uid,
                    work.operation.generation,
                    TaskCleanupCause::Failed("shared_runtime_absent_during_activation"),
                    TASK_ORCHESTRATOR_ACTOR,
                )
                .await
                .map_err(TaskControllerError::Store)?;
            return Ok(());
        };
        if !shared_runtime_binding_matches(&observed, work)? {
            authority
                .enter_task_cleanup(
                    work.task.task_uid,
                    work.operation.generation,
                    TaskCleanupCause::Failed("shared_runtime_binding_changed_during_activation"),
                    TASK_ORCHESTRATOR_ACTOR,
                )
                .await
                .map_err(TaskControllerError::Store)?;
            return Ok(());
        }
        if !task_runtime_observation_is_ready(&observed)? {
            return Ok(());
        }
        let resource_version = observed
            .metadata
            .resource_version
            .as_deref()
            .ok_or_else(|| {
                TaskControllerError::InvalidState(
                    "shared Task runtime activation has no resource version".to_owned(),
                )
            })?;
        return authority
            .record_task_activation_observed(
                work.task.task_uid,
                work.operation.generation,
                &TaskActivationObservation {
                    runtime_uid: expected_uid,
                    resource_version,
                    active_manifest_digest: &work.operation.active_manifest_digest,
                    provider_set_ready: true,
                },
                TASK_ORCHESTRATOR_ACTOR,
            )
            .await
            .map(|_| ())
            .map_err(TaskControllerError::Store);
    }
    let Some(mut observed) = api
        .get_opt(&work.operation.runtime_name)
        .await
        .map_err(TaskControllerError::Kubernetes)?
    else {
        authority
            .enter_task_cleanup(
                work.task.task_uid,
                work.operation.generation,
                TaskCleanupCause::Failed("observed_runtime_disappeared"),
                TASK_ORCHESTRATOR_ACTOR,
            )
            .await
            .map_err(TaskControllerError::Store)?;
        return Ok(());
    };
    if observed.metadata.uid.as_deref() != Some(expected_uid)
        || !runtime_identity_matches(&observed, work)
    {
        authority
            .enter_task_cleanup(
                work.task.task_uid,
                work.operation.generation,
                TaskCleanupCause::Failed("observed_runtime_identity_changed"),
                TASK_ORCHESTRATOR_ACTOR,
            )
            .await
            .map_err(TaskControllerError::Store)?;
        return Ok(());
    }
    let desired = orchestrated_task_runtime_manifest(work, None, true)?;
    if observed.spec != desired.spec || !runtime_mode_matches(&observed, &desired, "active") {
        let resource_version = observed.metadata.resource_version.clone().ok_or_else(|| {
            TaskControllerError::InvalidState(
                "Task runtime activation has no resource version".to_owned(),
            )
        })?;
        let mut replacement = desired;
        replacement.metadata.resource_version = Some(resource_version);
        match api
            .replace(
                &work.operation.runtime_name,
                &PostParams::default(),
                &replacement,
            )
            .await
        {
            Ok(_) => {}
            Err(kube::Error::Api(response)) if response.code == 409 => {}
            Err(_) => return Ok(()),
        }
        observed = match api.get(&work.operation.runtime_name).await {
            Ok(runtime) => runtime,
            Err(kube::Error::Api(response)) if response.code == 404 => return Ok(()),
            Err(error) => return Err(TaskControllerError::Kubernetes(error)),
        };
    }
    let expected = orchestrated_task_runtime_manifest(work, None, true)?;
    let expected_spec_digest = spec_digest(&expected.spec).map_err(|error| {
        TaskControllerError::InvalidState(format!(
            "active Task runtime spec cannot be digested: {error}"
        ))
    })?;
    let ready = runtime_matches_orchestration(&observed, &expected, work, "active")
        && observed.status.as_ref().is_some_and(|status| {
            status.phase == Phase::Running
                && status.observed_generation == observed.metadata.generation.unwrap_or_default()
                && status.spec_digest == expected_spec_digest
        });
    let resource_version = observed
        .metadata
        .resource_version
        .as_deref()
        .ok_or_else(|| {
            TaskControllerError::InvalidState(
                "active Task runtime has no resource version".to_owned(),
            )
        })?;
    authority
        .record_task_activation_observed(
            work.task.task_uid,
            work.operation.generation,
            &TaskActivationObservation {
                runtime_uid: expected_uid,
                resource_version,
                active_manifest_digest: &work.operation.active_manifest_digest,
                provider_set_ready: ready,
            },
            TASK_ORCHESTRATOR_ACTOR,
        )
        .await
        .map(|_| ())
        .map_err(TaskControllerError::Store)
}

async fn reconcile_runtime_cleanup(
    client: &Client,
    authority: &PgStore,
    work: &TaskOrchestrationWorkItem,
) -> Result<(), TaskControllerError> {
    let api = Api::<AgentRuntime>::namespaced(client.clone(), &work.operation.runtime_namespace);
    let observed = api
        .get_opt(&work.operation.runtime_name)
        .await
        .map_err(TaskControllerError::Kubernetes)?;
    if let Some(expected_uid) = work.operation.runtime_uid.as_deref() {
        if work.operation.runtime_ownership == TaskRuntimeOwnership::Provisioned
            && let Some(runtime) = observed.as_ref()
            && runtime.metadata.uid.as_deref() == Some(expected_uid)
        {
            api.delete(
                &work.operation.runtime_name,
                &DeleteParams {
                    preconditions: Some(Preconditions {
                        uid: Some(expected_uid.to_owned()),
                        resource_version: None,
                    }),
                    ..DeleteParams::default()
                },
            )
            .await
            .map(|_| ())
            .or_else(|error| match error {
                kube::Error::Api(response) if response.code == 404 => Ok(()),
                error => Err(error),
            })
            .map_err(TaskControllerError::Kubernetes)?;
            return Ok(());
        }
        return authority
            .record_task_cleanup_complete(
                work.task.task_uid,
                work.operation.generation,
                TaskCleanupObservation {
                    exact_runtime_absent: work.operation.runtime_ownership
                        == TaskRuntimeOwnership::Provisioned,
                },
                TASK_ORCHESTRATOR_ACTOR,
            )
            .await
            .map(|_| ())
            .map_err(TaskControllerError::Store);
    }
    if work.operation.runtime_create_authorized_at.is_none() {
        return authority
            .record_task_cleanup_complete(
                work.task.task_uid,
                work.operation.generation,
                TaskCleanupObservation {
                    exact_runtime_absent: false,
                },
                TASK_ORCHESTRATOR_ACTOR,
            )
            .await
            .map(|_| ())
            .map_err(TaskControllerError::Store);
    }
    let envelope = authority
        .service_envelope_revision(&work.task.submitter_service, work.task.envelope_revision)
        .await
        .map_err(TaskControllerError::Store)?
        .ok_or_else(|| {
            TaskControllerError::InvalidState(
                "Task's immutable Envelope revision is unavailable during cleanup".to_owned(),
            )
        })?;
    let expected = orchestrated_task_runtime_manifest(work, Some(&envelope), false)?;
    if let Some(runtime) = observed.as_ref() {
        if runtime_matches_orchestration(runtime, &expected, work, "inert") {
            let uid = runtime.metadata.uid.as_deref().ok_or_else(|| {
                TaskControllerError::InvalidState("ambiguous Task runtime has no UID".to_owned())
            })?;
            let resource_version =
                runtime
                    .metadata
                    .resource_version
                    .as_deref()
                    .ok_or_else(|| {
                        TaskControllerError::InvalidState(
                            "ambiguous Task runtime has no resource version".to_owned(),
                        )
                    })?;
            authority
                .record_task_cleanup_runtime_observed(
                    work.task.task_uid,
                    work.operation.generation,
                    uid,
                    resource_version,
                    TASK_ORCHESTRATOR_ACTOR,
                )
                .await
                .map_err(TaskControllerError::Store)?;
        }
        return Ok(());
    }
    match api.create(&PostParams::default(), &expected).await {
        Ok(_) => {}
        Err(kube::Error::Api(response)) if response.code == 409 => {}
        Err(_) => return Ok(()),
    }
    let Some(runtime) = api
        .get_opt(&work.operation.runtime_name)
        .await
        .map_err(TaskControllerError::Kubernetes)?
    else {
        return Ok(());
    };
    if !runtime_matches_orchestration(&runtime, &expected, work, "inert") {
        return Ok(());
    }
    let uid = runtime.metadata.uid.as_deref().ok_or_else(|| {
        TaskControllerError::InvalidState("ambiguous Task runtime has no UID".to_owned())
    })?;
    let resource_version = runtime
        .metadata
        .resource_version
        .as_deref()
        .ok_or_else(|| {
            TaskControllerError::InvalidState(
                "ambiguous Task runtime has no resource version".to_owned(),
            )
        })?;
    authority
        .record_task_cleanup_runtime_observed(
            work.task.task_uid,
            work.operation.generation,
            uid,
            resource_version,
            TASK_ORCHESTRATOR_ACTOR,
        )
        .await
        .map(|_| ())
        .map_err(TaskControllerError::Store)
}

fn orchestrated_task_runtime_manifest(
    work: &TaskOrchestrationWorkItem,
    envelope: Option<&Envelope>,
    active: bool,
) -> Result<AgentRuntime, TaskControllerError> {
    let mut runtime = server_task_runtime_manifest(TaskRuntimeBinding::from(&work.task))?;
    let mode = if active { "active" } else { "inert" };
    let digest = if active {
        &work.operation.active_manifest_digest
    } else {
        let envelope = envelope.ok_or_else(|| {
            TaskControllerError::InvalidState(
                "inert Task runtime construction requires its Envelope".to_owned(),
            )
        })?;
        runtime.spec.llms.clear();
        runtime.spec.tools.clear();
        runtime.spec.budget.monthly_limit = "0".to_owned();
        runtime.spec.budget.single_run_limit = Some("0".to_owned());
        runtime.spec.budget.currency = envelope.spec.budget.currency.clone();
        &work.operation.inert_manifest_digest
    };
    let canonical_digest = bytes_digest(
        &serde_json::to_vec(&serde_json::json!({
            "schemaVersion": "steward-task-runtime-manifest/v1",
            "taskUid": work.task.task_uid,
            "operationId": work.operation.operation_id,
            "runtimeNamespace": work.operation.runtime_namespace,
            "runtimeName": work.operation.runtime_name,
            "mode": mode,
            "spec": &runtime.spec,
            "executionBinding": &work.task.execution_binding,
        }))
        .map_err(|error| {
            TaskControllerError::InvalidState(format!(
                "Task runtime manifest cannot be canonicalized: {error}"
            ))
        })?,
    );
    if &canonical_digest != digest {
        return Err(TaskControllerError::InvalidState(
            "persisted Task runtime manifest digest does not match immutable intent".to_owned(),
        ));
    }
    let annotations = runtime.metadata.annotations.get_or_insert_default();
    annotations.insert(
        TASK_UID_ANNOTATION.to_owned(),
        work.task.task_uid.to_string(),
    );
    annotations.insert(
        TASK_OPERATION_ANNOTATION.to_owned(),
        work.operation.operation_id.to_string(),
    );
    annotations.insert(TASK_MANIFEST_DIGEST_ANNOTATION.to_owned(), digest.clone());
    annotations.insert(TASK_RUNTIME_MODE_ANNOTATION.to_owned(), mode.to_owned());
    if active {
        annotations.remove(PENDING_APPROVAL_ANNOTATION);
    } else {
        annotations.insert(
            PENDING_APPROVAL_ANNOTATION.to_owned(),
            work.task.candidate_digest.clone().ok_or_else(|| {
                TaskControllerError::InvalidState(
                    "Task has no immutable candidate digest".to_owned(),
                )
            })?,
        );
    }
    Ok(runtime)
}

fn runtime_identity_matches(runtime: &AgentRuntime, work: &TaskOrchestrationWorkItem) -> bool {
    let annotations = runtime.annotations();
    annotations.get(TASK_UID_ANNOTATION) == Some(&work.task.task_uid.to_string())
        && annotations.get(TASK_OPERATION_ANNOTATION)
            == Some(&work.operation.operation_id.to_string())
}

fn runtime_mode_matches(runtime: &AgentRuntime, expected: &AgentRuntime, mode: &str) -> bool {
    let annotations = runtime.annotations();
    let expected_annotations = expected.annotations();
    [
        SERVICE_PRINCIPAL_ANNOTATION,
        TASK_EXECUTION_BINDING_ANNOTATION,
        TASK_UID_ANNOTATION,
        TASK_OPERATION_ANNOTATION,
        TASK_MANIFEST_DIGEST_ANNOTATION,
        TASK_RUNTIME_MODE_ANNOTATION,
        PENDING_APPROVAL_ANNOTATION,
    ]
    .into_iter()
    .all(|key| annotations.get(key) == expected_annotations.get(key))
        && annotations
            .get(TASK_RUNTIME_MODE_ANNOTATION)
            .is_some_and(|value| value == mode)
}

fn runtime_matches_orchestration(
    runtime: &AgentRuntime,
    expected: &AgentRuntime,
    work: &TaskOrchestrationWorkItem,
    mode: &str,
) -> bool {
    runtime.spec == expected.spec
        && runtime_identity_matches(runtime, work)
        && runtime_mode_matches(runtime, expected, mode)
}

async fn observe_execution_attempt<R: SandboxTaskRuntime>(
    sandbox_runtime: &R,
    authority: &PgStore,
    request: &SandboxTaskRequest,
    attempt: steward_store::TaskExecutionAttemptRecord,
) -> Result<(), TaskControllerError> {
    let attempt_id = TaskAttemptId(attempt.attempt_id.to_string());
    let observation = sandbox_runtime
        .observe_task(&attempt_id, request)
        .await
        .map_err(TaskControllerError::Sandbox)?;
    let Some(observation) = terminalize_expired_attempt_observation(
        authority,
        &attempt,
        observation,
        "authorized execution start has no durable adapter observation",
    )
    .await?
    else {
        return Ok(());
    };
    persist_execution_observation(authority, &attempt, attempt.generation, observation).await
}

async fn terminalize_expired_attempt_observation(
    authority: &PgStore,
    attempt: &steward_store::TaskExecutionAttemptRecord,
    observation: SandboxTaskObservation,
    absent_reason: &str,
) -> Result<Option<SandboxTaskObservation>, TaskControllerError> {
    let can_expire = matches!(
        observation,
        SandboxTaskObservation::Absent | SandboxTaskObservation::Accepted { .. }
    );
    if can_expire
        && attempt.start_invoked_at.is_some()
        && authority
            .task_execution_start_observation_expired(attempt.attempt_id)
            .await
            .map_err(TaskControllerError::Store)?
    {
        return Ok(Some(SandboxTaskObservation::OutcomeUnknown {
            reason: absent_reason.to_owned(),
        }));
    }
    if matches!(observation, SandboxTaskObservation::Absent) {
        Ok(None)
    } else {
        Ok(Some(observation))
    }
}

async fn persist_execution_observation(
    authority: &PgStore,
    attempt: &steward_store::TaskExecutionAttemptRecord,
    generation: i64,
    observation: SandboxTaskObservation,
) -> Result<(), TaskControllerError> {
    let transition = match observation {
        SandboxTaskObservation::Absent => return Ok(()),
        SandboxTaskObservation::Accepted {
            adapter_observation_id,
        } => {
            authority
                .record_task_execution_observation(
                    attempt.attempt_id,
                    generation,
                    TaskExecutionObservation::Accepted {
                        adapter_observation_id: &adapter_observation_id,
                    },
                    "task-orchestrator",
                )
                .await
        }
        SandboxTaskObservation::Running {
            adapter_observation_id,
        } => {
            authority
                .record_task_execution_observation(
                    attempt.attempt_id,
                    generation,
                    TaskExecutionObservation::Running {
                        adapter_observation_id: &adapter_observation_id,
                    },
                    "task-orchestrator",
                )
                .await
        }
        SandboxTaskObservation::Succeeded {
            adapter_observation_id,
            output,
        } => {
            if let Some(reason) = task_output_archive_failure(output.archive.len()) {
                authority
                    .record_task_execution_observation(
                        attempt.attempt_id,
                        generation,
                        TaskExecutionObservation::Failed {
                            adapter_observation_id: &adapter_observation_id,
                            reason,
                        },
                        "task-orchestrator",
                    )
                    .await
            } else {
                let result_digest = bytes_digest(&output.archive);
                let result_reference = format!("adapter:{adapter_observation_id}");
                authority
                    .record_task_execution_observation(
                        attempt.attempt_id,
                        generation,
                        TaskExecutionObservation::Succeeded {
                            adapter_observation_id: &adapter_observation_id,
                            result_digest: &result_digest,
                            result_reference: &result_reference,
                            output_archive: &output.archive,
                        },
                        "task-orchestrator",
                    )
                    .await
            }
        }
        SandboxTaskObservation::Failed {
            adapter_observation_id,
            reason,
        } => {
            authority
                .record_task_execution_observation(
                    attempt.attempt_id,
                    generation,
                    TaskExecutionObservation::Failed {
                        adapter_observation_id: &adapter_observation_id,
                        reason: &reason,
                    },
                    "task-orchestrator",
                )
                .await
        }
        SandboxTaskObservation::OutcomeUnknown { reason } => {
            authority
                .record_task_execution_observation(
                    attempt.attempt_id,
                    generation,
                    TaskExecutionObservation::OutcomeUnknown { reason: &reason },
                    "task-orchestrator",
                )
                .await
        }
    }
    .map_err(TaskControllerError::Store)?;
    match transition {
        TaskExecutionTransition::Created(_)
        | TaskExecutionTransition::Applied(_)
        | TaskExecutionTransition::AlreadyApplied(_)
        | TaskExecutionTransition::Superseded(_)
        | TaskExecutionTransition::AuthorityInactive { .. }
        | TaskExecutionTransition::InvariantViolation { .. } => Ok(()),
    }
}

fn bytes_digest(value: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(value))
}

fn sandbox_task_request(
    task: &TaskRecord,
    runtime_uid: String,
    refs: RuntimeRefs,
) -> SandboxTaskRequest {
    SandboxTaskRequest {
        runtime: RuntimeId(runtime_uid),
        refs,
        execution_class: sandbox_execution_class(&task.runtime_spec),
        agent_type: task.runtime_spec.agent_type.clone(),
        command: task.agent_command.clone(),
        execution_binding: task
            .execution_binding
            .as_ref()
            .and_then(TaskExecutionBinding::disposable)
            .cloned(),
    }
}

fn sandbox_execution_class(spec: &AgentRuntimeSpec) -> SandboxExecutionClass {
    if spec.agent_type.name == "connections-bridge"
        && matches!(
            &spec.principal,
            steward_types::Principal::Service { name, .. } if name == "steward-connections"
        )
    {
        SandboxExecutionClass::ProviderControl
    } else {
        SandboxExecutionClass::Agent
    }
}

fn connection_operation_bindings_match(
    operation: &ConnectionOperationRecord,
    task: &TaskRecord,
    current: Option<&steward_ports::ProviderControlExecutionBindings>,
) -> bool {
    let Some(current) = current else {
        return false;
    };
    operation.authority_id == "steward-connections"
        && operation.authority_version == 1
        && operation.authority_digest == steward_connections_v1::AUTHORITY_DIGEST
        && task.internal_authority_id.as_deref() == Some(operation.authority_id.as_str())
        && task.internal_authority_version == Some(operation.authority_version)
        && task.internal_authority_digest.as_deref() == Some(operation.authority_digest.as_str())
        && operation.runtime_spec_snapshot == task.runtime_spec
        && operation.command_snapshot == task.agent_command
        && provider_control_bindings_match(&operation.bindings, current)
        && task.runtime_namespace == operation.bindings.namespace
}

fn provider_control_bindings_match(
    persisted: &steward_store::ConnectionExecutionBindingSnapshot,
    current: &steward_ports::ProviderControlExecutionBindings,
) -> bool {
    persisted.artifact_trust_mode == current.artifact_trust_mode
        && persisted.bridge_image_digest == current.bridge_image_digest
        && persisted.mcp_gw_origin == current.mcp_gw_origin
        && persisted.mcp_gw_version == current.mcp_gw_version
        && persisted.namespace == current.namespace
        && persisted.runtime_class == current.runtime_class
}

fn connection_operation_authority_action(
    runtime: &AgentRuntime,
    operation: &ConnectionOperationRecord,
    current: Option<&steward_ports::ProviderControlExecutionBindings>,
) -> Result<AuthorityAction, ReconcileError> {
    let runtime_uid = runtime
        .metadata
        .uid
        .as_deref()
        .ok_or(ReconcileError::MissingRuntimeUid)?;
    let namespace = runtime
        .metadata
        .namespace
        .as_deref()
        .ok_or(ReconcileError::MissingNamespace)?;
    let expected_action = operation.operation_kind.as_str();
    let expected_bridge_operation = match operation.operation_kind {
        ConnectionOperationKind::Status => "github.status",
        ConnectionOperationKind::Start => "github.start",
        ConnectionOperationKind::Disconnect => "github.disconnect",
    };
    let expected_command = [
        steward_connections_v1::BRIDGE_BINARY,
        "--operation",
        expected_bridge_operation,
        "--input",
        steward_connections_v1::INPUT_FILE,
    ];
    let expected_grant = steward_connections_v1::provider_control_grant(expected_action)
        .ok_or_else(|| {
            ReconcileError::Authority(
                "connection operation has an unsupported provider-control action".to_owned(),
            )
        })?;
    let canonical_authority = runtime.spec.canonical_authority.as_ref();
    let identity_matches = canonical_authority.is_some_and(|binding| {
        binding.owner_user_id.as_str() == operation.canonical_user_id
            && binding.acting_user_id.as_ref().map(|id| id.as_str())
                == Some(operation.canonical_user_id.as_str())
    });
    let principal_matches = matches!(
        &runtime.spec.principal,
        steward_types::Principal::Service {
            name,
            acting_user: Some(acting_user),
        } if name == steward_connections_v1::SERVICE && acting_user == &runtime.spec.owner
    );
    let authority = steward_connections_v1::envelope();
    let fixed_limits_match = runtime.spec.budget == authority.spec.budget
        && runtime.spec.ttl == authority.spec.ttl
        && runtime.spec.runner == authority.spec.runner;
    let admitted =
        evaluate(&runtime.spec, &authority).map_err(|error| ReconcileError::InvalidSpec {
            reason: format!("{error:?}"),
        })? == AdmissionDecision::Admit;
    let current_matches = current
        .is_some_and(|bindings| provider_control_bindings_match(&operation.bindings, bindings));

    if operation.operation_id != operation.task_uid
        || operation.provider != "github"
        || operation.authority_id != steward_connections_v1::AUTHORITY_ID
        || operation.authority_version != steward_connections_v1::AUTHORITY_VERSION
        || operation.authority_digest != steward_connections_v1::AUTHORITY_DIGEST
        || operation.runtime_uid.as_deref() != Some(runtime_uid)
        || operation.runtime_spec_snapshot != runtime.spec
        || operation
            .command_snapshot
            .iter()
            .map(String::as_str)
            .ne(expected_command)
        || namespace != operation.bindings.namespace
        || runtime.spec.agent_type.name != steward_connections_v1::AGENT_TYPE
        || !runtime.spec.llms.is_empty()
        || runtime.spec.tools.as_slice() != [expected_grant]
        || runtime.spec.bindings.is_some()
        || !fixed_limits_match
        || !identity_matches
        || !principal_matches
        || runtime
            .annotations()
            .get(SERVICE_PRINCIPAL_ANNOTATION)
            .map(String::as_str)
            != Some(steward_connections_v1::SERVICE)
        || runtime.annotations().contains_key(MEMBER_ROLE_ANNOTATION)
        || !current_matches
        || !admitted
    {
        return Ok(AuthorityAction::Suspend);
    }
    Ok(AuthorityAction::Continue)
}

#[cfg(test)]
trait TaskRuntimeBindingStore {
    fn bind_task_runtime(
        &self,
        task: &TaskRecord,
        runtime_uid: &str,
        phase: TaskPhase,
    ) -> impl Future<Output = Result<TaskRecord, StoreError>> + Send;

    fn service_envelope_revision(
        &self,
        _service: &str,
        _revision: i64,
    ) -> impl Future<Output = Result<Option<Envelope>, StoreError>> + Send {
        async {
            Err(StoreError::Database(
                "service envelope recovery is unavailable".to_owned(),
            ))
        }
    }
}

#[cfg(test)]
async fn create_task_runtime_inner(
    client: &Client,
    authority: &impl TaskRuntimeBindingStore,
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
    let binding = authority
        .bind_task_runtime(task, runtime_uid, task.phase)
        .await;
    let Err(binding_error) = binding else {
        return Ok(());
    };
    if !matches!(
        &binding_error,
        StoreError::InvalidTaskTransition | StoreError::TaskNotFound
    ) {
        return Err(TaskControllerError::Store(binding_error));
    }
    let deletion = api
        .delete(
            &created.name_any(),
            &DeleteParams {
                preconditions: Some(Preconditions {
                    uid: Some(runtime_uid.to_owned()),
                    resource_version: None,
                }),
                ..DeleteParams::default()
            },
        )
        .await;
    match deletion {
        Ok(_) => Err(TaskControllerError::Store(binding_error)),
        Err(kube::Error::Api(response)) if response.code == 404 => {
            Err(TaskControllerError::Store(binding_error))
        }
        Err(cleanup_error) => Err(TaskControllerError::RuntimeBindingCleanup {
            binding: binding_error,
            cleanup: Box::new(cleanup_error),
        }),
    }
}

#[cfg(test)]
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
    execution_binding: Option<&'a TaskExecutionBinding>,
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
            execution_binding: task.execution_binding.as_ref(),
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
    if let Some(binding) = task.execution_binding {
        let disposable = binding.disposable().ok_or_else(|| {
            TaskControllerError::InvalidState(
                "resident execution bindings cannot provision task-owned runtimes".to_owned(),
            )
        })?;
        disposable.validate().map_err(|reason| {
            TaskControllerError::InvalidState(format!(
                "task execution binding is invalid: {reason}"
            ))
        })?;
        if disposable.agent_ref != task.runtime_spec.agent_type.name {
            return Err(TaskControllerError::InvalidState(
                "task execution binding does not match runtime agent type".to_owned(),
            ));
        }
        runtime.metadata.annotations.get_or_insert_default().insert(
            TASK_EXECUTION_BINDING_ANNOTATION.to_owned(),
            serde_json::to_string(disposable).map_err(|error| {
                TaskControllerError::InvalidState(format!(
                    "task execution binding cannot be serialized: {error}"
                ))
            })?,
        );
    }
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

#[cfg(test)]
async fn task_runtime(
    client: &Client,
    authority: &impl TaskRuntimeBindingStore,
    task: &TaskRecord,
) -> Result<Option<AgentRuntime>, TaskControllerError> {
    if task.runtime_uid.is_none()
        && (!task.finalize_requested || task.runtime_ownership != RuntimeOwnership::Provisioned)
    {
        return Ok(None);
    }
    let runtime = Api::<AgentRuntime>::namespaced(client.clone(), &task.runtime_namespace)
        .get_opt(&task.runtime_name)
        .await
        .map_err(TaskControllerError::Kubernetes)?;
    if let Some(expected_uid) = task.runtime_uid.as_deref() {
        return Ok(runtime.filter(|runtime| runtime.metadata.uid.as_deref() == Some(expected_uid)));
    }
    let expected = task_runtime_manifest(task)?;
    let Some(runtime) = runtime else {
        return Ok(None);
    };
    if runtime.spec == expected.spec && runtime.annotations() == expected.annotations() {
        return Ok(Some(runtime));
    }
    let envelope = authority
        .service_envelope_revision(&task.submitter_service, task.envelope_revision)
        .await
        .map_err(TaskControllerError::Store)?
        .ok_or_else(|| {
            TaskControllerError::InvalidState(
                "task's exact service envelope revision is unavailable".to_owned(),
            )
        })?;
    let mut pending = expected;
    pending.spec.llms.clear();
    pending.spec.tools.clear();
    pending.spec.budget.monthly_limit = "0".to_owned();
    pending.spec.budget.currency = envelope.spec.budget.currency;
    pending.spec.ttl = envelope.spec.ttl;
    pending.metadata.annotations.get_or_insert_default().insert(
        PENDING_APPROVAL_ANNOTATION.to_owned(),
        spec_digest(&task.runtime_spec).map_err(|error| {
            TaskControllerError::InvalidState(format!(
                "task runtime spec cannot be digested: {error}"
            ))
        })?,
    );
    Ok(
        (runtime.spec == pending.spec && runtime.annotations() == pending.annotations())
            .then_some(runtime),
    )
}

#[derive(Debug)]
pub enum TaskControllerError {
    Kubernetes(kube::Error),
    Sandbox(PortError),
    Store(StoreError),
    #[cfg(test)]
    RuntimeBindingCleanup {
        binding: StoreError,
        cleanup: Box<kube::Error>,
    },
    InvalidState(String),
}

impl fmt::Display for TaskControllerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Kubernetes(error) => {
                write!(formatter, "Kubernetes task operation failed: {error}")
            }
            Self::Sandbox(error) => write!(formatter, "sandbox Task observation failed: {error:?}"),
            Self::Store(error) => write!(formatter, "task store operation failed: {error}"),
            #[cfg(test)]
            Self::RuntimeBindingCleanup { binding, cleanup } => write!(
                formatter,
                "task runtime binding failed ({binding}) and exact-UID cleanup failed ({cleanup})"
            ),
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
                    let cleanup = cleanup_runtime(
                        &runtime,
                        context.client.clone(),
                        &context.inference,
                        &context.sandbox_runtime,
                    )
                    .await?;
                    if let ReconcileDecision::Status(status) = cleanup {
                        if runtime.status.as_ref() != Some(&status) {
                            api.patch_status(
                                &runtime.name_any(),
                                &PatchParams::default(),
                                &Patch::Merge(&status_merge_patch(&status)),
                            )
                            .await
                            .map_err(ControllerError::Kubernetes)?;
                        }
                        return Ok(Action::requeue(StdDuration::from_secs(2)));
                    }
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
                    let runtime_uid =
                        runtime
                            .metadata
                            .uid
                            .as_deref()
                            .ok_or(ControllerError::Reconcile(
                                ReconcileError::MissingRuntimeUid,
                            ))?;
                    let connection_operation = authority
                        .connection_operation_for_runtime(runtime_uid)
                        .await
                        .map_err(|error| {
                            ControllerError::Reconcile(ReconcileError::Authority(error.to_string()))
                        })?;
                    if let Some(operation) = connection_operation {
                        if matches!(
                            connection_operation_authority_action(
                                &runtime,
                                &operation,
                                context.sandbox_runtime.provider_control_bindings().as_ref(),
                            )
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
                    } else {
                        if let Some(reversion) = authority
                            .grant_reversion(runtime.metadata.uid.as_deref().ok_or(
                                ControllerError::Reconcile(ReconcileError::MissingRuntimeUid),
                            )?)
                            .await
                            .map_err(|error| {
                                ControllerError::Reconcile(ReconcileError::Authority(
                                    error.to_string(),
                                ))
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
                                        ControllerError::Reconcile(
                                            ReconcileError::MissingRuntimeUid,
                                        ),
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
                                ControllerError::Reconcile(ReconcileError::Authority(
                                    error.to_string(),
                                ))
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
                                ControllerError::Reconcile(ReconcileError::Authority(
                                    error.to_string(),
                                ))
                            })?
                            .ok_or_else(|| {
                                ControllerError::Reconcile(ReconcileError::Authority(
                                    "runtime principal no longer has an envelope".to_owned(),
                                ))
                            })?;
                        let grants = authority
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
                        if let Some(spend) = authority
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
    let (mut status, requeue_after) = match decision {
        ReconcileDecision::Deleted => (suspended_status(runtime)?, StdDuration::from_secs(60)),
        ReconcileDecision::Status(status) => (status, StdDuration::from_secs(2)),
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
    Ok(Action::requeue(requeue_after))
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
    let execution_binding = runtime.annotations().get(TASK_EXECUTION_BINDING_ANNOTATION);
    if request.operation == Operation::Create
        && execution_binding.is_some()
        && !trusted_writer_usernames.contains(username)
    {
        return response.deny("task execution binding may be set only by a trusted Steward writer");
    }
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
        if old_runtime
            .annotations()
            .get(TASK_EXECUTION_BINDING_ANNOTATION)
            != execution_binding
        {
            return response.deny("task execution binding is immutable");
        }
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
    use steward_admission::internal_authorities::steward_connections_v1;
    use steward_admission::{AdmissionDelta, Envelope, EnvelopeSpec};
    use steward_ports::{
        InferenceCapabilities, InferenceObservation, InferencePlane, InferenceRequest,
        MAX_TASK_OUTPUT_ARCHIVE_BYTES, PortError, ProviderControlExecutionBindings,
        ProvisionedInference, SandboxExecutionClass, SandboxObservation, SandboxRequest,
        SandboxRuntime,
    };
    use steward_store::{
        ConnectionExecutionBindingSnapshot, ConnectionOAuthPhase, ConnectionOperationKind,
        ConnectionOperationRecord, ConnectionOperationState, GrantReversion, StoreError,
        TaskRecord,
    };
    use steward_types::{
        AgentRuntime, AgentRuntimeSpec, AgentRuntimeStatus, AgentType, Budget,
        CanonicalAuthorityBinding, CanonicalUserId, Duration, Email, ModelRef,
        PENDING_APPROVAL_ANNOTATION, Phase, Principal, ResidentExecutionBinding, RuntimeId,
        RuntimeOwnership, RuntimeRefs, TASK_EXECUTION_BINDING_SCHEMA_VERSION, TaskExecutionBinding,
        TaskPhase,
    };
    use tower::service_fn;

    use super::{
        Action, AuthorityAction, InferenceAction, MEMBER_ROLE_ANNOTATION, ReconcileDecision,
        ReconcileIntent, SERVICE_PRINCIPAL_ANNOTATION, TaskRuntimeAction, TaskRuntimeBinding,
        TaskRuntimeBindingStore, authority_action, authority_application_action, cleanup_runtime,
        connection_operation_authority_action, create_task_runtime_inner,
        exhausted_spend_to_preserve, inference_action, provider_control_bindings_match,
        reconcile_once, replace_as_authority, runtime_authority_action, runtime_ttl_action,
        sandbox_execution_class, server_task_runtime_manifest, status_merge_patch, suspend_runtime,
        suspend_runtime_with_inference_cleanup, task_output_archive_failure, task_runtime,
        task_runtime_action, ttl_action,
    };

    struct RejectingTaskRuntimeBindingStore;

    impl TaskRuntimeBindingStore for RejectingTaskRuntimeBindingStore {
        async fn bind_task_runtime(
            &self,
            _task: &TaskRecord,
            _runtime_uid: &str,
            _phase: TaskPhase,
        ) -> Result<TaskRecord, StoreError> {
            Err(StoreError::InvalidTaskTransition)
        }
    }

    struct RecoveringTaskRuntimeBindingStore;

    impl TaskRuntimeBindingStore for RecoveringTaskRuntimeBindingStore {
        async fn bind_task_runtime(
            &self,
            _task: &TaskRecord,
            _runtime_uid: &str,
            _phase: TaskPhase,
        ) -> Result<TaskRecord, StoreError> {
            Err(StoreError::InvalidTaskTransition)
        }

        async fn service_envelope_revision(
            &self,
            _service: &str,
            revision: i64,
        ) -> Result<Option<Envelope>, StoreError> {
            let mut recovered = envelope("1.00");
            recovered.revision = revision;
            recovered.spec.ttl = Duration("24h".to_owned());
            Ok(Some(recovered))
        }
    }

    struct AmbiguousTaskRuntimeBindingStore {
        durable_runtime_uid: Arc<Mutex<Option<String>>>,
    }

    impl TaskRuntimeBindingStore for AmbiguousTaskRuntimeBindingStore {
        async fn bind_task_runtime(
            &self,
            _task: &TaskRecord,
            runtime_uid: &str,
            _phase: TaskPhase,
        ) -> Result<TaskRecord, StoreError> {
            *self.durable_runtime_uid.lock().map_err(|_| {
                StoreError::Database("durable binding fixture lock was poisoned".to_owned())
            })? = Some(runtime_uid.to_owned());
            Err(StoreError::Database(
                "post-bind task read failed".to_owned(),
            ))
        }
    }

    struct TaskRuntimeCreationFixture {
        task: TaskRecord,
        client: Client,
        delete_body: Arc<Mutex<Option<Vec<u8>>>>,
    }

    fn task_runtime_creation_fixture() -> Result<TaskRuntimeCreationFixture, String> {
        task_runtime_creation_fixture_with_pending(false)
    }

    fn task_runtime_creation_fixture_with_pending(
        pending: bool,
    ) -> Result<TaskRuntimeCreationFixture, String> {
        let canonical_user_id = CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?;
        let mut spec = fixture().spec;
        spec.principal = Principal::Service {
            name: "steward-run".to_owned(),
            acting_user: Some(Email("alice@example.com".to_owned())),
        };
        spec.owner = Email("alice@example.com".to_owned());
        spec.canonical_authority = Some(CanonicalAuthorityBinding::new(
            canonical_user_id.clone(),
            Some(canonical_user_id),
        )?);
        let task = TaskRecord {
            task_uid: serde_json::from_value(serde_json::json!(
                "00000000-0000-0000-0000-000000000000"
            ))
            .map_err(|error| format!("parse task UID fixture: {error}"))?,
            idempotency_key: "runtime-finalization-race".to_owned(),
            submitter_service: "steward-run".to_owned(),
            acting_user: Some("alice@example.com".to_owned()),
            acting_user_id: Some("usr_0123456789abcdef0123456789abcdef".to_owned()),
            owner: "alice@example.com".to_owned(),
            owner_user_id: Some("usr_0123456789abcdef0123456789abcdef".to_owned()),
            identity_binding_state: "bound".to_owned(),
            workflow: "code-review".to_owned(),
            workflow_name: None,
            workflow_version: None,
            workflow_digest: None,
            user_envelope_instance_id: None,
            user_envelope_revision: None,
            user_envelope_digest: None,
            internal_authority_id: None,
            internal_authority_version: None,
            internal_authority_digest: None,
            coding_agent_runtime: "base".to_owned(),
            runtime_uid: None,
            runtime_namespace: "team-a".to_owned(),
            runtime_name: "task-race".to_owned(),
            runtime_ownership: RuntimeOwnership::Provisioned,
            phase: TaskPhase::Submitted,
            runtime_spec: spec,
            agent_command: Vec::new(),
            execution_binding: None,
            envelope_revision: 3,
            orchestration_version: 2,
            orchestration_operation_id: Some(
                serde_json::from_value(serde_json::json!("00000000-0000-0000-0000-000000000001"))
                    .map_err(|error| format!("parse orchestration UID fixture: {error}"))?,
            ),
            candidate_digest: Some(format!("sha256:{}", "a".repeat(64))),
            service_envelope_digest: Some(format!("sha256:{}", "b".repeat(64))),
            original_admission_decision: Some("admit".to_owned()),
            original_admission_deltas: Some(Vec::new()),
            input_archive: None,
            output_archive: None,
            execute_requested: false,
            cancel_requested: false,
            finalize_requested: false,
            finalized: false,
            failure_reason: None,
        };
        let mut created = super::task_runtime_manifest(&task)
            .map_err(|error| format!("build runtime fixture: {error}"))?;
        if pending {
            created.spec.llms.clear();
            created.spec.tools.clear();
            created.spec.budget.monthly_limit = "0".to_owned();
            created.spec.budget.currency = "USD".to_owned();
            created.spec.ttl = Duration("24h".to_owned());
            created.metadata.annotations.get_or_insert_default().insert(
                PENDING_APPROVAL_ANNOTATION.to_owned(),
                super::spec_digest(&task.runtime_spec)
                    .map_err(|error| format!("digest pending runtime fixture: {error}"))?,
            );
        }
        created.metadata.uid = Some("created-runtime-uid".to_owned());
        let created_json = serde_json::to_vec(&created)
            .map_err(|error| format!("serialize runtime fixture: {error}"))?;
        let delete_body = Arc::new(Mutex::new(None));
        let delete_body_for_service = delete_body.clone();
        let client = Client::new(
            service_fn(move |request: Request<KubeBody>| {
                let created_json = created_json.clone();
                let delete_body = delete_body_for_service.clone();
                async move {
                    let method = request.method().clone();
                    let body = if matches!(method, Method::GET | Method::POST) {
                        created_json
                    } else if method == Method::DELETE {
                        let bytes = request.into_body().collect_bytes().await.map_err(|error| {
                            std::io::Error::other(format!(
                                "delete request body must be readable: {error}"
                            ))
                        })?;
                        *delete_body.lock().map_err(|_| {
                            std::io::Error::other("delete request lock was poisoned")
                        })? = Some(bytes.to_vec());
                        br#"{"apiVersion":"v1","kind":"Status","metadata":{},"status":"Success","code":200}"#
                            .to_vec()
                    } else {
                        br#"{"apiVersion":"v1","kind":"Status","metadata":{},"status":"Failure","reason":"NotFound","code":404}"#
                            .to_vec()
                    };
                    let mut response = Response::new(Body::from(body));
                    *response.status_mut() =
                        if matches!(method, Method::GET | Method::POST | Method::DELETE) {
                            StatusCode::OK
                        } else {
                            StatusCode::NOT_FOUND
                        };
                    Ok::<_, std::io::Error>(response)
                }
            }),
            "team-a",
        );

        Ok(TaskRuntimeCreationFixture {
            task,
            client,
            delete_body,
        })
    }

    #[tokio::test]
    async fn runtime_created_during_finalization_race_is_deleted_by_exact_uid() -> Result<(), String>
    {
        let TaskRuntimeCreationFixture {
            task,
            client,
            delete_body,
        } = task_runtime_creation_fixture()?;

        let result =
            create_task_runtime_inner(&client, &RejectingTaskRuntimeBindingStore, &task).await;
        assert!(
            matches!(
                result,
                Err(super::TaskControllerError::Store(
                    StoreError::InvalidTaskTransition
                ))
            ),
            "the no-resurrection transition failure must remain visible"
        );
        let body = delete_body
            .lock()
            .map_err(|_| "delete request lock was poisoned")?
            .clone()
            .ok_or_else(|| "binding loss orphaned the created AgentRuntime".to_owned())?;
        let body: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("parse delete request: {error}"))?;
        assert_eq!(
            body.pointer("/preconditions/uid")
                .and_then(serde_json::Value::as_str),
            Some("created-runtime-uid"),
            "cleanup must not delete a same-name replacement runtime"
        );
        Ok(())
    }

    #[tokio::test]
    async fn ambiguous_post_bind_error_preserves_the_durably_bound_runtime() -> Result<(), String> {
        let TaskRuntimeCreationFixture {
            task,
            client,
            delete_body,
        } = task_runtime_creation_fixture()?;
        let durable_runtime_uid = Arc::new(Mutex::new(None));
        let store = AmbiguousTaskRuntimeBindingStore {
            durable_runtime_uid: durable_runtime_uid.clone(),
        };

        let result = create_task_runtime_inner(&client, &store, &task).await;
        assert!(
            matches!(
                result,
                Err(super::TaskControllerError::Store(StoreError::Database(_)))
            ),
            "the ambiguous store failure must remain visible"
        );
        assert_eq!(
            durable_runtime_uid
                .lock()
                .map_err(|_| "durable binding fixture lock was poisoned")?
                .as_deref(),
            Some("created-runtime-uid"),
            "the fixture must model a committed binding before the read failure"
        );
        assert!(
            delete_body
                .lock()
                .map_err(|_| "delete request lock was poisoned")?
                .is_none(),
            "an ambiguous store error deleted a runtime that may be durably bound"
        );
        Ok(())
    }

    #[tokio::test]
    async fn finalization_recovers_an_unbound_deterministic_runtime() -> Result<(), String> {
        let TaskRuntimeCreationFixture {
            mut task, client, ..
        } = task_runtime_creation_fixture()?;
        task.finalize_requested = true;

        let runtime = task_runtime(&client, &RecoveringTaskRuntimeBindingStore, &task)
            .await
            .map_err(|error| format!("discover unbound deterministic runtime: {error}"))?;
        assert_eq!(
            runtime
                .as_ref()
                .and_then(|runtime| runtime.metadata.uid.as_deref()),
            Some("created-runtime-uid"),
            "finalization must recover the runtime created before an ambiguous bind failure"
        );
        assert_eq!(
            task_runtime_action(
                task.phase,
                task.runtime_ownership,
                task.execution_binding.as_ref(),
                task.finalize_requested,
                false,
                &task.runtime_spec,
                runtime.as_ref(),
            ),
            TaskRuntimeAction::DeleteRuntime
        );
        Ok(())
    }

    #[tokio::test]
    async fn finalization_ignores_an_unbound_same_name_runtime_with_different_state()
    -> Result<(), String> {
        let TaskRuntimeCreationFixture {
            mut task, client, ..
        } = task_runtime_creation_fixture()?;
        task.finalize_requested = true;
        task.runtime_spec.budget.monthly_limit = "999.00".to_owned();

        assert!(
            task_runtime(&client, &RecoveringTaskRuntimeBindingStore, &task)
                .await
                .map_err(|error| format!("inspect conflicting deterministic runtime: {error}"))?
                .is_none(),
            "finalization must not claim a same-name runtime with different server-authored state"
        );
        Ok(())
    }

    #[tokio::test]
    async fn finalization_recovers_an_unbound_pending_placeholder() -> Result<(), String> {
        let TaskRuntimeCreationFixture {
            mut task, client, ..
        } = task_runtime_creation_fixture_with_pending(true)?;
        task.finalize_requested = true;

        let runtime = task_runtime(&client, &RecoveringTaskRuntimeBindingStore, &task)
            .await
            .map_err(|error| format!("discover unbound pending placeholder: {error}"))?;
        assert_eq!(
            runtime
                .as_ref()
                .and_then(|runtime| runtime.metadata.uid.as_deref()),
            Some("created-runtime-uid"),
            "finalization must recover the exact pending-placeholder runtime"
        );
        assert_eq!(
            task_runtime_action(
                task.phase,
                task.runtime_ownership,
                task.execution_binding.as_ref(),
                task.finalize_requested,
                false,
                &task.runtime_spec,
                runtime.as_ref(),
            ),
            TaskRuntimeAction::DeleteRuntime
        );
        Ok(())
    }

    #[tokio::test]
    async fn finalization_deletes_the_exact_bound_runtime_after_revocation_restores_its_placeholder()
    -> Result<(), String> {
        let TaskRuntimeCreationFixture {
            mut task, client, ..
        } = task_runtime_creation_fixture_with_pending(true)?;
        task.runtime_uid = Some("created-runtime-uid".to_owned());
        task.finalize_requested = true;

        let runtime = task_runtime(&client, &RecoveringTaskRuntimeBindingStore, &task)
            .await
            .map_err(|error| format!("discover bound pending placeholder: {error}"))?;
        assert_eq!(
            task_runtime_action(
                task.phase,
                task.runtime_ownership,
                task.execution_binding.as_ref(),
                task.finalize_requested,
                true,
                &task.runtime_spec,
                runtime.as_ref(),
            ),
            TaskRuntimeAction::DeleteRuntime,
            "the exact bound UID must remain cleanable after its active grant is revoked"
        );
        Ok(())
    }

    #[test]
    fn provider_control_execution_rejects_every_binding_drift_dimension() {
        let persisted = ConnectionExecutionBindingSnapshot {
            artifact_trust_mode: "github-attestation".to_owned(),
            bridge_image_digest: "registry.example.test/bridge@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            mcp_gw_origin: "https://mcp-gw.example.test".to_owned(),
            mcp_gw_version: "0.3.2".to_owned(),
            namespace: "steward-test".to_owned(),
            runtime_class: "kata-qemu".to_owned(),
        };
        let current = ProviderControlExecutionBindings {
            artifact_trust_mode: persisted.artifact_trust_mode.clone(),
            bridge_image_digest: persisted.bridge_image_digest.clone(),
            mcp_gw_origin: persisted.mcp_gw_origin.clone(),
            mcp_gw_version: persisted.mcp_gw_version.clone(),
            namespace: persisted.namespace.clone(),
            runtime_class: persisted.runtime_class.clone(),
        };
        assert!(provider_control_bindings_match(&persisted, &current));

        for drifted in [
            ProviderControlExecutionBindings {
                artifact_trust_mode: "operator-pinned".to_owned(),
                ..current.clone()
            },
            ProviderControlExecutionBindings {
                bridge_image_digest: "registry.example.test/bridge@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
                ..current.clone()
            },
            ProviderControlExecutionBindings {
                mcp_gw_origin: "https://other-mcp-gw.example.test".to_owned(),
                ..current.clone()
            },
            ProviderControlExecutionBindings {
                mcp_gw_version: "0.3.1".to_owned(),
                ..current.clone()
            },
            ProviderControlExecutionBindings {
                namespace: "other-test".to_owned(),
                ..current.clone()
            },
            ProviderControlExecutionBindings {
                runtime_class: "other-runtime".to_owned(),
                ..current.clone()
            },
        ] {
            assert!(
                !provider_control_bindings_match(&persisted, &drifted),
                "a retry must not reinterpret any persisted connection-operation binding"
            );
        }
    }

    #[test]
    fn provider_control_execution_class_is_exact_and_does_not_capture_long_running_services()
    -> Result<(), String> {
        let acting_user = Email::parse("alice@example.com")
            .map_err(|error| format!("neutral email is invalid: {error}"))?;
        let mut governed = fixture().spec;
        governed.agent_type.name = "connections-bridge".to_owned();
        governed.principal = Principal::Service {
            name: "steward-connections".to_owned(),
            acting_user: Some(acting_user.clone()),
        };
        assert_eq!(
            sandbox_execution_class(&governed),
            SandboxExecutionClass::ProviderControl
        );

        let mut long_running = governed.clone();
        long_running.principal = Principal::Service {
            name: "steward-run".to_owned(),
            acting_user: Some(acting_user),
        };
        assert_eq!(
            sandbox_execution_class(&long_running),
            SandboxExecutionClass::Agent,
            "stable and long-running services must never inherit one-shot bridge behavior"
        );

        let mut ordinary = governed;
        ordinary.agent_type.name = "example-agent@1.0.0".to_owned();
        assert_eq!(
            sandbox_execution_class(&ordinary),
            SandboxExecutionClass::Agent
        );
        Ok(())
    }

    #[test]
    fn exact_governed_connection_runtime_uses_immutable_authority_without_service_envelope()
    -> Result<(), String> {
        let mut runtime = fixture();
        runtime.metadata.namespace = Some("steward-connections".to_owned());
        runtime.metadata.uid = Some("bridge-runtime-uid".to_owned());
        runtime.metadata.annotations = Some(std::collections::BTreeMap::from([(
            SERVICE_PRINCIPAL_ANNOTATION.to_owned(),
            "steward-connections".to_owned(),
        )]));
        runtime.spec.principal = Principal::Service {
            name: "steward-connections".to_owned(),
            acting_user: Some(Email::parse("alice@example.com")?),
        };
        runtime.spec.owner = Email::parse("alice@example.com")?;
        let user_id = CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?;
        runtime.spec.canonical_authority = Some(CanonicalAuthorityBinding::new(
            user_id.clone(),
            Some(user_id),
        )?);
        runtime.spec.agent_type.name = "connections-bridge".to_owned();
        runtime.spec.llms.clear();
        runtime.spec.tools = vec![steward_types::ToolGrant {
            provider: "github".to_owned(),
            resource: "provider-control".to_owned(),
            action: "status".to_owned(),
        }];
        runtime.spec.budget = Budget {
            monthly_limit: "0.00".to_owned(),
            single_run_limit: Some("0.00".to_owned()),
            currency: "USD".to_owned(),
        };
        runtime.spec.ttl = Duration("2m".to_owned());
        runtime.spec.runner = steward_types::RunnerRequirements {
            platforms: vec![steward_types::RunnerPlatform::Linux],
            memory: Some(steward_types::KubernetesQuantity("128Mi".to_owned())),
            compute: Some(steward_types::KubernetesQuantity("100m".to_owned())),
            storage: Some(steward_types::KubernetesQuantity("64Mi".to_owned())),
        };

        let bindings = ConnectionExecutionBindingSnapshot {
            artifact_trust_mode: "github-attestation".to_owned(),
            bridge_image_digest: "registry.example.test/steward-connections-bridge@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            mcp_gw_origin: "https://mcp-gw.example.test".to_owned(),
            mcp_gw_version: "0.3.2".to_owned(),
            namespace: "steward-connections".to_owned(),
            runtime_class: "kata-qemu".to_owned(),
        };
        let operation = ConnectionOperationRecord {
            operation_id: serde_json::from_value(serde_json::json!(
                "00000000-0000-0000-0000-000000000001"
            ))
            .map_err(|error| error.to_string())?,
            task_uid: serde_json::from_value(serde_json::json!(
                "00000000-0000-0000-0000-000000000001"
            ))
            .map_err(|error| error.to_string())?,
            canonical_user_id: "usr_0123456789abcdef0123456789abcdef".to_owned(),
            provider: "github".to_owned(),
            operation_kind: ConnectionOperationKind::Status,
            authority_id: "steward-connections".to_owned(),
            authority_version: 1,
            authority_digest: steward_connections_v1::AUTHORITY_DIGEST.to_owned(),
            runtime_spec_snapshot: runtime.spec.clone(),
            command_snapshot: vec![
                "/usr/local/bin/steward-connections-bridge".to_owned(),
                "--operation".to_owned(),
                "github.status".to_owned(),
                "--input".to_owned(),
                "request.json".to_owned(),
            ],
            bindings: bindings.clone(),
            idempotency_identity: "status:alice".to_owned(),
            uncached_status: false,
            operation_state: ConnectionOperationState::Provisioning,
            oauth_phase: ConnectionOAuthPhase::None,
            authorization_url: None,
            authorization_url_digest: None,
            flow_expires_at: None,
            cached_status: None,
            result: None,
            failure_category: None,
            finalization_state: "not_requested".to_owned(),
            cleanup_state: "not_started".to_owned(),
            cleanup_finding: None,
            response_deadline_at: "2026-09-01T00:00:40Z".to_owned(),
            task_phase: TaskPhase::Submitted,
            runtime_uid: Some("bridge-runtime-uid".to_owned()),
            output_archive: None,
            finalize_requested: false,
            finalized: false,
        };
        let current = ProviderControlExecutionBindings {
            artifact_trust_mode: bindings.artifact_trust_mode.clone(),
            bridge_image_digest: bindings.bridge_image_digest.clone(),
            mcp_gw_origin: bindings.mcp_gw_origin.clone(),
            mcp_gw_version: bindings.mcp_gw_version.clone(),
            namespace: bindings.namespace.clone(),
            runtime_class: bindings.runtime_class.clone(),
        };

        assert!(
            matches!(
                connection_operation_authority_action(&runtime, &operation, Some(&current))
                    .map_err(|error| format!("immutable authority evaluation failed: {error:?}"))?,
                AuthorityAction::Continue
            ),
            "an exact persisted connection operation must pass immutable internal admission without a mutable service envelope"
        );
        let mut unrelated_runtime = runtime.clone();
        unrelated_runtime.metadata.uid = Some("long-running-runtime-uid".to_owned());
        assert!(
            matches!(
                connection_operation_authority_action(
                    &unrelated_runtime,
                    &operation,
                    Some(&current)
                )
                .map_err(|error| format!("exact-UID rejection failed: {error:?}"))?,
                AuthorityAction::Suspend
            ),
            "internal authority must never attach to a different or long-running runtime UID"
        );
        let mut ordinary_tool_runtime = runtime.clone();
        ordinary_tool_runtime.spec.tools[0].resource = "repository".to_owned();
        ordinary_tool_runtime.spec.tools[0].action = "get_file_contents".to_owned();
        assert!(
            matches!(
                connection_operation_authority_action(
                    &ordinary_tool_runtime,
                    &operation,
                    Some(&current)
                )
                .map_err(|error| format!("ordinary-tool rejection failed: {error:?}"))?,
                AuthorityAction::Suspend
            ),
            "provider-control authority must not authorize an ordinary GitHub MCP tool"
        );
        assert!(
            matches!(
                connection_operation_authority_action(&runtime, &operation, None)
                    .map_err(|error| format!("missing-binding rejection failed: {error:?}"))?,
                AuthorityAction::Suspend
            ),
            "missing current execution bindings must fail closed"
        );
        Ok(())
    }

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
                None,
                false,
                true,
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
                None,
                false,
                true,
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
                None,
                false,
                true,
                &runtime.spec,
                Some(&runtime),
            ),
            TaskRuntimeAction::Release
        );

        assert_eq!(
            task_runtime_action(
                TaskPhase::Cancelled,
                RuntimeOwnership::Provisioned,
                None,
                true,
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
                None,
                true,
                true,
                &runtime.spec,
                Some(&runtime),
            ),
            TaskRuntimeAction::MarkFinalized
        );
        let resident = TaskExecutionBinding::Resident(ResidentExecutionBinding {
            schema_version: TASK_EXECUTION_BINDING_SCHEMA_VERSION.to_owned(),
            binding_id: "resident-agent-instance-v1".to_owned(),
            binding_digest: format!("sha256:{}", "a".repeat(64)),
            owner_user_id: CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?,
            agent_instance_id: "agent-instance-01".to_owned(),
            agent_instance_revision: 1,
            runtime_uid: RuntimeId("runtime-uid-a".to_owned()),
            runtime_spec_digest: format!("sha256:{}", "b".repeat(64)),
            standing_authority_digest: format!("sha256:{}", "c".repeat(64)),
            deployment_binding_digest: format!("sha256:{}", "d".repeat(64)),
            freshness_generation: 1,
        });
        assert_eq!(
            task_runtime_action(
                TaskPhase::Cancelled,
                RuntimeOwnership::Provisioned,
                Some(&resident),
                true,
                true,
                &runtime.spec,
                Some(&runtime),
            ),
            TaskRuntimeAction::MarkFinalized,
            "Task completion must not delete an AgentInstance-owned resident runtime"
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
                None,
                true,
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
                None,
                false,
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
                execution_binding: None,
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
                execution_binding: None,
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

    #[tokio::test]
    async fn suspension_requeues_promptly_while_sandbox_deletion_is_pending() -> Result<(), String>
    {
        let runtime = fixture();
        let (client, _) = successful_cleanup_client(&runtime)?;
        let api = kube::Api::<AgentRuntime>::namespaced(client.clone(), "team-a");

        let action = suspend_runtime(&runtime, &api, &ProvisioningDeleteRuntime, None)
            .await
            .map_err(|error| {
                format!("pending sandbox deletion must remain reconcilable: {error}")
            })?;

        assert_eq!(
            action,
            Action::requeue(StdDuration::from_secs(2)),
            "a budget-exhausted runtime in Terminating must be polled promptly; a 60-second retry can make the required suspension transition miss its deadline"
        );
        Ok(())
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
                    single_run_limit: None,
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
                    single_run_limit: None,
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
    async fn exact_uid_cleanup_does_not_depend_on_a_valid_execution_binding_annotation()
    -> Result<(), String> {
        let mut runtime = fixture();
        runtime.metadata.annotations.get_or_insert_default().insert(
            steward_types::TASK_EXECUTION_BINDING_ANNOTATION.to_owned(),
            "{".to_owned(),
        );
        let refs = RuntimeRefs {
            workspace: Some("workspace-team-a".to_owned()),
            sandbox: Some("sandbox-runtime-uid-a".to_owned()),
            litellm_key: None,
        };
        runtime.status = Some(AgentRuntimeStatus {
            phase: Phase::Running,
            observed_generation: 3,
            spec_digest: "previously-active-digest".to_owned(),
            refs: refs.clone(),
            conditions: Vec::new(),
            spend: None,
        });
        let sandbox_runtime = FakeSandboxRuntime {
            state: Mutex::new(FakeState {
                created: 1,
                deleted: 0,
                refs: Some(refs),
            }),
        };

        let decision = reconcile_once(&runtime, ReconcileIntent::Delete, &sandbox_runtime)
            .await
            .map_err(|error| format!("exact UID cleanup was blocked: {error:?}"))?;
        assert_eq!(decision, ReconcileDecision::Deleted);
        assert_eq!(
            sandbox_runtime
                .state
                .lock()
                .map_err(|_| "fake runtime state lock was poisoned")?
                .deleted,
            1,
            "desired-state corruption must not preserve an exact UID's external authority"
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

    #[tokio::test]
    async fn restored_pending_placeholder_removes_previously_provisioned_sandbox_authority()
    -> Result<(), String> {
        let mut runtime = fixture();
        runtime.metadata.annotations.get_or_insert_default().insert(
            PENDING_APPROVAL_ANNOTATION.to_owned(),
            "request-digest".to_owned(),
        );
        let refs = RuntimeRefs {
            workspace: Some("workspace-team-a".to_owned()),
            sandbox: Some("sandbox-runtime-uid-a".to_owned()),
            litellm_key: None,
        };
        runtime.status = Some(AgentRuntimeStatus {
            phase: Phase::Running,
            observed_generation: 3,
            spec_digest: "previously-active-digest".to_owned(),
            refs: refs.clone(),
            conditions: Vec::new(),
            spend: None,
        });
        let sandbox_runtime = FakeSandboxRuntime {
            state: Mutex::new(FakeState {
                created: 1,
                deleted: 0,
                refs: Some(refs),
            }),
        };

        let decision = reconcile_once(&runtime, ReconcileIntent::Ensure, &sandbox_runtime)
            .await
            .map_err(|error| format!("restored pending reconcile failed: {error:?}"))?;

        assert_eq!(
            decision,
            ReconcileDecision::Deleted,
            "revoked authority must remove an already-provisioned sandbox before the placeholder returns to Pending"
        );
        assert_eq!(
            sandbox_runtime
                .state
                .lock()
                .map_err(|_| "fake runtime state lock was poisoned")?
                .deleted,
            1,
            "the pending fast path must not retain the sandbox that held the revoked provider"
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
    use steward_types::{
        AgentRuntime, Budget, Duration, ModelRef, TASK_EXECUTION_BINDING_ANNOTATION,
    };
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
                        single_run_limit: None,
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
    async fn webhook_rejects_caller_authored_task_execution_bindings() -> Result<(), String> {
        let mut value = admission_review_value();
        value["request"]["object"]["spec"]["budget"]["monthlyLimit"] = serde_json::json!("100.00");
        value["request"]["oldObject"]["spec"]["budget"]["monthlyLimit"] =
            serde_json::json!("100.00");
        value["request"]["object"]["metadata"]["annotations"][TASK_EXECUTION_BINDING_ANNOTATION] =
            serde_json::json!("caller-selected");
        let review = serde_json::from_value::<AdmissionReview<AgentRuntime>>(value)
            .map_err(|error| format!("failed to construct binding injection review: {error}"))?;
        let request: AdmissionRequest<AgentRuntime> = review
            .try_into()
            .map_err(|error| format!("failed to read binding injection review: {error}"))?;

        let response = validate_admission(&request, &fake_envelopes()).await;

        assert!(
            !response.allowed,
            "a caller-authored deployment binding was admitted"
        );
        assert_eq!(
            response.result.message,
            "task execution binding is immutable"
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
