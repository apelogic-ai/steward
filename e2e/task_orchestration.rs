use std::convert::Infallible;
use std::env;
use std::error::Error;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{Method, Response, StatusCode, header};
use axum::routing::any;
use kube::{Client, ResourceExt};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{postgres::PgPoolOptions, types::Uuid};
use steward_admission::{AdmissionDecision, Envelope, EnvelopeSpec};
use steward_controller::reconcile_task_orchestration_work_item;
use steward_ports::{
    PortError, SandboxTaskObservation, SandboxTaskRequest, SandboxTaskRuntime, TaskAttemptId,
};
use steward_store::{PgStore, StoreError, TaskOrchestrationState, TaskReservationRequest};
use steward_types::{
    AgentRuntime, AgentRuntimeSpec, AgentRuntimeStatus, AgentType, Budget,
    CanonicalAuthorityBinding, Duration, Email, ModelRef, OrganizationId,
    OrganizationIdentityPolicy, Phase, Principal, RunnerRequirements, RuntimeOwnership,
    RuntimeRefs, TaskPhase, ToolGrant,
};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

const RUNTIME_PATH_PREFIX: &str =
    "/apis/agents.apelogic.ai/v1alpha1/namespaces/steward-test/agentruntimes";

#[derive(Clone, Default)]
struct AmbiguousKubernetes {
    runtime: Arc<Mutex<Option<AgentRuntime>>>,
    created: Arc<Mutex<Vec<AgentRuntime>>>,
    create_calls: Arc<AtomicUsize>,
    replace_calls: Arc<AtomicUsize>,
    fail_first_create_response: Arc<AtomicBool>,
    delete_preconditions: Arc<Mutex<Vec<String>>>,
    replace_name_after_delete: Arc<AtomicBool>,
}

struct ServerGuard(JoinHandle<Result<(), io::Error>>);

impl Drop for ServerGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Clone, Default)]
struct AmbiguousTaskRuntime {
    starts: Arc<AtomicUsize>,
    terminal_observed: Arc<AtomicBool>,
    fail_next_observation: Arc<AtomicBool>,
}

impl SandboxTaskRuntime for AmbiguousTaskRuntime {
    async fn start_task(
        &self,
        _attempt_id: &TaskAttemptId,
        _request: &SandboxTaskRequest,
        _input_archive: &[u8],
    ) -> Result<SandboxTaskObservation, PortError> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        Err(PortError::Failed {
            reason: "execution start response was lost".to_owned(),
        })
    }

    async fn observe_task(
        &self,
        attempt_id: &TaskAttemptId,
        _request: &SandboxTaskRequest,
    ) -> Result<SandboxTaskObservation, PortError> {
        if self.fail_next_observation.swap(false, Ordering::SeqCst) {
            return Err(PortError::Failed {
                reason: "temporary observation transport failure".to_owned(),
            });
        }
        if self.terminal_observed.load(Ordering::SeqCst) {
            Ok(SandboxTaskObservation::Failed {
                adapter_observation_id: attempt_id.0.clone(),
                reason: "late durable process exit".to_owned(),
            })
        } else {
            Ok(SandboxTaskObservation::Absent)
        }
    }

    async fn cancel_task(
        &self,
        _attempt_id: &TaskAttemptId,
        _request: &SandboxTaskRequest,
    ) -> Result<SandboxTaskObservation, PortError> {
        Ok(SandboxTaskObservation::OutcomeUnknown {
            reason: "attempt-scoped cancellation is unprovable".to_owned(),
        })
    }
}

#[tokio::test]
async fn two_reconcilers_recover_ambiguous_effects_without_rebinding_or_replay()
-> Result<(), Box<dyn Error>> {
    install_rustls_crypto_provider()?;
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for orchestration fault injection")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let service = format!("orchestrator-fault-{suffix}");
    let identity = store
        .register_canonical_identity(
            &OrganizationIdentityPolicy::new(
                "https://accounts.google.com",
                "example.com",
                OrganizationId::parse("org_example")?,
            )?
            .validate(
                "https://accounts.google.com",
                &format!("orchestrator-subject-{suffix}"),
                "example.com",
                &format!("alice-{suffix}@example.com"),
                true,
            )?,
            "test-bootstrap",
        )
        .await?;
    let envelope = Envelope {
        revision: 1,
        spec: EnvelopeSpec {
            llms: vec![ModelRef {
                provider: "example".to_owned(),
                model: "model-a".to_owned(),
            }],
            tools: vec![ToolGrant {
                provider: "example".to_owned(),
                resource: "repository".to_owned(),
                action: "read".to_owned(),
            }],
            budget: Budget {
                monthly_limit: "100.00".to_owned(),
                single_run_limit: Some("10.00".to_owned()),
                currency: "USD".to_owned(),
            },
            ttl: Duration("1h".to_owned()),
            runner: RunnerRequirements::default(),
        },
    };
    store
        .insert_service_envelope(&service, &envelope, "admin@example.com")
        .await?;
    let spec = AgentRuntimeSpec {
        principal: Principal::Service {
            name: service.clone(),
            acting_user: Some(Email("alice@example.com".to_owned())),
        },
        owner: Email("alice@example.com".to_owned()),
        canonical_authority: Some(CanonicalAuthorityBinding::new(
            identity.user_id.clone(),
            Some(identity.user_id.clone()),
        )?),
        agent_type: AgentType {
            name: "example-agent".to_owned(),
        },
        llms: envelope.spec.llms.clone(),
        tools: envelope.spec.tools.clone(),
        budget: envelope.spec.budget.clone(),
        ttl: envelope.spec.ttl.clone(),
        runner: envelope.spec.runner.clone(),
        bindings: None,
    };
    let task_uid = Uuid::new_v4();
    let operation_id = Uuid::new_v4();
    let runtime_name = format!("task-{}", operation_id.simple());
    let candidate_digest = digest(serde_json::to_value(&spec))?;
    let envelope_digest = digest(serde_json::to_value(&envelope))?;
    let inert_digest = manifest_digest(
        task_uid,
        operation_id,
        &runtime_name,
        &inert_spec(&spec, &envelope),
        "inert",
    )?;
    let active_digest = manifest_digest(task_uid, operation_id, &runtime_name, &spec, "active")?;
    let idempotency_key = format!("orchestration-fault-{suffix}");
    let agent_command = ["example-agent".to_owned(), "run".to_owned()];
    let admission_decision = AdmissionDecision::Admit;
    let reservation = store
        .reserve_task(&TaskReservationRequest {
            task_uid,
            operation_id,
            idempotency_key: &idempotency_key,
            submitter_service: &service,
            acting_user: Some("alice@example.com"),
            acting_user_id: Some(identity.user_id.as_str()),
            owner: "alice@example.com",
            owner_user_id: identity.user_id.as_str(),
            workflow: "fault-injection",
            workflow_name: None,
            workflow_version: None,
            workflow_digest: None,
            user_envelope_instance_id: None,
            user_envelope_revision: None,
            user_envelope_digest: None,
            coding_agent_runtime: "example-agent",
            runtime_uid: None,
            runtime_namespace: "steward-test",
            runtime_name: &runtime_name,
            runtime_ownership: RuntimeOwnership::Provisioned,
            runtime_spec: &spec,
            agent_command: &agent_command,
            execution_binding: None,
            envelope_revision: envelope.revision,
            service_envelope: &envelope,
            service_envelope_digest: &envelope_digest,
            candidate_digest: &candidate_digest,
            admission_decision: &admission_decision,
            inert_manifest_digest: &inert_digest,
            active_manifest_digest: &active_digest,
        })
        .await?;
    store
        .put_task_inputs(
            task_uid,
            &service,
            identity.user_id.as_str(),
            b"neutral input archive",
        )
        .await?;
    store
        .request_task_execution(task_uid, &service, identity.user_id.as_str())
        .await?;

    let kubernetes = AmbiguousKubernetes::default();
    kubernetes
        .fail_first_create_response
        .store(true, Ordering::SeqCst);
    let (client, _server) = kubernetes_client(kubernetes.clone()).await?;
    let task_runtime = AmbiguousTaskRuntime::default();

    reconcile_current(&client, &task_runtime, &store, task_uid).await?;
    assert_eq!(
        operation(&store, task_uid).await?.state,
        TaskOrchestrationState::RuntimeCreatePending
    );

    let create_work = current_work(&store, task_uid).await?;
    let (left, right) = tokio::join!(
        reconcile_task_orchestration_work_item(&client, &task_runtime, &store, &create_work),
        reconcile_task_orchestration_work_item(&client, &task_runtime, &store, &create_work),
    );
    left?;
    right?;
    let observed = operation(&store, task_uid).await?;
    assert_eq!(observed.state, TaskOrchestrationState::RuntimeObserved);
    assert_eq!(observed.runtime_uid.as_deref(), Some("runtime-uid-a"));
    assert!(kubernetes.create_calls.load(Ordering::SeqCst) >= 1);
    {
        let created = kubernetes
            .created
            .lock()
            .map_err(|_| io::Error::other("created runtime fixture was poisoned"))?;
        assert_eq!(
            created.len(),
            1,
            "ambiguous creates must converge on one CR"
        );
        assert!(created[0].spec.llms.is_empty() && created[0].spec.tools.is_empty());
        assert_eq!(created[0].spec.budget.monthly_limit, "0");
        assert_eq!(
            created[0].spec.budget.single_run_limit.as_deref(),
            Some("0")
        );
        assert_eq!(
            created[0]
                .annotations()
                .get("agents.apelogic.ai/orchestration-id"),
            Some(&operation_id.to_string())
        );
    }

    reconcile_current(&client, &task_runtime, &store, task_uid).await?;
    assert_eq!(
        operation(&store, task_uid).await?.state,
        TaskOrchestrationState::ActivationPending
    );
    reconcile_current(&client, &task_runtime, &store, task_uid).await?;
    assert!(
        operation(&store, task_uid)
            .await?
            .activation_effect_authorized_at
            .is_some()
    );
    reconcile_current(&client, &task_runtime, &store, task_uid).await?;
    let activated_runtime = kubernetes
        .runtime
        .lock()
        .map_err(|_| io::Error::other("runtime fixture was poisoned"))?
        .clone()
        .ok_or_else(|| io::Error::other("active runtime fixture is absent"))?;
    assert_eq!(activated_runtime.spec, spec);
    assert_eq!(
        activated_runtime
            .annotations()
            .get("agents.apelogic.ai/runtime-mode"),
        Some(&"active".to_owned())
    );
    assert_eq!(
        activated_runtime
            .annotations()
            .get("agents.apelogic.ai/manifest-digest"),
        Some(&active_digest)
    );
    assert_eq!(
        activated_runtime
            .annotations()
            .get("agents.apelogic.ai/service-principal"),
        Some(&service)
    );
    assert_eq!(
        activated_runtime
            .annotations()
            .get("agents.apelogic.ai/task-uid"),
        Some(&task_uid.to_string())
    );
    assert!(
        !activated_runtime
            .annotations()
            .contains_key("agents.apelogic.ai/task-execution-binding")
    );
    assert!(
        !activated_runtime
            .annotations()
            .contains_key("agents.apelogic.ai/pending-approval")
    );
    let status = activated_runtime
        .status
        .as_ref()
        .ok_or_else(|| io::Error::other("active runtime status is absent"))?;
    assert_eq!(status.phase, Phase::Running);
    assert_eq!(status.observed_generation, 2);
    assert_eq!(status.spec_digest, runtime_spec_digest(&spec)?);
    assert_eq!(
        operation(&store, task_uid).await?.state,
        TaskOrchestrationState::Active
    );
    assert_eq!(
        store
            .task(task_uid)
            .await?
            .ok_or(StoreError::TaskNotFound)?
            .phase,
        TaskPhase::Queued
    );

    reconcile_current(&client, &task_runtime, &store, task_uid).await?;
    assert_eq!(
        task_runtime.starts.load(Ordering::SeqCst),
        0,
        "reserving an attempt must not cross the adapter boundary"
    );
    let reserved_attempt = store
        .task_execution_attempt(task_uid)
        .await?
        .ok_or_else(|| io::Error::other("execution attempt was not reserved"))?;
    assert!(reserved_attempt.start_invoked_at.is_none());
    reconcile_current(&client, &task_runtime, &store, task_uid).await?;
    assert_eq!(
        task_runtime.starts.load(Ordering::SeqCst),
        0,
        "authorizing the start crossing must remain a durable step before invocation"
    );
    assert!(
        store
            .task_execution_attempt(task_uid)
            .await?
            .is_some_and(|attempt| attempt.start_invoked_at.is_some())
    );
    reconcile_current(&client, &task_runtime, &store, task_uid).await?;
    assert_eq!(task_runtime.starts.load(Ordering::SeqCst), 1);
    assert_eq!(
        operation(&store, task_uid).await?.state,
        TaskOrchestrationState::CleanupPending
    );
    kubernetes
        .replace_name_after_delete
        .store(true, Ordering::SeqCst);
    reconcile_current(&client, &task_runtime, &store, task_uid).await?;
    reconcile_current(&client, &task_runtime, &store, task_uid).await?;
    let finalized = store
        .task(task_uid)
        .await?
        .ok_or(StoreError::TaskNotFound)?;
    assert!(finalized.finalized);
    assert_eq!(finalized.phase, TaskPhase::Failed);
    assert_eq!(
        finalized.failure_reason.as_deref(),
        Some("execution_outcome_unknown")
    );
    assert_eq!(task_runtime.starts.load(Ordering::SeqCst), 1);
    assert_eq!(
        kubernetes
            .delete_preconditions
            .lock()
            .map_err(|_| io::Error::other("delete fixture was poisoned"))?
            .as_slice(),
        ["runtime-uid-a"]
    );
    assert_eq!(
        kubernetes
            .runtime
            .lock()
            .map_err(|_| io::Error::other("runtime fixture was poisoned"))?
            .as_ref()
            .and_then(|runtime| runtime.metadata.uid.as_deref()),
        Some("runtime-uid-replacement"),
        "same-name replacement infrastructure must remain untouched"
    );

    *kubernetes
        .runtime
        .lock()
        .map_err(|_| io::Error::other("runtime fixture was poisoned"))? = None;
    let cleanup_task_uid = Uuid::new_v4();
    let cleanup_operation_id = Uuid::new_v4();
    let cleanup_runtime_name = format!("task-{}", cleanup_operation_id.simple());
    let cleanup_candidate_digest = digest(serde_json::to_value(&spec))?;
    let cleanup_inert_digest = manifest_digest(
        cleanup_task_uid,
        cleanup_operation_id,
        &cleanup_runtime_name,
        &inert_spec(&spec, &envelope),
        "inert",
    )?;
    let cleanup_active_digest = manifest_digest(
        cleanup_task_uid,
        cleanup_operation_id,
        &cleanup_runtime_name,
        &spec,
        "active",
    )?;
    let cleanup_key = format!("finalize-before-observation-{suffix}");
    store
        .reserve_task(&TaskReservationRequest {
            task_uid: cleanup_task_uid,
            operation_id: cleanup_operation_id,
            idempotency_key: &cleanup_key,
            submitter_service: &service,
            acting_user: Some("alice@example.com"),
            acting_user_id: Some(identity.user_id.as_str()),
            owner: "alice@example.com",
            owner_user_id: identity.user_id.as_str(),
            workflow: "finalize-before-observation",
            workflow_name: None,
            workflow_version: None,
            workflow_digest: None,
            user_envelope_instance_id: None,
            user_envelope_revision: None,
            user_envelope_digest: None,
            coding_agent_runtime: "example-agent",
            runtime_uid: None,
            runtime_namespace: "steward-test",
            runtime_name: &cleanup_runtime_name,
            runtime_ownership: RuntimeOwnership::Provisioned,
            runtime_spec: &spec,
            agent_command: &agent_command,
            execution_binding: None,
            envelope_revision: envelope.revision,
            service_envelope: &envelope,
            service_envelope_digest: &envelope_digest,
            candidate_digest: &cleanup_candidate_digest,
            admission_decision: &admission_decision,
            inert_manifest_digest: &cleanup_inert_digest,
            active_manifest_digest: &cleanup_active_digest,
        })
        .await?;
    reconcile_current(&client, &task_runtime, &store, cleanup_task_uid).await?;
    assert_eq!(
        operation(&store, cleanup_task_uid).await?.state,
        TaskOrchestrationState::RuntimeCreatePending
    );
    store
        .request_task_finalization(cleanup_task_uid, &service, identity.user_id.as_str())
        .await?;
    reconcile_current(&client, &task_runtime, &store, cleanup_task_uid).await?;
    assert_eq!(
        operation(&store, cleanup_task_uid).await?.state,
        TaskOrchestrationState::CleanupPending
    );
    let create_calls_before_cleanup = kubernetes.create_calls.load(Ordering::SeqCst);
    reconcile_current(&client, &task_runtime, &store, cleanup_task_uid).await?;
    let cleanup_observed = operation(&store, cleanup_task_uid).await?;
    assert_eq!(
        cleanup_observed.state,
        TaskOrchestrationState::CleanupPending
    );
    assert_eq!(
        cleanup_observed.runtime_uid.as_deref(),
        Some("runtime-uid-a")
    );
    assert_eq!(
        kubernetes.create_calls.load(Ordering::SeqCst),
        create_calls_before_cleanup + 1,
        "cleanup must complete the authorized inert create before it can prove absence"
    );
    assert!(
        !store
            .task(cleanup_task_uid)
            .await?
            .ok_or(StoreError::TaskNotFound)?
            .finalized
    );
    reconcile_current(&client, &task_runtime, &store, cleanup_task_uid).await?;
    reconcile_current(&client, &task_runtime, &store, cleanup_task_uid).await?;
    assert!(
        store
            .task(cleanup_task_uid)
            .await?
            .ok_or(StoreError::TaskNotFound)?
            .finalized
    );
    let delete_count_before_adopted = kubernetes
        .delete_preconditions
        .lock()
        .map_err(|_| io::Error::other("delete fixture was poisoned"))?
        .len();

    let adopted_task_uid = Uuid::new_v4();
    let adopted_operation_id = Uuid::new_v4();
    let adopted_runtime_name = format!("shared-runtime-{}", adopted_operation_id.simple());
    let adopted_runtime_uid = "shared-runtime-uid";
    let mut shared_runtime = AgentRuntime::new(&adopted_runtime_name, spec.clone());
    shared_runtime.metadata.namespace = Some("steward-test".to_owned());
    shared_runtime.metadata.uid = Some(adopted_runtime_uid.to_owned());
    shared_runtime.metadata.resource_version = Some("shared-resource-version".to_owned());
    shared_runtime.metadata.generation = Some(7);
    shared_runtime.status = Some(AgentRuntimeStatus {
        phase: Phase::Running,
        observed_generation: 7,
        spec_digest: runtime_spec_digest(&spec)?,
        refs: RuntimeRefs {
            workspace: Some("shared-workspace".to_owned()),
            sandbox: Some("shared-sandbox".to_owned()),
            litellm_key: None,
        },
        conditions: Vec::new(),
        spend: None,
    });
    *kubernetes
        .runtime
        .lock()
        .map_err(|_| io::Error::other("runtime fixture was poisoned"))? = Some(shared_runtime);
    let adopted_key = format!("adopted-finalization-{suffix}");
    let adopted = store
        .reserve_task(&TaskReservationRequest {
            task_uid: adopted_task_uid,
            operation_id: adopted_operation_id,
            idempotency_key: &adopted_key,
            submitter_service: &service,
            acting_user: Some("alice@example.com"),
            acting_user_id: Some(identity.user_id.as_str()),
            owner: "alice@example.com",
            owner_user_id: identity.user_id.as_str(),
            workflow: "shared-runtime-finalization",
            workflow_name: None,
            workflow_version: None,
            workflow_digest: None,
            user_envelope_instance_id: None,
            user_envelope_revision: None,
            user_envelope_digest: None,
            coding_agent_runtime: "example-agent",
            runtime_uid: Some(adopted_runtime_uid),
            runtime_namespace: "steward-test",
            runtime_name: &adopted_runtime_name,
            runtime_ownership: RuntimeOwnership::Adopted,
            runtime_spec: &spec,
            agent_command: &agent_command,
            execution_binding: None,
            envelope_revision: envelope.revision,
            service_envelope: &envelope,
            service_envelope_digest: &envelope_digest,
            candidate_digest: &candidate_digest,
            admission_decision: &admission_decision,
            inert_manifest_digest: &inert_digest,
            active_manifest_digest: &active_digest,
        })
        .await?;
    assert!(adopted.inserted);
    let ready_status = {
        kubernetes
            .runtime
            .lock()
            .map_err(|_| io::Error::other("runtime fixture was poisoned"))?
            .as_mut()
            .ok_or_else(|| io::Error::other("shared runtime missing"))?
            .status
            .take()
    };
    reconcile_current(&client, &task_runtime, &store, adopted_task_uid).await?;
    assert_eq!(
        operation(&store, adopted_task_uid).await?.state,
        TaskOrchestrationState::IntentRecorded,
        "readiness lag is not an immutable identity conflict"
    );
    kubernetes
        .runtime
        .lock()
        .map_err(|_| io::Error::other("runtime fixture was poisoned"))?
        .as_mut()
        .ok_or_else(|| io::Error::other("shared runtime missing"))?
        .status = ready_status.clone();
    reconcile_current(&client, &task_runtime, &store, adopted_task_uid).await?;
    assert_eq!(
        operation(&store, adopted_task_uid).await?.state,
        TaskOrchestrationState::RuntimeObserved,
        "an exact adopted runtime must be observed without claiming ownership"
    );
    let replace_count_before_adopted_activation = kubernetes.replace_calls.load(Ordering::SeqCst);
    reconcile_current(&client, &task_runtime, &store, adopted_task_uid).await?;
    reconcile_current(&client, &task_runtime, &store, adopted_task_uid).await?;
    reconcile_current(&client, &task_runtime, &store, adopted_task_uid).await?;
    assert_eq!(
        operation(&store, adopted_task_uid).await?.state,
        TaskOrchestrationState::Active,
        "an exact ready adopted runtime must become active by observation"
    );
    assert_eq!(
        kubernetes.replace_calls.load(Ordering::SeqCst),
        replace_count_before_adopted_activation,
        "adopted activation must not replace or otherwise mutate the shared runtime"
    );
    store
        .put_task_inputs(
            adopted_task_uid,
            &service,
            identity.user_id.as_str(),
            b"adopted cancellation input",
        )
        .await?;
    store
        .request_task_execution(adopted_task_uid, &service, identity.user_id.as_str())
        .await?;
    reconcile_current(&client, &task_runtime, &store, adopted_task_uid).await?;
    let adopted_attempt = store
        .task_execution_attempt(adopted_task_uid)
        .await?
        .ok_or_else(|| io::Error::other("adopted execution attempt was not reserved"))?;
    sqlx::query(
        "UPDATE task_execution_attempts \
         SET start_invoked_at = now() - interval '3 minutes', \
             start_observation_deadline_at = now() - interval '1 minute', \
             generation = generation + 1, updated_at = now() \
         WHERE attempt_id = $1 AND generation = $2 AND state = 'start_pending'",
    )
    .bind(adopted_attempt.attempt_id)
    .bind(adopted_attempt.generation)
    .execute(store.pool())
    .await?;
    let accepted_attempt = store
        .task_execution_attempt(adopted_task_uid)
        .await?
        .ok_or(StoreError::TaskNotFound)?;
    store
        .record_task_execution_observation(
            accepted_attempt.attempt_id,
            accepted_attempt.generation,
            steward_store::TaskExecutionObservation::Accepted {
                adapter_observation_id: &accepted_attempt.attempt_id.to_string(),
            },
            "controller-a",
        )
        .await?;
    let before_transport_error = store.task(adopted_task_uid).await?;
    let attempt_before_transport_error = store.task_execution_attempt(adopted_task_uid).await?;
    task_runtime
        .fail_next_observation
        .store(true, Ordering::SeqCst);
    assert!(
        reconcile_current(&client, &task_runtime, &store, adopted_task_uid)
            .await
            .is_err(),
        "a transient observation failure must remain retryable"
    );
    assert!(!task_runtime.fail_next_observation.load(Ordering::SeqCst));
    assert_eq!(store.task(adopted_task_uid).await?, before_transport_error);
    assert_eq!(
        store.task_execution_attempt(adopted_task_uid).await?,
        attempt_before_transport_error,
        "a transport error must not manufacture outcome_unknown or retire the lease"
    );
    store
        .request_task_finalization(adopted_task_uid, &service, identity.user_id.as_str())
        .await?;
    kubernetes
        .runtime
        .lock()
        .map_err(|_| io::Error::other("runtime fixture was poisoned"))?
        .as_mut()
        .ok_or_else(|| io::Error::other("shared runtime fixture is absent"))?
        .status = None;
    reconcile_current(&client, &task_runtime, &store, adopted_task_uid).await?;
    assert_eq!(
        operation(&store, adopted_task_uid).await?.state,
        TaskOrchestrationState::CleanupPending,
        "expired cancellation must enter cleanup when runtime references disappeared"
    );
    reconcile_current(&client, &task_runtime, &store, adopted_task_uid).await?;
    assert!(
        store
            .task(adopted_task_uid)
            .await?
            .ok_or(StoreError::TaskNotFound)?
            .finalized
    );
    assert_eq!(
        kubernetes
            .delete_preconditions
            .lock()
            .map_err(|_| io::Error::other("delete fixture was poisoned"))?
            .len(),
        delete_count_before_adopted,
        "finalizing an adopted runtime must not issue a Kubernetes delete"
    );
    assert_eq!(
        kubernetes
            .runtime
            .lock()
            .map_err(|_| io::Error::other("runtime fixture was poisoned"))?
            .as_ref()
            .and_then(|runtime| runtime.metadata.uid.as_deref()),
        Some(adopted_runtime_uid),
        "shared infrastructure must remain after Task finalization"
    );
    let before_retirement = store
        .task(adopted_task_uid)
        .await?
        .ok_or(StoreError::TaskNotFound)?;
    let unknown_attempt = store
        .task_execution_attempt(adopted_task_uid)
        .await?
        .ok_or(StoreError::TaskNotFound)?;
    assert!(
        store
            .task_orchestration_work_items()
            .await?
            .iter()
            .any(|work| work.task.task_uid == adopted_task_uid),
        "finalized shared-runtime quarantine must remain observable after restart"
    );
    kubernetes
        .runtime
        .lock()
        .map_err(|_| io::Error::other("runtime fixture was poisoned"))?
        .as_mut()
        .ok_or_else(|| io::Error::other("shared runtime missing"))?
        .status = ready_status;
    task_runtime.terminal_observed.store(true, Ordering::SeqCst);
    reconcile_current(&client, &task_runtime, &store, adopted_task_uid).await?;
    assert_eq!(
        store
            .task(adopted_task_uid)
            .await?
            .ok_or(StoreError::TaskNotFound)?,
        before_retirement
    );
    assert_eq!(
        store
            .task_execution_attempt(adopted_task_uid)
            .await?
            .ok_or(StoreError::TaskNotFound)?,
        unknown_attempt
    );
    assert!(
        !store
            .task_orchestration_work_items()
            .await?
            .iter()
            .any(|work| work.task.task_uid == adopted_task_uid),
        "proven retirement removes finalized work without rewriting history"
    );
    assert_eq!(reservation.record.task_uid, task_uid);
    Ok(())
}

fn install_rustls_crypto_provider() -> Result<(), io::Error> {
    use tokio_rustls::rustls::crypto::{CryptoProvider, ring};

    if CryptoProvider::get_default().is_none() {
        let _ = ring::default_provider().install_default();
    }
    if CryptoProvider::get_default().is_some() {
        Ok(())
    } else {
        Err(io::Error::other("Rustls crypto provider is unavailable"))
    }
}

async fn reconcile_current(
    client: &Client,
    runtime: &AmbiguousTaskRuntime,
    store: &PgStore,
    task_uid: Uuid,
) -> Result<(), Box<dyn Error>> {
    let work = current_work(store, task_uid).await?;
    reconcile_task_orchestration_work_item(client, runtime, store, &work).await?;
    Ok(())
}

async fn current_work(
    store: &PgStore,
    task_uid: Uuid,
) -> Result<steward_store::TaskOrchestrationWorkItem, Box<dyn Error>> {
    store
        .task_orchestration_work_items()
        .await?
        .into_iter()
        .find(|work| work.task.task_uid == task_uid)
        .ok_or_else(|| io::Error::other("Task orchestration work item is not due").into())
}

async fn operation(
    store: &PgStore,
    task_uid: Uuid,
) -> Result<steward_store::TaskRuntimeOperationRecord, Box<dyn Error>> {
    Ok(store
        .task_runtime_operation(task_uid)
        .await?
        .ok_or(StoreError::TaskNotFound)?)
}

async fn kubernetes_client(
    state: AmbiguousKubernetes,
) -> Result<(Client, ServerGuard), Box<dyn Error>> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .fallback(any(kubernetes_request))
                .with_state(state),
        )
        .await
        .map_err(io::Error::other)
    });
    let mut config = kube::Config::new(format!("http://{address}").parse()?);
    config.default_namespace = "steward-test".to_owned();
    Ok((Client::try_from(config)?, ServerGuard(server)))
}

async fn kubernetes_request(
    State(state): State<AmbiguousKubernetes>,
    request: Request,
) -> Result<Response<Body>, Infallible> {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let response = match method {
        Method::POST if path == RUNTIME_PATH_PREFIX => {
            state.create_calls.fetch_add(1, Ordering::SeqCst);
            let bytes = match to_bytes(request.into_body(), 1024 * 1024).await {
                Ok(bytes) => bytes,
                Err(_) => return Ok(status_response(StatusCode::BAD_REQUEST, "BadRequest")),
            };
            let mut runtime = match serde_json::from_slice::<AgentRuntime>(&bytes) {
                Ok(runtime) => runtime,
                Err(_) => return Ok(status_response(StatusCode::BAD_REQUEST, "BadRequest")),
            };
            let mut stored = match state.runtime.lock() {
                Ok(stored) => stored,
                Err(_) => {
                    return Ok(status_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "InternalError",
                    ));
                }
            };
            if stored.is_some() {
                status_response(StatusCode::CONFLICT, "AlreadyExists")
            } else {
                runtime.metadata.uid = Some("runtime-uid-a".to_owned());
                runtime.metadata.resource_version = Some("1".to_owned());
                runtime.metadata.generation = Some(1);
                *stored = Some(runtime.clone());
                if let Ok(mut created) = state.created.lock() {
                    created.push(runtime.clone());
                }
                if state
                    .fail_first_create_response
                    .swap(false, Ordering::SeqCst)
                {
                    status_response(StatusCode::INTERNAL_SERVER_ERROR, "InternalError")
                } else {
                    json_response(StatusCode::CREATED, serde_json::to_value(&runtime))
                }
            }
        }
        Method::GET if path.starts_with(RUNTIME_PATH_PREFIX) => match state.runtime.lock() {
            Ok(runtime) => runtime.as_ref().map_or_else(
                || status_response(StatusCode::NOT_FOUND, "NotFound"),
                |runtime| json_response(StatusCode::OK, serde_json::to_value(runtime)),
            ),
            Err(_) => status_response(StatusCode::INTERNAL_SERVER_ERROR, "InternalError"),
        },
        Method::PUT if path.starts_with(RUNTIME_PATH_PREFIX) => {
            state.replace_calls.fetch_add(1, Ordering::SeqCst);
            let bytes = match to_bytes(request.into_body(), 1024 * 1024).await {
                Ok(bytes) => bytes,
                Err(_) => return Ok(status_response(StatusCode::BAD_REQUEST, "BadRequest")),
            };
            let mut desired = match serde_json::from_slice::<AgentRuntime>(&bytes) {
                Ok(runtime) => runtime,
                Err(_) => return Ok(status_response(StatusCode::BAD_REQUEST, "BadRequest")),
            };
            let mut stored = match state.runtime.lock() {
                Ok(stored) => stored,
                Err(_) => {
                    return Ok(status_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "InternalError",
                    ));
                }
            };
            let Some(current) = stored.as_ref() else {
                return Ok(status_response(StatusCode::NOT_FOUND, "NotFound"));
            };
            if desired.metadata.resource_version != current.metadata.resource_version {
                status_response(StatusCode::CONFLICT, "Conflict")
            } else {
                desired.metadata.uid.clone_from(&current.metadata.uid);
                desired.metadata.resource_version = Some("2".to_owned());
                desired.metadata.generation = Some(2);
                desired.status = Some(AgentRuntimeStatus {
                    phase: Phase::Running,
                    observed_generation: 2,
                    spec_digest: match runtime_spec_digest(&desired.spec) {
                        Ok(digest) => digest,
                        Err(_) => {
                            return Ok(status_response(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "InternalError",
                            ));
                        }
                    },
                    refs: RuntimeRefs {
                        workspace: Some("workspace-a".to_owned()),
                        sandbox: Some("sandbox-a".to_owned()),
                        litellm_key: None,
                    },
                    conditions: Vec::new(),
                    spend: None,
                });
                *stored = Some(desired.clone());
                json_response(StatusCode::OK, serde_json::to_value(&desired))
            }
        }
        Method::DELETE if path.starts_with(RUNTIME_PATH_PREFIX) => {
            let bytes = match to_bytes(request.into_body(), 1024 * 1024).await {
                Ok(bytes) => bytes,
                Err(_) => return Ok(status_response(StatusCode::BAD_REQUEST, "BadRequest")),
            };
            let uid = serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|value| {
                    value
                        .pointer("/preconditions/uid")
                        .and_then(|uid| uid.as_str())
                        .map(str::to_owned)
                });
            let Some(uid) = uid else {
                return Ok(status_response(StatusCode::CONFLICT, "Conflict"));
            };
            if let Ok(mut preconditions) = state.delete_preconditions.lock() {
                preconditions.push(uid.clone());
            }
            let mut stored = match state.runtime.lock() {
                Ok(stored) => stored,
                Err(_) => {
                    return Ok(status_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "InternalError",
                    ));
                }
            };
            if stored
                .as_ref()
                .and_then(|runtime| runtime.metadata.uid.as_deref())
                != Some(uid.as_str())
            {
                status_response(StatusCode::CONFLICT, "Conflict")
            } else {
                let mut replacement = stored.take().unwrap_or_else(|| unreachable!());
                if state
                    .replace_name_after_delete
                    .swap(false, Ordering::SeqCst)
                {
                    replacement.metadata.uid = Some("runtime-uid-replacement".to_owned());
                    replacement.metadata.resource_version = Some("3".to_owned());
                    *stored = Some(replacement);
                }
                status_response(StatusCode::OK, "Success")
            }
        }
        _ => status_response(StatusCode::NOT_FOUND, "NotFound"),
    };
    Ok(response)
}

fn status_response(status: StatusCode, reason: &str) -> Response<Body> {
    let body = json!({
        "apiVersion": "v1",
        "kind": "Status",
        "status": if status.is_success() { "Success" } else { "Failure" },
        "message": reason,
        "reason": reason,
        "code": status.as_u16(),
    });
    json_response(status, Ok(body))
}

fn json_response(
    status: StatusCode,
    value: Result<serde_json::Value, serde_json::Error>,
) -> Response<Body> {
    match value.and_then(|value| serde_json::to_vec(&value)) {
        Ok(body) => Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap_or_else(|_| Response::new(Body::empty())),
        Err(_) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::empty())
            .unwrap_or_else(|_| Response::new(Body::empty())),
    }
}

fn inert_spec(spec: &AgentRuntimeSpec, envelope: &Envelope) -> AgentRuntimeSpec {
    let mut inert = spec.clone();
    inert.llms.clear();
    inert.tools.clear();
    inert.budget.monthly_limit = "0".to_owned();
    inert.budget.single_run_limit = Some("0".to_owned());
    inert.budget.currency = envelope.spec.budget.currency.clone();
    inert
}

fn manifest_digest(
    task_uid: Uuid,
    operation_id: Uuid,
    runtime_name: &str,
    spec: &AgentRuntimeSpec,
    mode: &str,
) -> Result<String, serde_json::Error> {
    digest(Ok(json!({
        "schemaVersion": "steward-task-runtime-manifest/v1",
        "taskUid": task_uid,
        "operationId": operation_id,
        "runtimeNamespace": "steward-test",
        "runtimeName": runtime_name,
        "mode": mode,
        "spec": spec,
        "executionBinding": serde_json::Value::Null,
    })))
}

fn digest(
    value: Result<serde_json::Value, serde_json::Error>,
) -> Result<String, serde_json::Error> {
    Ok(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&value?)?)
    ))
}

fn runtime_spec_digest(spec: &AgentRuntimeSpec) -> Result<String, serde_json::Error> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(spec)?)))
}
