use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use sqlx::Row;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::types::Uuid;
use steward_admission::internal_authorities::steward_connections_v1;
use steward_admission::{
    AdmissionDecision, AdmissionDelta, Envelope, EnvelopeScopeKind, EnvelopeSpec, evaluate,
};
use steward_apiserver::governed_connections::{
    CONNECTIONS_AUTHORITY_DIGEST, CONNECTIONS_AUTHORITY_VERSION, CONNECTIONS_SERVICE,
    ConnectionExecutionBindings, ConnectionOperationKind as PlannedConnectionOperationKind,
    plan_connection_operation,
};
use steward_store::{
    AgentRunLogStream, AgentRunQuery, AgentRunTimelineKind, AgentRunTimelineProvenance,
    ApprovalDeliveryTransition, ApproveAdmission, BrowserRbacAssignment,
    BrowserRbacAssignmentAction, BrowserRbacAssignmentChange, ConnectionExecutionBindingSnapshot,
    ConnectionOAuthPhase, ConnectionOperationKind, ConnectionOperationReservation,
    ConnectionOperationReservationRequest, ConnectionOperationRetention, ConnectionOperationState,
    EnvelopeRequestReservationRequest, EnvelopeRequestStatus, EnvelopeRequestStatusUpdate,
    ParkRejection, PgStore, StoreError, TaskActivationObservation, TaskCleanupObservation,
    TaskExecutionAttemptState, TaskExecutionObservation, TaskExecutionTransition,
    TaskOperationTransition, TaskOrchestrationState, TaskReservationRequest,
};
use steward_types::direct_package::DirectTaskBindingEvidence;
use steward_types::{
    AgentRuntimeSpec, AgentType, Budget, CanonicalAuthorityBinding, CanonicalUserId, Duration,
    Email, ModelRef, OrganizationId, OrganizationIdentity, OrganizationIdentityMigration,
    OrganizationIdentityPolicy, Principal, RunnerRequirements, RuntimeOwnership, SpendSummary,
    TaskPhase,
};

fn governed_connection_bindings() -> ConnectionExecutionBindings {
    ConnectionExecutionBindings {
        artifact_trust_mode: "github-attestation".to_owned(),
        bridge_image_digest:
            "ghcr.io/example-org/steward-connections-bridge@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .to_owned(),
        mcp_gw_origin: "https://mcp-gw.example.test".to_owned(),
        mcp_gw_version: "0.3.2".to_owned(),
        namespace: "steward-test".to_owned(),
        runtime_class: "kata-qemu".to_owned(),
    }
}

fn connection_operation_retention() -> ConnectionOperationRetention {
    ConnectionOperationRetention {
        cache_ttl_seconds: 5,
        result_ttl_seconds: 30,
        oauth_lifetime_seconds: 630,
    }
}

async fn isolated_approval_queue_store(database_url: &str) -> Result<PgStore, Box<dyn Error>> {
    // Queue consumers intentionally select globally, not by this test's Task.
    // Give each queue fixture its own schema so unrelated tests cannot consume
    // its work. The shell harness owns and unconditionally deletes the ephemeral
    // Postgres instance (and these schemas), including when a test fails.
    let schema = format!("approval_queue_{}", Uuid::new_v4().simple());
    let bootstrap = PgPoolOptions::new()
        .max_connections(1)
        .connect(database_url)
        .await?;
    let created = sqlx::query(&format!("CREATE SCHEMA \"{schema}\""))
        .execute(&bootstrap)
        .await;
    bootstrap.close().await;
    created?;
    let options = database_url
        .parse::<PgConnectOptions>()?
        .options([("search_path", schema.as_str())]);
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    Ok(store)
}

#[tokio::test]
async fn durable_task_operation_is_atomic_generation_checked_and_uid_immutable()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the Task Postgres test")
    })?;
    let store = isolated_approval_queue_store(&database_url).await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let service = format!("task-orchestrator-{suffix}");
    let identity = store
        .register_canonical_identity(
            &google_identity(
                format!("task-orchestrator-subject-{suffix}"),
                format!("alice-{suffix}@example.com"),
            )?,
            "test-bootstrap",
        )
        .await?;
    let authority = envelope("250.00", 1);
    store
        .insert_service_envelope(&service, &authority, "admin@example.com")
        .await?;
    let mut spec = proposed_spec();
    spec.principal = Principal::Service {
        name: service.clone(),
        acting_user: Some(Email("alice@example.com".to_owned())),
    };
    spec.owner = Email("alice@example.com".to_owned());
    spec.canonical_authority = Some(CanonicalAuthorityBinding::new(
        identity.user_id.clone(),
        Some(identity.user_id.clone()),
    )?);
    let decision = evaluate(&spec, &authority)
        .map_err(|error| io::Error::other(format!("evaluate Task fixture: {error:?}")))?;
    assert_eq!(decision, AdmissionDecision::Admit);
    let command = vec!["agent-v1".to_owned()];
    let idempotency_key = format!("orchestration-{suffix}");
    let task_uid = Uuid::new_v4();
    let operation_id = Uuid::new_v4();
    let runtime_name = format!("task-{}", operation_id.simple());
    let candidate_digest = format!("sha256:{}", "1".repeat(64));
    let envelope_digest = format!("sha256:{}", "2".repeat(64));
    let inert_digest = format!("sha256:{}", "3".repeat(64));
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
            workflow: "code-review",
            workflow_name: None,
            workflow_version: None,
            workflow_digest: None,
            user_envelope_instance_id: None,
            user_envelope_revision: None,
            user_envelope_digest: None,
            coding_agent_runtime: "agent-v1",
            runtime_uid: None,
            runtime_namespace: "steward-test",
            runtime_name: &runtime_name,
            runtime_ownership: RuntimeOwnership::Provisioned,
            runtime_spec: &spec,
            agent_command: &command,
            execution_binding: None,
            direct_task_evidence: None,
            envelope_revision: authority.revision,
            service_envelope: &authority,
            service_envelope_digest: &envelope_digest,
            candidate_digest: &candidate_digest,
            admission_decision: &decision,
            inert_manifest_digest: &inert_digest,
            active_manifest_digest: &candidate_digest,
        })
        .await?;
    assert!(reservation.inserted);
    assert_eq!(
        reservation.operation.state,
        TaskOrchestrationState::IntentRecorded
    );
    assert_eq!(reservation.operation.generation, 1);
    assert_eq!(
        reservation.record.orchestration_operation_id,
        Some(reservation.operation.operation_id),
        "Task intent and runtime operation must commit as one identity"
    );
    store
        .put_task_inputs(
            reservation.record.task_uid,
            &service,
            identity.user_id.as_str(),
            b"fixture-input",
        )
        .await?;
    let execution_requested = store
        .request_task_execution(
            reservation.record.task_uid,
            &service,
            identity.user_id.as_str(),
        )
        .await?;
    assert_eq!(
        execution_requested.phase,
        TaskPhase::Submitted,
        "an execution command cannot project queued before exact runtime activation"
    );

    let mut invalid_owned_observation = store.pool().begin().await?;
    let skipped_create_authorization = sqlx::query(
        "UPDATE task_runtime_operations \
         SET state = 'runtime_observed', generation = generation + 1, \
             runtime_uid = 'runtime-uid-without-create-intent', \
             runtime_resource_version = 'resource-version-a', observed_at = now() \
         WHERE task_uid = $1",
    )
    .bind(task_uid)
    .execute(&mut *invalid_owned_observation)
    .await;
    invalid_owned_observation.rollback().await?;
    assert!(
        skipped_create_authorization.is_err(),
        "a provisioned runtime UID cannot be observed before inert creation is authorized"
    );

    let first_store = store.clone();
    let second_store = store.clone();
    let task_uid = reservation.record.task_uid;
    let (first, second) = tokio::join!(
        first_store.authorize_task_runtime_creation(task_uid, 1, "controller-a"),
        second_store.authorize_task_runtime_creation(task_uid, 1, "controller-b")
    );
    let transitions = [first?, second?];
    assert_eq!(
        transitions
            .iter()
            .filter(|transition| matches!(transition, TaskOperationTransition::Applied(_)))
            .count(),
        1,
        "exactly one reconciler may authorize the external create effect"
    );
    assert!(transitions.iter().all(|transition| match transition {
        TaskOperationTransition::Applied(current)
        | TaskOperationTransition::Superseded(current) => {
            current.state == TaskOrchestrationState::RuntimeCreatePending && current.generation == 2
        }
        TaskOperationTransition::AlreadyApplied(_)
        | TaskOperationTransition::AuthorityInactive { .. }
        | TaskOperationTransition::InvariantViolation { .. } => false,
    }));

    let mut invalid_observation = store.pool().begin().await?;
    let missing_resource_version = sqlx::query(
        "UPDATE task_runtime_operations \
         SET state = 'runtime_observed', generation = generation + 1, \
             runtime_uid = 'runtime-uid-without-version', observed_at = now() \
         WHERE task_uid = $1",
    )
    .bind(task_uid)
    .execute(&mut *invalid_observation)
    .await;
    invalid_observation.rollback().await?;
    assert!(
        missing_resource_version.is_err(),
        "a UID observation without its exact Kubernetes resource version must be rejected"
    );

    let observed = store
        .record_task_runtime_observed(
            task_uid,
            2,
            "runtime-uid-a",
            "resource-version-a",
            "controller-a",
        )
        .await?;
    let TaskOperationTransition::Applied(observed) = observed else {
        return Err(io::Error::other("the first exact UID observation must apply").into());
    };
    assert_eq!(observed.state, TaskOrchestrationState::RuntimeObserved);
    assert_eq!(observed.runtime_uid.as_deref(), Some("runtime-uid-a"));
    assert_eq!(observed.generation, 3);
    assert_eq!(
        store
            .task(task_uid)
            .await?
            .ok_or(StoreError::TaskNotFound)?
            .runtime_uid
            .as_deref(),
        Some("runtime-uid-a"),
        "the public Task projection must expose the operation's exact observed UID"
    );

    let stale = store
        .record_task_runtime_observed(
            task_uid,
            2,
            "runtime-uid-b",
            "resource-version-b",
            "controller-b",
        )
        .await?;
    assert!(matches!(
        stale,
        TaskOperationTransition::Superseded(current)
            if current.runtime_uid.as_deref() == Some("runtime-uid-a")
                && current.generation == 3
    ));
    let overwrite = sqlx::query(
        "UPDATE task_runtime_operations \
         SET runtime_uid = 'runtime-uid-b', generation = generation + 1 \
         WHERE task_uid = $1",
    )
    .bind(task_uid)
    .execute(store.pool())
    .await;
    assert!(
        overwrite.is_err(),
        "the database must reject replacement of an observed runtime UID"
    );
    let backwards = sqlx::query(
        "UPDATE task_runtime_operations \
         SET state = 'runtime_create_pending', generation = generation + 1 \
         WHERE task_uid = $1",
    )
    .bind(task_uid)
    .execute(store.pool())
    .await;
    assert!(
        backwards.is_err(),
        "the database must reject a backward orchestration transition"
    );

    let mut invalid_activation = store.pool().begin().await?;
    let incomplete_authority = sqlx::query(
        "UPDATE task_runtime_operations \
         SET state = 'activation_pending', generation = generation + 1, \
             activation_authority_kind = 'baseline' \
         WHERE task_uid = $1",
    )
    .bind(task_uid)
    .execute(&mut *invalid_activation)
    .await;
    invalid_activation.rollback().await?;
    assert!(
        incomplete_authority.is_err(),
        "activation intent without the exact Envelope revision and digest must be rejected"
    );

    let authority_selected = store
        .decide_task_runtime_authority(task_uid, 3, &authority, &envelope_digest, "controller-a")
        .await?;
    assert!(matches!(
        authority_selected,
        TaskOperationTransition::Applied(current)
            if current.state == TaskOrchestrationState::ActivationPending
                && current.generation == 4
                && current.activation_authority_kind.as_deref() == Some("baseline")
                && current.activation_envelope_revision == Some(authority.revision)
                && current.activation_envelope_digest.as_deref() == Some(envelope_digest.as_str())
    ));
    let refreshed_authority = envelope("250.00", 2);
    let refreshed_envelope_digest = format!("sha256:{}", "7".repeat(64));
    store
        .insert_service_envelope(&service, &refreshed_authority, "admin@example.com")
        .await?;
    let activation_authorized = store
        .decide_task_runtime_authority(
            task_uid,
            4,
            &refreshed_authority,
            &refreshed_envelope_digest,
            "controller-a",
        )
        .await?;
    assert!(
        matches!(
            activation_authorized,
            TaskOperationTransition::Applied(current)
                if current.state == TaskOrchestrationState::ActivationPending
                    && current.generation == 5
                    && current.activation_effect_authorized_at.is_some()
                    && current.activation_envelope_revision == Some(refreshed_authority.revision)
                    && current.activation_envelope_digest.as_deref()
                        == Some(refreshed_envelope_digest.as_str())
        ),
        "the external activation intent must revalidate and pin the latest still-admitting Envelope"
    );
    let active = store
        .record_task_activation_observed(
            task_uid,
            5,
            &TaskActivationObservation {
                runtime_uid: "runtime-uid-a",
                resource_version: "resource-version-active",
                active_manifest_digest: &candidate_digest,
                provider_set_ready: true,
            },
            "controller-a",
        )
        .await?;
    assert!(matches!(
        active,
        TaskOperationTransition::Applied(current)
            if current.state == TaskOrchestrationState::Active
                && current.generation == 6
                && current.runtime_uid.as_deref() == Some("runtime-uid-a")
    ));

    assert_eq!(
        store
            .task(task_uid)
            .await?
            .ok_or(StoreError::TaskNotFound)?
            .phase,
        TaskPhase::Queued,
        "the durable execution command becomes queued only after activation"
    );
    let phase_backwards =
        sqlx::query("UPDATE task_submissions SET phase = 'submitted' WHERE task_uid = $1")
            .bind(task_uid)
            .execute(store.pool())
            .await;
    assert!(
        phase_backwards.is_err(),
        "the database must reject a backward public Task phase transition"
    );
    let command_digest = format!("sha256:{}", "5".repeat(64));
    let input_digest = format!("sha256:{}", "6".repeat(64));
    let first_store = store.clone();
    let second_store = store.clone();
    let (first, second) = tokio::join!(
        first_store.claim_task_execution_attempt(
            task_uid,
            &command_digest,
            &input_digest,
            "controller-a",
        ),
        second_store.claim_task_execution_attempt(
            task_uid,
            &command_digest,
            &input_digest,
            "controller-b",
        )
    );
    let attempts = [first?, second?];
    assert_eq!(
        attempts
            .iter()
            .filter(|transition| matches!(transition, TaskExecutionTransition::Created(_)))
            .count(),
        1,
        "two reconcilers must reserve one immutable execution attempt"
    );
    let attempt = attempts
        .iter()
        .find_map(|transition| match transition {
            TaskExecutionTransition::Created(attempt)
            | TaskExecutionTransition::AlreadyApplied(attempt) => Some(attempt.clone()),
            _ => None,
        })
        .ok_or_else(|| io::Error::other("the execution attempt must remain observable"))?;
    assert_eq!(attempt.state, TaskExecutionAttemptState::StartPending);
    let mut invalid_running = store.pool().begin().await?;
    let unobserved_running = sqlx::query(
        "UPDATE task_execution_attempts \
         SET state = 'running', generation = generation + 1 \
         WHERE attempt_id = $1",
    )
    .bind(attempt.attempt_id)
    .execute(&mut *invalid_running)
    .await;
    invalid_running.rollback().await?;
    assert!(
        unobserved_running.is_err(),
        "a running execution attempt must carry start invocation and acknowledgement evidence"
    );
    let first_store = store.clone();
    let second_store = store.clone();
    let (first, second) = tokio::join!(
        first_store.authorize_task_execution_start(attempt.attempt_id, 1, "controller-a"),
        second_store.authorize_task_execution_start(attempt.attempt_id, 1, "controller-b")
    );
    let starts = [first?, second?];
    assert_eq!(
        starts
            .iter()
            .filter(|transition| matches!(transition, TaskExecutionTransition::Applied(_)))
            .count(),
        1,
        "exactly one reconciler may cross the external execution-start boundary"
    );
    let attempt = starts
        .iter()
        .find_map(|transition| match transition {
            TaskExecutionTransition::Applied(attempt)
            | TaskExecutionTransition::Superseded(attempt) => Some(attempt.clone()),
            _ => None,
        })
        .ok_or_else(|| io::Error::other("the authorized execution start must be observable"))?;
    assert!(attempt.start_invoked_at.is_some());
    assert!(attempt.start_observation_deadline_at.is_some());
    assert_eq!(attempt.generation, 2);
    let erase_start_intent = sqlx::query(
        "UPDATE task_execution_attempts \
         SET start_invoked_at = NULL, generation = generation + 1 \
         WHERE attempt_id = $1",
    )
    .bind(attempt.attempt_id)
    .execute(store.pool())
    .await;
    assert!(
        erase_start_intent.is_err(),
        "durable external-start intent must not be erasable before a replay"
    );
    let outcome_unknown = store
        .record_task_execution_observation(
            attempt.attempt_id,
            2,
            TaskExecutionObservation::OutcomeUnknown {
                reason: "ambiguous_adapter_start",
            },
            "controller-a",
        )
        .await?;
    assert!(matches!(
        outcome_unknown,
        TaskExecutionTransition::Applied(current)
            if current.state == TaskExecutionAttemptState::OutcomeUnknown
                && current.generation == 3
    ));
    let cleanup = store
        .task_runtime_operation(task_uid)
        .await?
        .ok_or(StoreError::TaskNotFound)?;
    assert_eq!(cleanup.state, TaskOrchestrationState::CleanupPending);
    assert_eq!(cleanup.generation, 7);
    let terminal_task = store
        .task(task_uid)
        .await?
        .ok_or(StoreError::TaskNotFound)?;
    assert_eq!(terminal_task.phase, TaskPhase::Failed);
    assert!(terminal_task.finalize_requested);
    assert_eq!(
        terminal_task.failure_reason.as_deref(),
        Some("execution_outcome_unknown")
    );
    assert!(matches!(
        store
            .claim_task_execution_attempt(
                task_uid,
                &command_digest,
                &input_digest,
                "controller-b",
            )
            .await?,
        TaskExecutionTransition::InvariantViolation { attempt: Some(current), .. }
            if current.attempt_id == attempt.attempt_id
                && current.state == TaskExecutionAttemptState::OutcomeUnknown
    ));

    let incomplete = store
        .record_task_cleanup_complete(
            task_uid,
            7,
            TaskCleanupObservation {
                exact_runtime_absent: false,
            },
            "controller-a",
        )
        .await?;
    assert!(matches!(
        incomplete,
        TaskOperationTransition::InvariantViolation { current, .. }
            if current.state == TaskOrchestrationState::CleanupPending
    ));
    let finalized = store
        .record_task_cleanup_complete(
            task_uid,
            7,
            TaskCleanupObservation {
                exact_runtime_absent: true,
            },
            "controller-a",
        )
        .await?;
    assert!(matches!(
        finalized,
        TaskOperationTransition::Applied(current)
            if current.state == TaskOrchestrationState::Finalized
                && current.runtime_absent_observed_at.is_some()
                && current.projections_absent_observed_at.is_some()
    ));
    assert!(
        store
            .task(task_uid)
            .await?
            .ok_or(StoreError::TaskNotFound)?
            .finalized
    );

    let command_after_finalization =
        sqlx::query("UPDATE task_submissions SET cancel_requested = true WHERE task_uid = $1")
            .bind(task_uid)
            .execute(store.pool())
            .await;
    assert!(
        command_after_finalization.is_err(),
        "a finalized Task must reject every later command"
    );

    let stale_task_uid = Uuid::new_v4();
    let stale_operation_id = Uuid::new_v4();
    let stale_runtime_name = format!("task-{}", stale_operation_id.simple());
    let stale_idempotency_key = format!("orchestration-stale-authority-{suffix}");
    let stale_reservation = store
        .reserve_task(&TaskReservationRequest {
            task_uid: stale_task_uid,
            operation_id: stale_operation_id,
            idempotency_key: &stale_idempotency_key,
            submitter_service: &service,
            acting_user: Some("alice@example.com"),
            acting_user_id: Some(identity.user_id.as_str()),
            owner: "alice@example.com",
            owner_user_id: identity.user_id.as_str(),
            workflow: "code-review",
            workflow_name: None,
            workflow_version: None,
            workflow_digest: None,
            user_envelope_instance_id: None,
            user_envelope_revision: None,
            user_envelope_digest: None,
            coding_agent_runtime: "agent-v1",
            runtime_uid: None,
            runtime_namespace: "steward-test",
            runtime_name: &stale_runtime_name,
            runtime_ownership: RuntimeOwnership::Provisioned,
            runtime_spec: &spec,
            agent_command: &command,
            execution_binding: None,
            direct_task_evidence: None,
            envelope_revision: refreshed_authority.revision,
            service_envelope: &refreshed_authority,
            service_envelope_digest: &refreshed_envelope_digest,
            candidate_digest: &candidate_digest,
            admission_decision: &AdmissionDecision::Admit,
            inert_manifest_digest: &inert_digest,
            active_manifest_digest: &candidate_digest,
        })
        .await?;
    assert!(matches!(
        store
            .authorize_task_runtime_creation(stale_reservation.record.task_uid, 1, "controller-a")
            .await?,
        TaskOperationTransition::Applied(_)
    ));
    assert!(matches!(
        store
            .record_task_runtime_observed(
                stale_reservation.record.task_uid,
                2,
                "runtime-uid-stale-authority",
                "resource-version-stale-authority",
                "controller-a",
            )
            .await?,
        TaskOperationTransition::Applied(_)
    ));
    assert!(matches!(
        store
            .decide_task_runtime_authority(
                stale_reservation.record.task_uid,
                3,
                &refreshed_authority,
                &refreshed_envelope_digest,
                "controller-a",
            )
            .await?,
        TaskOperationTransition::Applied(_)
    ));
    let restrictive_authority = envelope("200.00", 3);
    store
        .insert_service_envelope(&service, &restrictive_authority, "admin@example.com")
        .await?;
    let restrictive_envelope_digest = format!("sha256:{}", "8".repeat(64));
    assert!(
        matches!(
            store
                .decide_task_runtime_authority(
                    stale_reservation.record.task_uid,
                    4,
                    &restrictive_authority,
                    &restrictive_envelope_digest,
                    "controller-a",
                )
                .await?,
            TaskOperationTransition::AuthorityInactive { current, .. }
                if current.state == TaskOrchestrationState::CleanupPending
                    && current.generation == 5
        ),
        "authority loss must atomically prevent the activation effect and enter cleanup"
    );
    let excessive_decision = evaluate(&spec, &restrictive_authority)
        .map_err(|error| io::Error::other(format!("evaluate excessive Task: {error:?}")))?;
    assert!(matches!(
        excessive_decision,
        AdmissionDecision::Reject { .. }
    ));
    let excessive_envelope_digest = format!("sha256:{}", "4".repeat(64));
    let excessive_idempotency_key = format!("orchestration-excessive-{suffix}");
    let excessive_task_uid = Uuid::new_v4();
    let excessive_operation_id = Uuid::new_v4();
    let excessive_runtime_name = format!("task-{}", excessive_operation_id.simple());
    let excessive = store
        .reserve_task(&TaskReservationRequest {
            task_uid: excessive_task_uid,
            operation_id: excessive_operation_id,
            idempotency_key: &excessive_idempotency_key,
            submitter_service: &service,
            acting_user: Some("alice@example.com"),
            acting_user_id: Some(identity.user_id.as_str()),
            owner: "alice@example.com",
            owner_user_id: identity.user_id.as_str(),
            workflow: "code-review",
            workflow_name: None,
            workflow_version: None,
            workflow_digest: None,
            user_envelope_instance_id: None,
            user_envelope_revision: None,
            user_envelope_digest: None,
            coding_agent_runtime: "agent-v1",
            runtime_uid: None,
            runtime_namespace: "steward-test",
            runtime_name: &excessive_runtime_name,
            runtime_ownership: RuntimeOwnership::Provisioned,
            runtime_spec: &spec,
            agent_command: &command,
            execution_binding: None,
            direct_task_evidence: None,
            envelope_revision: restrictive_authority.revision,
            service_envelope: &restrictive_authority,
            service_envelope_digest: &excessive_envelope_digest,
            candidate_digest: &candidate_digest,
            admission_decision: &excessive_decision,
            inert_manifest_digest: &inert_digest,
            active_manifest_digest: &candidate_digest,
        })
        .await?;
    let excessive_uid = excessive.record.task_uid;
    assert!(matches!(
        store
            .authorize_task_runtime_creation(excessive_uid, 1, "controller-a")
            .await?,
        TaskOperationTransition::Applied(_)
    ));
    assert!(matches!(
        store
            .record_task_runtime_observed(
                excessive_uid,
                2,
                "runtime-uid-excessive",
                "resource-version-excessive",
                "controller-a",
            )
            .await?,
        TaskOperationTransition::Applied(_)
    ));
    let first_store = store.clone();
    let second_store = store.clone();
    let (first, second) = tokio::join!(
        first_store.decide_task_runtime_authority(
            excessive_uid,
            3,
            &restrictive_authority,
            &excessive_envelope_digest,
            "controller-a",
        ),
        second_store.decide_task_runtime_authority(
            excessive_uid,
            3,
            &restrictive_authority,
            &excessive_envelope_digest,
            "controller-b",
        )
    );
    let decisions = [first?, second?];
    assert_eq!(
        decisions
            .iter()
            .filter(|transition| matches!(transition, TaskOperationTransition::Applied(_)))
            .count(),
        1,
        "two reconcilers must materialize one runtime-bound approval"
    );
    assert!(decisions.iter().all(|transition| match transition {
        TaskOperationTransition::Applied(current)
        | TaskOperationTransition::Superseded(current) => {
            current.state == TaskOrchestrationState::ApprovalPending
                && current.generation == 4
                && current.approval_id.is_some()
        }
        TaskOperationTransition::AlreadyApplied(_)
        | TaskOperationTransition::AuthorityInactive { .. }
        | TaskOperationTransition::InvariantViolation { .. } => false,
    }));
    let approval_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM admission_decisions WHERE task_uid = $1")
            .bind(excessive_uid)
            .fetch_one(store.pool())
            .await?;
    let outbox_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM external_effect_outbox WHERE task_uid = $1")
            .bind(excessive_uid)
            .fetch_one(store.pool())
            .await?;
    assert_eq!(approval_count, 1);
    assert_eq!(outbox_count, 1);
    let approval_id = store
        .task_runtime_operation(excessive_uid)
        .await?
        .and_then(|operation| operation.approval_id)
        .ok_or_else(|| io::Error::other("approval identity must be durable"))?;
    sqlx::query(
        "UPDATE external_effect_outbox \
         SET created_at = '-infinity'::timestamptz, generation = generation + 1 \
         WHERE approval_id = $1",
    )
    .bind(approval_id)
    .execute(store.pool())
    .await?;
    let first_store = store.clone();
    let second_store = store.clone();
    let (first_delivery, second_delivery) = tokio::join!(
        first_store.claim_approval_delivery("dispatcher-a", 30),
        second_store.claim_approval_delivery("dispatcher-b", 30),
    );
    let deliveries = [first_delivery?, second_delivery?];
    assert_eq!(
        deliveries
            .iter()
            .filter(|delivery| {
                delivery
                    .as_ref()
                    .is_some_and(|delivery| delivery.approval_id == approval_id)
            })
            .count(),
        1,
        "two dispatchers must claim one durable approval-delivery identity"
    );
    let delivery = deliveries
        .into_iter()
        .flatten()
        .find(|delivery| delivery.approval_id == approval_id)
        .ok_or_else(|| io::Error::other("approval delivery was not claimable"))?;
    assert_eq!(delivery.approval_id, approval_id);
    assert_eq!(delivery.runtime_uid, "runtime-uid-excessive");
    assert_eq!(delivery.idempotency_key, format!("approval:{approval_id}"));
    let dispatcher = if store
        .complete_approval_delivery(
            delivery.effect_id,
            delivery.generation,
            "dispatcher-a",
            "PROJ-123",
            "https://jira.example.com/browse/PROJ-123",
        )
        .await?
        == ApprovalDeliveryTransition::Applied
    {
        "dispatcher-a"
    } else {
        "dispatcher-b"
    };
    if dispatcher == "dispatcher-b" {
        assert_eq!(
            store
                .complete_approval_delivery(
                    delivery.effect_id,
                    delivery.generation,
                    dispatcher,
                    "PROJ-123",
                    "https://jira.example.com/browse/PROJ-123",
                )
                .await?,
            ApprovalDeliveryTransition::Applied
        );
    }
    assert_eq!(
        store
            .complete_approval_delivery(
                delivery.effect_id,
                delivery.generation,
                dispatcher,
                "PROJ-123",
                "https://jira.example.com/browse/PROJ-123",
            )
            .await?,
        ApprovalDeliveryTransition::AlreadyApplied,
        "a lost database response must be recovered without another external approval"
    );
    store
        .approve_admission(ApproveAdmission {
            approval_id,
            decided_by: "admin@example.com",
            rationale: "bounded exact-runtime exception",
            evidence_url: "https://jira.example.com/browse/PROJ-123",
            expires_at: "2999-01-01T00:00:00Z",
        })
        .await?;
    let grant_authority = store
        .authorize_task_activation_from_approval(excessive_uid, 4, "controller-a")
        .await?;
    assert!(matches!(
        grant_authority,
        TaskOperationTransition::Applied(current)
            if current.state == TaskOrchestrationState::ActivationPending
                && current.generation == 5
                && current.activation_authority_kind.as_deref() == Some("grant")
                && current.approval_id == Some(approval_id)
    ));
    assert!(matches!(
        store
            .decide_task_runtime_authority(
                excessive_uid,
                5,
                &restrictive_authority,
                &excessive_envelope_digest,
                "controller-a",
            )
            .await?,
        TaskOperationTransition::Applied(current)
            if current.state == TaskOrchestrationState::ActivationPending
                && current.generation == 6
                && current.activation_effect_authorized_at.is_some()
    ));
    assert_eq!(
        store
            .revoke_runtime_grants(
                "runtime-uid-excessive",
                "admin@example.com",
                "approval withdrawn before activation",
            )
            .await?,
        1
    );
    assert!(
        matches!(
            store
                .decide_task_runtime_authority(
                    excessive_uid,
                    6,
                    &restrictive_authority,
                    &excessive_envelope_digest,
                    "controller-b",
                )
                .await?,
            TaskOperationTransition::AuthorityInactive { current, .. }
                if current.state == TaskOrchestrationState::CleanupPending
                    && current.generation == 7
        ),
        "a revoked exact grant must prevent activation even after an earlier worker was authorized"
    );

    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum CleanupEntryState {
    IntentRecorded,
    RuntimeCreatePending,
    RuntimeObserved,
    ApprovalPending,
    ApprovalClaimed,
    ApprovalActive,
    ActivationPending,
    Active,
}

#[tokio::test]
async fn finalization_is_monotonic_from_every_task_orchestration_state()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the Task Postgres test")
    })?;
    let store = isolated_approval_queue_store(&database_url).await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let service = format!("cleanup-matrix-{suffix}");
    let identity = store
        .register_canonical_identity(
            &google_identity(
                format!("cleanup-matrix-subject-{suffix}"),
                format!("alice-{suffix}@example.com"),
            )?,
            "test-bootstrap",
        )
        .await?;
    let authority = envelope("250.00", 1);
    store
        .insert_service_envelope(&service, &authority, "admin@example.com")
        .await?;
    let mut admitted_spec = proposed_spec();
    admitted_spec.principal = Principal::Service {
        name: service.clone(),
        acting_user: Some(Email("alice@example.com".to_owned())),
    };
    admitted_spec.owner = Email("alice@example.com".to_owned());
    admitted_spec.canonical_authority = Some(CanonicalAuthorityBinding::new(
        identity.user_id.clone(),
        Some(identity.user_id.clone()),
    )?);

    for entry_state in [
        CleanupEntryState::IntentRecorded,
        CleanupEntryState::RuntimeCreatePending,
        CleanupEntryState::RuntimeObserved,
        CleanupEntryState::ApprovalPending,
        CleanupEntryState::ApprovalClaimed,
        CleanupEntryState::ApprovalActive,
        CleanupEntryState::ActivationPending,
        CleanupEntryState::Active,
    ] {
        let task_uid = reserve_task_at_cleanup_entry_state(
            &store,
            &service,
            &identity.user_id,
            &authority,
            &admitted_spec,
            entry_state,
            &suffix,
        )
        .await?;
        let before = store
            .task_runtime_operation(task_uid)
            .await?
            .ok_or(StoreError::TaskNotFound)?;
        assert_eq!(before.state.as_str(), cleanup_entry_state_name(entry_state));
        store
            .request_task_finalization(task_uid, &service, identity.user_id.as_str())
            .await?;
        let cleanup = store
            .enter_task_cleanup(
                task_uid,
                before.generation,
                steward_store::TaskCleanupCause::FinalizationRequested,
                "controller-a",
            )
            .await?;
        let TaskOperationTransition::Applied(cleanup) = cleanup else {
            return Err(io::Error::other(format!(
                "finalization did not enter cleanup from {entry_state:?}"
            ))
            .into());
        };
        assert_eq!(cleanup.state, TaskOrchestrationState::CleanupPending);
        assert!(matches!(
            store
                .enter_task_cleanup(
                    task_uid,
                    cleanup.generation,
                    steward_store::TaskCleanupCause::FinalizationRequested,
                    "controller-b",
                )
                .await?,
            TaskOperationTransition::AlreadyApplied(current)
                if current.generation == cleanup.generation
        ));
        let cleanup =
            if cleanup.runtime_create_authorized_at.is_some() && cleanup.runtime_uid.is_none() {
                assert!(
                    matches!(
                        store
                            .record_task_cleanup_complete(
                                task_uid,
                                cleanup.generation,
                                TaskCleanupObservation {
                                    exact_runtime_absent: true,
                                },
                                "controller-a",
                            )
                            .await?,
                        TaskOperationTransition::InvariantViolation { current, .. }
                            if current.state == TaskOrchestrationState::CleanupPending
                    ),
                    "one absence read cannot finalize a Task after runtime creation was authorized"
                );
                let observed = store
                    .record_task_cleanup_runtime_observed(
                        task_uid,
                        cleanup.generation,
                        &format!("cleanup-runtime-{task_uid}"),
                        "cleanup-resource-version",
                        "controller-a",
                    )
                    .await?;
                let TaskOperationTransition::Applied(observed) = observed else {
                    return Err(io::Error::other(
                        "cleanup did not persist the exact runtime created to resolve ambiguity",
                    )
                    .into());
                };
                observed
            } else {
                cleanup
            };
        let runtime_effect_possible = cleanup.runtime_uid.is_some();
        if matches!(entry_state, CleanupEntryState::ApprovalClaimed) {
            assert!(
                matches!(
                    store
                        .record_task_cleanup_complete(
                            task_uid,
                            cleanup.generation,
                            TaskCleanupObservation {
                                exact_runtime_absent: runtime_effect_possible,
                            },
                            "controller-a",
                        )
                        .await?,
                    TaskOperationTransition::InvariantViolation {
                        reason: "approval_authority_cleanup_pending",
                        ..
                    }
                ),
                "cleanup must wait for a claimed external approval delivery"
            );
            let delivery = sqlx::query(
                "SELECT id, generation FROM external_effect_outbox WHERE task_uid = $1",
            )
            .bind(task_uid)
            .fetch_one(store.pool())
            .await?;
            assert_eq!(
                store
                    .retry_approval_delivery(
                        delivery.try_get("id")?,
                        delivery.try_get("generation")?,
                        "cleanup-matrix",
                        "delivery_interrupted_by_cleanup",
                    )
                    .await?,
                ApprovalDeliveryTransition::Applied
            );
        }
        let finalized = store
            .record_task_cleanup_complete(
                task_uid,
                cleanup.generation,
                TaskCleanupObservation {
                    exact_runtime_absent: runtime_effect_possible,
                },
                "controller-a",
            )
            .await?;
        assert!(matches!(
            finalized,
            TaskOperationTransition::Applied(current)
                if current.state == TaskOrchestrationState::Finalized
        ));
        let finalized_task = store
            .task(task_uid)
            .await?
            .ok_or(StoreError::TaskNotFound)?;
        assert!(finalized_task.finalized);
        let repeated_delete = store
            .request_task_finalization(task_uid, &service, identity.user_id.as_str())
            .await?;
        assert_eq!(
            repeated_delete, finalized_task,
            "DELETE retry must not rewrite finalized history"
        );
        if matches!(
            entry_state,
            CleanupEntryState::ApprovalPending | CleanupEntryState::ApprovalClaimed
        ) {
            let approval_state: String = sqlx::query_scalar(
                "SELECT approvals.state FROM approvals \
                 JOIN admission_decisions decisions \
                   ON decisions.id = approvals.admission_decision_id \
                 WHERE decisions.task_uid = $1",
            )
            .bind(task_uid)
            .fetch_one(store.pool())
            .await?;
            let delivery_state: String =
                sqlx::query_scalar("SELECT state FROM external_effect_outbox WHERE task_uid = $1")
                    .bind(task_uid)
                    .fetch_one(store.pool())
                    .await?;
            assert_eq!(approval_state, "rejected");
            assert_eq!(delivery_state, "failed");
        }
        if matches!(entry_state, CleanupEntryState::ApprovalActive) {
            let active_grants: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM grants \
                 LEFT JOIN grant_revocations ON grant_revocations.grant_id = grants.id \
                 JOIN approvals ON approvals.id = grants.approval_id \
                 JOIN admission_decisions decisions \
                   ON decisions.id = approvals.admission_decision_id \
                 WHERE decisions.task_uid = $1 AND grant_revocations.grant_id IS NULL",
            )
            .bind(task_uid)
            .fetch_one(store.pool())
            .await?;
            assert_eq!(
                active_grants, 0,
                "finalization must revoke every Task grant"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn execution_claim_and_start_revalidate_latest_baseline_authority()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the Task Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let service = format!("start-authority-race-{suffix}");
    let identity = store
        .register_canonical_identity(
            &google_identity(
                format!("start-authority-subject-{suffix}"),
                format!("alice-{suffix}@example.com"),
            )?,
            "test-bootstrap",
        )
        .await?;
    let authority = envelope("250.00", 1);
    store
        .insert_service_envelope(&service, &authority, "admin@example.com")
        .await?;
    let mut spec = proposed_spec();
    spec.principal = Principal::Service {
        name: service.clone(),
        acting_user: Some(Email("alice@example.com".to_owned())),
    };
    spec.owner = Email("alice@example.com".to_owned());
    spec.canonical_authority = Some(CanonicalAuthorityBinding::new(
        identity.user_id.clone(),
        Some(identity.user_id.clone()),
    )?);
    let admitted_task_uid = reserve_task_at_cleanup_entry_state(
        &store,
        &service,
        &identity.user_id,
        &authority,
        &spec,
        CleanupEntryState::Active,
        &format!("{suffix}-still-admitted"),
    )
    .await?;
    let revoked_task_uid = reserve_task_at_cleanup_entry_state(
        &store,
        &service,
        &identity.user_id,
        &authority,
        &spec,
        CleanupEntryState::Active,
        &format!("{suffix}-revoked"),
    )
    .await?;
    let idle_task_uid = reserve_task_at_cleanup_entry_state(
        &store,
        &service,
        &identity.user_id,
        &authority,
        &spec,
        CleanupEntryState::Active,
        &format!("{suffix}-idle-authority-loss"),
    )
    .await?;
    store
        .put_task_inputs(
            admitted_task_uid,
            &service,
            identity.user_id.as_str(),
            b"still-admitted-input",
        )
        .await?;
    store
        .request_task_execution(admitted_task_uid, &service, identity.user_id.as_str())
        .await?;
    let still_admitted = envelope("300.00", 2);
    store
        .insert_service_envelope(&service, &still_admitted, "admin@example.com")
        .await?;
    let admitted_attempt = match store
        .claim_task_execution_attempt(
            admitted_task_uid,
            &format!("sha256:{}", "5".repeat(64)),
            &format!("sha256:{}", "6".repeat(64)),
            "controller-a",
        )
        .await?
    {
        TaskExecutionTransition::Created(attempt) => attempt,
        transition => {
            return Err(io::Error::other(format!(
                "execution attempt was not reserved: {transition:?}"
            ))
            .into());
        }
    };
    assert!(
        matches!(
            store
                .authorize_task_execution_start(
                    admitted_attempt.attempt_id,
                    admitted_attempt.generation,
                    "controller-a",
                )
                .await?,
            TaskExecutionTransition::Applied(current)
                if current.attempt_id == admitted_attempt.attempt_id
                    && current.start_invoked_at.is_some()
        ),
        "a newer Envelope that still admits the immutable candidate must permit execution"
    );

    store
        .put_task_inputs(
            revoked_task_uid,
            &service,
            identity.user_id.as_str(),
            b"authority-race-input",
        )
        .await?;
    store
        .request_task_execution(revoked_task_uid, &service, identity.user_id.as_str())
        .await?;
    let revoked_attempt = match store
        .claim_task_execution_attempt(
            revoked_task_uid,
            &format!("sha256:{}", "7".repeat(64)),
            &format!("sha256:{}", "8".repeat(64)),
            "controller-a",
        )
        .await?
    {
        TaskExecutionTransition::Created(attempt) => attempt,
        transition => {
            return Err(io::Error::other(format!(
                "execution attempt was not reserved under the latest authority: {transition:?}"
            ))
            .into());
        }
    };
    let restrictive = envelope("100.00", 3);
    store
        .insert_service_envelope(&service, &restrictive, "admin@example.com")
        .await?;
    assert!(
        matches!(
            store
                .revalidate_active_task_authority(idle_task_uid, 6, "controller-b")
                .await?,
            TaskOperationTransition::AuthorityInactive { current, .. }
                if current.state == TaskOrchestrationState::CleanupPending
        ),
        "an idle active Task must enter cleanup as soon as its authority becomes inactive"
    );
    assert!(
        matches!(
            store
                .authorize_task_execution_start(
                    revoked_attempt.attempt_id,
                    revoked_attempt.generation,
                    "controller-b",
                )
                .await?,
            TaskExecutionTransition::AuthorityInactive { attempt: Some(current), .. }
                if current.attempt_id == revoked_attempt.attempt_id
                    && current.start_invoked_at.is_none()
        ),
        "authority lost after attempt claim must prevent the external start crossing"
    );
    assert_eq!(
        store
            .task_runtime_operation(revoked_task_uid)
            .await?
            .ok_or(StoreError::TaskNotFound)?
            .state,
        TaskOrchestrationState::CleanupPending
    );
    assert_eq!(
        store
            .task(revoked_task_uid)
            .await?
            .ok_or(StoreError::TaskNotFound)?
            .phase,
        TaskPhase::Failed
    );
    Ok(())
}

#[tokio::test]
async fn orchestration_upgrade_preserves_finalized_adopted_identity_without_an_operation()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL")?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool.clone());
    store.migrate().await?;
    let suffix = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let identity = store
        .register_canonical_identity(
            &google_identity(
                format!("historical-{suffix}"),
                format!("alice-{suffix}@example.com"),
            )?,
            "test-bootstrap",
        )
        .await?;
    let mut spec = proposed_spec();
    spec.canonical_authority = Some(CanonicalAuthorityBinding::new(
        identity.user_id.clone(),
        Some(identity.user_id.clone()),
    )?);
    let task_uid = Uuid::new_v4();
    let schema = format!("historical_task_{suffix}");
    // Transaction rollback is the RAII teardown for the entire historical schema,
    // including on assertion failure or cancellation.
    let mut transaction = pool.begin().await?;
    sqlx::query(&format!("CREATE SCHEMA \"{schema}\""))
        .execute(&mut *transaction)
        .await?;
    sqlx::query(&format!("SET LOCAL search_path TO \"{schema}\""))
        .execute(&mut *transaction)
        .await?;
    let mut migrations =
        fs::read_dir(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../migrations"))?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "sql"))
            .collect::<Vec<_>>();
    migrations.sort();
    for migration in &migrations {
        let name = migration
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| io::Error::other("invalid migration name"))?;
        if name.starts_with("0028_") {
            sqlx::query("INSERT INTO canonical_users SELECT * FROM public.canonical_users WHERE user_id = $1")
                .bind(identity.user_id.as_str()).execute(&mut *transaction).await?;
            sqlx::query(
                "INSERT INTO task_submissions \
                 (task_uid, idempotency_key, submitter_service, acting_user, owner, workflow, \
                  coding_agent_runtime, runtime_uid, runtime_namespace, runtime_name, runtime_ownership, \
                  phase, runtime_spec, agent_command, owner_user_id, acting_user_id, identity_binding_state, \
                  envelope_revision, finalize_requested, finalized) \
                 VALUES ($1, 'historical-adopted', 'steward-run', 'alice@example.com', 'alice@example.com', \
                  'code-review', 'agent-v1', 'historical-runtime-uid', 'steward-test', 'shared-runtime', \
                  'adopted', 'succeeded', $2, '[\"agent-v1\"]', $3, $3, 'bound', 1, true, true)",
            ).bind(task_uid).bind(sqlx::types::Json(&spec)).bind(identity.user_id.as_str())
                .execute(&mut *transaction).await?;
        }
        sqlx::raw_sql(&fs::read_to_string(migration)?)
            .execute(&mut *transaction)
            .await?;
    }
    let historical = sqlx::query("SELECT runtime_uid, orchestration_version, orchestration_operation_id, finalized FROM task_submissions WHERE task_uid = $1")
        .bind(task_uid).fetch_one(&mut *transaction).await?;
    assert_eq!(
        historical.try_get::<String, _>("runtime_uid")?,
        "historical-runtime-uid"
    );
    assert_eq!(historical.try_get::<i16, _>("orchestration_version")?, 1);
    assert_eq!(
        historical.try_get::<Option<Uuid>, _>("orchestration_operation_id")?,
        None
    );
    assert!(historical.try_get::<bool, _>("finalized")?);
    let operations: i64 =
        sqlx::query_scalar("SELECT count(*) FROM task_runtime_operations WHERE task_uid = $1")
            .bind(task_uid)
            .fetch_one(&mut *transaction)
            .await?;
    assert_eq!(
        operations, 0,
        "upgrade must not manufacture orchestration history"
    );
    transaction.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn expired_delivery_lease_never_replays_an_inflight_approval_create()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL")?;
    let store = isolated_approval_queue_store(&database_url).await?;
    let suffix = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let service = format!("delivery-race-{suffix}");
    let identity = store
        .register_canonical_identity(
            &google_identity(
                format!("delivery-{suffix}"),
                format!("alice-{suffix}@example.com"),
            )?,
            "test-bootstrap",
        )
        .await?;
    let authority = envelope("250.00", 1);
    store
        .insert_service_envelope(&service, &authority, "admin@example.com")
        .await?;
    let mut spec = proposed_spec();
    spec.principal = Principal::Service {
        name: service.clone(),
        acting_user: Some(Email("alice@example.com".to_owned())),
    };
    spec.owner = Email("alice@example.com".to_owned());
    spec.canonical_authority = Some(CanonicalAuthorityBinding::new(
        identity.user_id.clone(),
        Some(identity.user_id.clone()),
    )?);
    let task_uid = reserve_task_at_cleanup_entry_state(
        &store,
        &service,
        &identity.user_id,
        &authority,
        &spec,
        CleanupEntryState::ApprovalPending,
        &suffix.to_string(),
    )
    .await?;
    let channel = SlowDecisionChannel::default();
    let first_store = store.clone();
    let first_channel = channel.clone();
    let mut first = ApprovalDispatcherGuard(tokio::spawn(async move {
        steward_controller::dispatch_one_task_approval(&first_store, &first_channel, "controller-a")
            .await
    }));
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        channel.started.notified(),
    )
    .await?;
    sqlx::query("UPDATE external_effect_outbox SET claimed_until = now() - interval '1 second', generation = generation + 1 WHERE task_uid = $1")
        .bind(task_uid).execute(store.pool()).await?;
    let _second =
        steward_controller::dispatch_one_task_approval(&store, &channel, "controller-b").await;
    assert_eq!(
        channel.creates.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "lease expiry must not authorize a concurrent external create"
    );

    store
        .request_task_finalization(task_uid, &service, identity.user_id.as_str())
        .await?;
    let operation = store
        .task_runtime_operation(task_uid)
        .await?
        .ok_or(StoreError::TaskNotFound)?;
    store
        .enter_task_cleanup(
            task_uid,
            operation.generation,
            steward_store::TaskCleanupCause::Cancelled,
            "controller-b",
        )
        .await?;
    let cleanup = store
        .task_runtime_operation(task_uid)
        .await?
        .ok_or(StoreError::TaskNotFound)?;
    assert!(
        matches!(
            store
                .record_task_cleanup_complete(
                    task_uid,
                    cleanup.generation,
                    TaskCleanupObservation {
                        exact_runtime_absent: true
                    },
                    "controller-b"
                )
                .await?,
            TaskOperationTransition::InvariantViolation {
                reason: "approval_authority_cleanup_pending",
                ..
            }
        ),
        "cleanup must not claim absence while an invoked approval remains unresolved"
    );
    channel.release.notify_one();
    (&mut first.0).await??;
    sqlx::query("UPDATE external_effect_outbox SET retry_at = now(), generation = generation + 1, claimed_until = CASE WHEN state = 'claimed' THEN now() - interval '1 second' ELSE claimed_until END WHERE task_uid = $1")
        .bind(task_uid).execute(store.pool()).await?;
    steward_controller::dispatch_one_task_approval(&store, &channel, "controller-b").await?;
    assert_eq!(channel.creates.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(matches!(
        store
            .record_task_cleanup_complete(
                task_uid,
                cleanup.generation,
                TaskCleanupObservation {
                    exact_runtime_absent: true
                },
                "controller-b"
            )
            .await?,
        TaskOperationTransition::Applied(_)
    ));
    Ok(())
}

#[derive(Clone, Default)]
struct SlowDecisionChannel {
    creates: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    delivered: std::sync::Arc<std::sync::atomic::AtomicBool>,
    started: std::sync::Arc<tokio::sync::Notify>,
    release: std::sync::Arc<tokio::sync::Notify>,
}

impl steward_ports::DecisionChannel for SlowDecisionChannel {
    async fn observe_request(
        &self,
        _request_id: &str,
    ) -> Result<Option<steward_ports::DecisionReference>, steward_ports::PortError> {
        Ok(self
            .delivered
            .load(std::sync::atomic::Ordering::SeqCst)
            .then(|| steward_ports::DecisionReference {
                key: "PROJ-123".to_owned(),
                evidence_url: "https://jira.example.test/browse/PROJ-123".to_owned(),
            }))
    }

    async fn request(
        &self,
        _request: &steward_ports::DecisionRequest,
    ) -> Result<steward_ports::DecisionReference, steward_ports::PortError> {
        if self
            .creates
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            != 0
        {
            return Err(steward_ports::PortError::Failed {
                reason: "duplicate external create".to_owned(),
            });
        }
        self.started.notify_one();
        self.release.notified().await;
        self.delivered
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(steward_ports::DecisionReference {
            key: "PROJ-123".to_owned(),
            evidence_url: "https://jira.example.test/browse/PROJ-123".to_owned(),
        })
    }

    async fn record_resolution(
        &self,
        _resolution: &steward_ports::DecisionResolution,
    ) -> Result<(), steward_ports::PortError> {
        Ok(())
    }
}

struct ApprovalDispatcherGuard(
    tokio::task::JoinHandle<Result<bool, steward_controller::ApprovalDispatcherError>>,
);

impl Drop for ApprovalDispatcherGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[tokio::test]
async fn prestart_cleanup_fences_start_without_manufacturing_execution()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL")?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool.clone());
    store.migrate().await?;
    let suffix = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let identity = store
        .register_canonical_identity(
            &google_identity(
                format!("prestart-{suffix}"),
                format!("alice-{suffix}@example.com"),
            )?,
            "test-bootstrap",
        )
        .await?;
    for path in [
        "revalidate",
        "claim",
        "authorize",
        "delete",
        "authorized_delete",
        "racing_delete",
    ] {
        let service = format!("prestart-{path}-{suffix}");
        let authority = envelope("250.00", 1);
        store
            .insert_service_envelope(&service, &authority, "admin@example.com")
            .await?;
        let mut spec = proposed_spec();
        spec.principal = Principal::Service {
            name: service.clone(),
            acting_user: Some(Email("alice@example.com".to_owned())),
        };
        spec.owner = Email("alice@example.com".to_owned());
        spec.canonical_authority = Some(CanonicalAuthorityBinding::new(
            identity.user_id.clone(),
            Some(identity.user_id.clone()),
        )?);
        let task_uid = reserve_task_at_cleanup_entry_state(
            &store,
            &service,
            &identity.user_id,
            &authority,
            &spec,
            CleanupEntryState::Active,
            &format!("{suffix}-{path}"),
        )
        .await?;
        store
            .put_task_inputs(task_uid, &service, identity.user_id.as_str(), b"input")
            .await?;
        store
            .request_task_execution(task_uid, &service, identity.user_id.as_str())
            .await?;
        let command = format!("sha256:{}", "7".repeat(64));
        let input = format!("sha256:{}", "8".repeat(64));
        let attempt = match store
            .claim_task_execution_attempt(task_uid, &command, &input, "controller-a")
            .await?
        {
            TaskExecutionTransition::Created(attempt) => attempt,
            other => return Err(io::Error::other(format!("claim failed: {other:?}")).into()),
        };
        if path == "authorized_delete" {
            assert!(matches!(
                store
                    .authorize_task_execution_start(
                        attempt.attempt_id,
                        attempt.generation,
                        "controller-a"
                    )
                    .await?,
                TaskExecutionTransition::Applied(_)
            ));
        }
        if path == "racing_delete" {
            let (start, delete) = tokio::join!(
                store.authorize_task_execution_start(
                    attempt.attempt_id,
                    attempt.generation,
                    "controller-a"
                ),
                store.request_task_finalization(task_uid, &service, identity.user_id.as_str()),
            );
            start?;
            delete?;
        }
        if path.ends_with("delete") {
            store
                .request_task_finalization(task_uid, &service, identity.user_id.as_str())
                .await?;
            store
                .enter_task_cleanup(
                    task_uid,
                    6,
                    steward_store::TaskCleanupCause::Cancelled,
                    "controller-b",
                )
                .await?;
        } else {
            store
                .insert_service_envelope(&service, &envelope("100.00", 2), "admin@example.com")
                .await?;
            match path {
                "revalidate" => {
                    store
                        .revalidate_active_task_authority(task_uid, 6, "controller-b")
                        .await?;
                }
                "claim" => {
                    store
                        .claim_task_execution_attempt(task_uid, &command, &input, "controller-b")
                        .await?;
                }
                _ => {
                    store
                        .authorize_task_execution_start(
                            attempt.attempt_id,
                            attempt.generation,
                            "controller-b",
                        )
                        .await?;
                }
            }
        }
        let row = sqlx::query("SELECT state, start_invoked_at IS NULL AS never_started, finished_at IS NOT NULL AS finished FROM task_execution_attempts WHERE attempt_id = $1")
            .bind(attempt.attempt_id).fetch_one(&pool).await?;
        let never_started = row.try_get::<bool, _>("never_started")?;
        if path != "authorized_delete" && path != "racing_delete" {
            assert!(never_started, "start was never authorized: {path}");
        }
        assert_eq!(
            row.try_get::<String, _>("state")?,
            if never_started {
                "not_started"
            } else {
                "cancel_pending"
            },
            "{path}"
        );
        assert_eq!(row.try_get::<bool, _>("finished")?, never_started);
        let leased: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM task_runtime_execution_leases WHERE attempt_id = $1)",
        )
        .bind(attempt.attempt_id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            leased, !never_started,
            "only proof of no authorized start can release ownership: {path}"
        );
        assert!(
            !matches!(
                store
                    .authorize_task_execution_start(
                        attempt.attempt_id,
                        attempt.generation,
                        "controller-a"
                    )
                    .await?,
                TaskExecutionTransition::Applied(_)
            ),
            "a stale start must remain fenced: {path}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn terminal_execution_failure_enters_cleanup_and_authority_loss_preserves_terminal_phase()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the Task Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let service = format!("terminal-cleanup-{suffix}");
    let identity = store
        .register_canonical_identity(
            &google_identity(
                format!("terminal-cleanup-subject-{suffix}"),
                format!("alice-{suffix}@example.com"),
            )?,
            "test-bootstrap",
        )
        .await?;
    let authority = envelope("250.00", 1);
    store
        .insert_service_envelope(&service, &authority, "admin@example.com")
        .await?;
    let mut spec = proposed_spec();
    spec.principal = Principal::Service {
        name: service.clone(),
        acting_user: Some(Email("alice@example.com".to_owned())),
    };
    spec.owner = Email("alice@example.com".to_owned());
    spec.canonical_authority = Some(CanonicalAuthorityBinding::new(
        identity.user_id.clone(),
        Some(identity.user_id.clone()),
    )?);

    let failed_task_uid = reserve_task_at_cleanup_entry_state(
        &store,
        &service,
        &identity.user_id,
        &authority,
        &spec,
        CleanupEntryState::Active,
        &format!("{suffix}-failed"),
    )
    .await?;
    store
        .put_task_inputs(
            failed_task_uid,
            &service,
            identity.user_id.as_str(),
            b"failed-input",
        )
        .await?;
    store
        .request_task_execution(failed_task_uid, &service, identity.user_id.as_str())
        .await?;
    let failed_attempt = match store
        .claim_task_execution_attempt(
            failed_task_uid,
            &format!("sha256:{}", "7".repeat(64)),
            &format!("sha256:{}", "8".repeat(64)),
            "controller-a",
        )
        .await?
    {
        TaskExecutionTransition::Created(attempt) => attempt,
        transition => {
            return Err(io::Error::other(format!(
                "failed execution attempt was not reserved: {transition:?}"
            ))
            .into());
        }
    };
    let started_attempt = match store
        .authorize_task_execution_start(
            failed_attempt.attempt_id,
            failed_attempt.generation,
            "controller-a",
        )
        .await?
    {
        TaskExecutionTransition::Applied(attempt) => attempt,
        transition => {
            return Err(io::Error::other(format!(
                "failed execution attempt was not started: {transition:?}"
            ))
            .into());
        }
    };
    store
        .record_task_execution_observation(
            started_attempt.attempt_id,
            started_attempt.generation,
            TaskExecutionObservation::Failed {
                adapter_observation_id: "adapter-failed-attempt",
                reason: "agent_exit_nonzero",
                execution_stdout: Some(b"failed stdout\n"),
                execution_stderr: Some(b"failed stderr\n"),
            },
            "controller-a",
        )
        .await?;
    let failed_task = store
        .task(failed_task_uid)
        .await?
        .ok_or(StoreError::TaskNotFound)?;
    assert_eq!(failed_task.phase, TaskPhase::Failed);
    assert_eq!(
        store
            .agent_run_execution_log(
                failed_task_uid,
                Some(identity.user_id.as_str()),
                AgentRunLogStream::Stdout,
            )
            .await?,
        Some(b"failed stdout\n".to_vec())
    );
    assert_eq!(
        store
            .agent_run_execution_log(
                failed_task_uid,
                Some(identity.user_id.as_str()),
                AgentRunLogStream::Stderr,
            )
            .await?,
        Some(b"failed stderr\n".to_vec())
    );
    assert_eq!(
        store
            .agent_run_execution_log(
                failed_task_uid,
                Some("usr_abcdefabcdefabcdefabcdefabcdefab"),
                AgentRunLogStream::Stderr,
            )
            .await?,
        None,
        "execution logs must not cross the browser owner boundary"
    );
    assert!(
        failed_task.finalize_requested,
        "a terminal execution failure must request runtime cleanup"
    );
    assert_eq!(
        store
            .task_runtime_operation(failed_task_uid)
            .await?
            .ok_or(StoreError::TaskNotFound)?
            .state,
        TaskOrchestrationState::CleanupPending,
        "a terminal execution failure must atomically enter cleanup"
    );

    let succeeded_task_uid = reserve_task_at_cleanup_entry_state(
        &store,
        &service,
        &identity.user_id,
        &authority,
        &spec,
        CleanupEntryState::Active,
        &format!("{suffix}-succeeded"),
    )
    .await?;
    store
        .put_task_inputs(
            succeeded_task_uid,
            &service,
            identity.user_id.as_str(),
            b"succeeded-input",
        )
        .await?;
    store
        .request_task_execution(succeeded_task_uid, &service, identity.user_id.as_str())
        .await?;
    let succeeded_attempt = match store
        .claim_task_execution_attempt(
            succeeded_task_uid,
            &format!("sha256:{}", "9".repeat(64)),
            &format!("sha256:{}", "a".repeat(64)),
            "controller-a",
        )
        .await?
    {
        TaskExecutionTransition::Created(attempt) => attempt,
        transition => {
            return Err(io::Error::other(format!(
                "successful execution attempt was not reserved: {transition:?}"
            ))
            .into());
        }
    };
    let succeeded_attempt = match store
        .authorize_task_execution_start(
            succeeded_attempt.attempt_id,
            succeeded_attempt.generation,
            "controller-a",
        )
        .await?
    {
        TaskExecutionTransition::Applied(attempt) => attempt,
        transition => {
            return Err(io::Error::other(format!(
                "successful execution attempt was not started: {transition:?}"
            ))
            .into());
        }
    };
    store
        .record_task_execution_observation(
            succeeded_attempt.attempt_id,
            succeeded_attempt.generation,
            TaskExecutionObservation::Succeeded {
                adapter_observation_id: "adapter-succeeded-attempt",
                result_digest: &format!("sha256:{}", "b".repeat(64)),
                result_reference: "adapter:succeeded-attempt",
                output_archive: b"succeeded-output",
                execution_stdout: Some(b"succeeded stdout\n"),
                execution_stderr: Some(b"succeeded stderr\n"),
            },
            "controller-a",
        )
        .await?;
    store
        .insert_service_envelope(&service, &envelope("100.00", 2), "admin@example.com")
        .await?;
    assert!(matches!(
        store
            .revalidate_active_task_authority(succeeded_task_uid, 6, "controller-b")
            .await?,
        TaskOperationTransition::AuthorityInactive { current, .. }
            if current.state == TaskOrchestrationState::CleanupPending
    ));
    let succeeded_task = store
        .task(succeeded_task_uid)
        .await?
        .ok_or(StoreError::TaskNotFound)?;
    assert_eq!(
        succeeded_task.phase,
        TaskPhase::Succeeded,
        "authority loss during cleanup must not rewrite a terminal Task phase"
    );
    assert_eq!(
        store
            .agent_run_execution_log(
                succeeded_task_uid,
                Some(identity.user_id.as_str()),
                AgentRunLogStream::Stdout,
            )
            .await?,
        Some(b"succeeded stdout\n".to_vec()),
        "successful execution logs remain separate from declared Task outputs"
    );
    assert!(succeeded_task.finalize_requested);
    Ok(())
}

fn cleanup_entry_state_name(state: CleanupEntryState) -> &'static str {
    match state {
        CleanupEntryState::IntentRecorded => "intent_recorded",
        CleanupEntryState::RuntimeCreatePending => "runtime_create_pending",
        CleanupEntryState::RuntimeObserved => "runtime_observed",
        CleanupEntryState::ApprovalPending => "approval_pending",
        CleanupEntryState::ApprovalClaimed => "approval_pending",
        CleanupEntryState::ApprovalActive => "active",
        CleanupEntryState::ActivationPending => "activation_pending",
        CleanupEntryState::Active => "active",
    }
}

async fn reserve_task_at_cleanup_entry_state(
    store: &PgStore,
    service: &str,
    owner_user_id: &CanonicalUserId,
    authority: &Envelope,
    admitted_spec: &AgentRuntimeSpec,
    entry_state: CleanupEntryState,
    suffix: &str,
) -> Result<Uuid, Box<dyn Error>> {
    let mut spec = admitted_spec.clone();
    if matches!(
        entry_state,
        CleanupEntryState::ApprovalPending
            | CleanupEntryState::ApprovalClaimed
            | CleanupEntryState::ApprovalActive
    ) {
        spec.budget.monthly_limit = "300.00".to_owned();
    }
    let decision = evaluate(&spec, authority)
        .map_err(|error| io::Error::other(format!("evaluate cleanup fixture: {error:?}")))?;
    let task_uid = Uuid::new_v4();
    let operation_id = Uuid::new_v4();
    let runtime_name = format!("task-{}", operation_id.simple());
    let idempotency_key = format!("cleanup-{entry_state:?}-{suffix}");
    let command = ["agent-v1".to_owned()];
    let candidate_digest = format!("sha256:{}", "1".repeat(64));
    let envelope_digest = format!("sha256:{}", "2".repeat(64));
    let inert_digest = format!("sha256:{}", "3".repeat(64));
    store
        .reserve_task(&TaskReservationRequest {
            task_uid,
            operation_id,
            idempotency_key: &idempotency_key,
            submitter_service: service,
            acting_user: Some("alice@example.com"),
            acting_user_id: Some(owner_user_id.as_str()),
            owner: "alice@example.com",
            owner_user_id: owner_user_id.as_str(),
            workflow: "cleanup-matrix",
            workflow_name: None,
            workflow_version: None,
            workflow_digest: None,
            user_envelope_instance_id: None,
            user_envelope_revision: None,
            user_envelope_digest: None,
            coding_agent_runtime: "agent-v1",
            runtime_uid: None,
            runtime_namespace: "steward-test",
            runtime_name: &runtime_name,
            runtime_ownership: RuntimeOwnership::Provisioned,
            runtime_spec: &spec,
            agent_command: &command,
            execution_binding: None,
            direct_task_evidence: None,
            envelope_revision: authority.revision,
            service_envelope: authority,
            service_envelope_digest: &envelope_digest,
            candidate_digest: &candidate_digest,
            admission_decision: &decision,
            inert_manifest_digest: &inert_digest,
            active_manifest_digest: &candidate_digest,
        })
        .await?;
    if matches!(entry_state, CleanupEntryState::IntentRecorded) {
        return Ok(task_uid);
    }
    store
        .authorize_task_runtime_creation(task_uid, 1, "controller-a")
        .await?;
    if matches!(entry_state, CleanupEntryState::RuntimeCreatePending) {
        return Ok(task_uid);
    }
    let runtime_uid = format!("runtime-{operation_id}");
    store
        .record_task_runtime_observed(
            task_uid,
            2,
            &runtime_uid,
            "resource-version-a",
            "controller-a",
        )
        .await?;
    if matches!(entry_state, CleanupEntryState::RuntimeObserved) {
        return Ok(task_uid);
    }
    store
        .decide_task_runtime_authority(task_uid, 3, authority, &envelope_digest, "controller-a")
        .await?;
    if matches!(entry_state, CleanupEntryState::ApprovalPending) {
        return Ok(task_uid);
    }
    if matches!(entry_state, CleanupEntryState::ApprovalClaimed) {
        let approval_id = store
            .task_runtime_operation(task_uid)
            .await?
            .and_then(|operation| operation.approval_id)
            .ok_or(StoreError::ApprovalNotFound)?;
        prioritize_approval_delivery(store, approval_id).await?;
        let delivery = store
            .claim_approval_delivery("cleanup-matrix", 30)
            .await?
            .filter(|delivery| delivery.approval_id == approval_id)
            .ok_or(StoreError::ApprovalNotFound)?;
        assert_eq!(delivery.task_uid, task_uid);
        return Ok(task_uid);
    }
    if matches!(entry_state, CleanupEntryState::ApprovalActive) {
        let approval_id = store
            .task_runtime_operation(task_uid)
            .await?
            .and_then(|operation| operation.approval_id)
            .ok_or(StoreError::ApprovalNotFound)?;
        prioritize_approval_delivery(store, approval_id).await?;
        let delivery = store
            .claim_approval_delivery("cleanup-matrix", 30)
            .await?
            .filter(|delivery| delivery.approval_id == approval_id)
            .ok_or(StoreError::ApprovalNotFound)?;
        store
            .complete_approval_delivery(
                delivery.effect_id,
                delivery.generation,
                "cleanup-matrix",
                "PROJ-456",
                "https://jira.example.com/browse/PROJ-456",
            )
            .await?;
        store
            .approve_admission(ApproveAdmission {
                approval_id,
                decided_by: "admin@example.com",
                rationale: "cleanup matrix approval",
                evidence_url: "https://jira.example.com/browse/PROJ-456",
                expires_at: "2999-01-01T00:00:00Z",
            })
            .await?;
        store
            .authorize_task_activation_from_approval(task_uid, 4, "controller-a")
            .await?;
        store
            .decide_task_runtime_authority(task_uid, 5, authority, &envelope_digest, "controller-a")
            .await?;
        store
            .record_task_activation_observed(
                task_uid,
                6,
                &TaskActivationObservation {
                    runtime_uid: &runtime_uid,
                    resource_version: "resource-version-active",
                    active_manifest_digest: &candidate_digest,
                    provider_set_ready: true,
                },
                "controller-a",
            )
            .await?;
        return Ok(task_uid);
    }
    if matches!(entry_state, CleanupEntryState::ActivationPending) {
        return Ok(task_uid);
    }
    store
        .decide_task_runtime_authority(task_uid, 4, authority, &envelope_digest, "controller-a")
        .await?;
    store
        .record_task_activation_observed(
            task_uid,
            5,
            &TaskActivationObservation {
                runtime_uid: &runtime_uid,
                resource_version: "resource-version-active",
                active_manifest_digest: &candidate_digest,
                provider_set_ready: true,
            },
            "controller-a",
        )
        .await?;
    Ok(task_uid)
}

async fn prioritize_approval_delivery(
    store: &PgStore,
    approval_id: Uuid,
) -> Result<(), Box<dyn Error>> {
    sqlx::query(
        "UPDATE external_effect_outbox \
         SET created_at = '-infinity'::timestamptz, generation = generation + 1 \
         WHERE approval_id = $1",
    )
    .bind(approval_id)
    .execute(store.pool())
    .await?;
    Ok(())
}

async fn reserve_governed_connection(
    store: &PgStore,
    user_id: &CanonicalUserId,
    email: &Email,
    operation_kind: ConnectionOperationKind,
    allow_status_cache: bool,
    idempotency_identity: &str,
) -> Result<ConnectionOperationReservation, StoreError> {
    reserve_governed_connection_with_bindings(
        store,
        user_id,
        email,
        operation_kind,
        allow_status_cache,
        idempotency_identity,
        governed_connection_bindings(),
    )
    .await
}

async fn reserve_governed_connection_with_bindings(
    store: &PgStore,
    user_id: &CanonicalUserId,
    email: &Email,
    operation_kind: ConnectionOperationKind,
    allow_status_cache: bool,
    idempotency_identity: &str,
    bindings: ConnectionExecutionBindings,
) -> Result<ConnectionOperationReservation, StoreError> {
    let planned_kind = match operation_kind {
        ConnectionOperationKind::Status => PlannedConnectionOperationKind::Status,
        ConnectionOperationKind::Start => PlannedConnectionOperationKind::Start,
        ConnectionOperationKind::Disconnect => PlannedConnectionOperationKind::Disconnect,
    };
    let plan = plan_connection_operation(user_id, email, planned_kind, bindings)
        .map_err(|_| StoreError::InvalidConnectionOperation)?;
    let operation_id = sqlx::types::Uuid::new_v4();
    let runtime_name = format!("conn-{}", operation_id.simple());
    let binding_snapshot = ConnectionExecutionBindingSnapshot {
        artifact_trust_mode: plan.bindings.artifact_trust_mode.clone(),
        bridge_image_digest: plan.bindings.bridge_image_digest.clone(),
        mcp_gw_origin: plan.bindings.mcp_gw_origin.clone(),
        mcp_gw_version: plan.bindings.mcp_gw_version.clone(),
        namespace: plan.bindings.namespace.clone(),
        runtime_class: plan.bindings.runtime_class.clone(),
    };
    let service_envelope = steward_connections_v1::envelope();
    let admission = AdmissionDecision::Admit;
    let candidate_digest = format!("sha256:{}", "a".repeat(64));
    let envelope_digest = format!("sha256:{}", "b".repeat(64));
    let inert_manifest_digest = format!("sha256:{}", "c".repeat(64));
    let task_uid = Uuid::new_v4();
    let task = TaskReservationRequest {
        task_uid,
        operation_id,
        idempotency_key: idempotency_identity,
        submitter_service: CONNECTIONS_SERVICE,
        acting_user: Some(email.as_str()),
        acting_user_id: Some(user_id.as_str()),
        owner: email.as_str(),
        owner_user_id: user_id.as_str(),
        workflow: "internal:steward-connections/v1",
        workflow_name: None,
        workflow_version: None,
        workflow_digest: None,
        user_envelope_instance_id: None,
        user_envelope_revision: None,
        user_envelope_digest: None,
        coding_agent_runtime: "connections-bridge",
        runtime_uid: None,
        runtime_namespace: &binding_snapshot.namespace,
        runtime_name: &runtime_name,
        runtime_ownership: RuntimeOwnership::Provisioned,
        runtime_spec: &plan.spec,
        agent_command: &plan.command,
        execution_binding: None,
        direct_task_evidence: None,
        envelope_revision: CONNECTIONS_AUTHORITY_VERSION,
        service_envelope: &service_envelope,
        service_envelope_digest: &envelope_digest,
        candidate_digest: &candidate_digest,
        admission_decision: &admission,
        inert_manifest_digest: &inert_manifest_digest,
        active_manifest_digest: &candidate_digest,
    };
    store
        .reserve_connection_operation(&ConnectionOperationReservationRequest {
            operation_id,
            operation_kind,
            authority_id: CONNECTIONS_SERVICE,
            authority_version: CONNECTIONS_AUTHORITY_VERSION,
            authority_digest: CONNECTIONS_AUTHORITY_DIGEST,
            bindings: &binding_snapshot,
            idempotency_identity,
            response_deadline_seconds: steward_connections_v1::RESPONSE_DEADLINE_SECONDS,
            allow_status_cache,
            input_archive: &[1],
            task,
        })
        .await
}

fn google_identity(
    subject: impl AsRef<str>,
    email: impl AsRef<str>,
) -> Result<OrganizationIdentity, Box<dyn Error>> {
    google_identity_for(
        "example.com",
        OrganizationId::parse("org_example")?,
        subject,
        email,
    )
}

fn google_identity_for(
    hosted_domain: impl AsRef<str>,
    organization_id: OrganizationId,
    subject: impl AsRef<str>,
    email: impl AsRef<str>,
) -> Result<OrganizationIdentity, Box<dyn Error>> {
    let hosted_domain = hosted_domain.as_ref();
    Ok(OrganizationIdentityPolicy::new(
        "https://accounts.google.com",
        hosted_domain,
        organization_id,
    )?
    .validate(
        "https://accounts.google.com",
        subject.as_ref(),
        hosted_domain,
        email.as_ref(),
        true,
    )?)
}

fn proposed_spec() -> AgentRuntimeSpec {
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
            provider: "provider-a".to_owned(),
            model: "model-a".to_owned(),
        }],
        tools: Vec::new(),
        budget: Budget {
            monthly_limit: "220.00".to_owned(),
            single_run_limit: None,
            currency: "USD".to_owned(),
        },
        ttl: Duration("24h".to_owned()),
        runner: RunnerRequirements::default(),
        bindings: None,
    }
}

fn direct_task_evidence(
    task_uid: Uuid,
    envelope_request_id: Uuid,
    envelope_revision: i64,
    envelope_digest: &str,
) -> Result<DirectTaskBindingEvidence, Box<dyn Error>> {
    let mut value = serde_json::from_str::<serde_json::Value>(include_str!(
        "../docs/contracts/task/v2/fixtures/positive/task-binding-evidence.json"
    ))?;
    value["taskUid"] = task_uid.to_string().into();
    value["envelope"]["uid"] = envelope_request_id.to_string().into();
    value["envelope"]["revision"] = envelope_revision.into();
    value["envelope"]["digest"] = format!("steward:{envelope_digest}").into();
    Ok(serde_json::from_value(value)?)
}

struct ActiveDirectTaskFixture {
    store: PgStore,
    suffix: String,
    service: String,
    member_role: String,
    owner_user_id: CanonicalUserId,
    owner: Email,
    authority: Envelope,
    envelope_request_id: Uuid,
    envelope_instance_id: String,
    envelope_digest: String,
    service_envelope_digest: String,
}

impl ActiveDirectTaskFixture {
    async fn new(database_url: &str, label: &str) -> Result<Self, Box<dyn Error>> {
        let store = isolated_approval_queue_store(database_url).await?;
        let suffix = format!(
            "{label}-{}",
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        );
        let service = format!("direct-envelope-{suffix}");
        let member_role = format!("direct-role-{suffix}");
        let identity = store
            .register_canonical_identity(
                &google_identity(
                    format!("direct-envelope-subject-{suffix}"),
                    format!("alice-{suffix}@example.com"),
                )?,
                "test-bootstrap",
            )
            .await?;
        let authority = envelope("250.00", 1);
        store
            .insert_service_envelope(&service, &authority, "admin@example.com")
            .await?;
        store
            .insert_envelope(&member_role, &authority, "admin@example.com")
            .await?;
        let envelope_request = store
            .reserve_envelope_request(EnvelopeRequestReservationRequest {
                owner_user_id: &identity.user_id,
                template_id: &member_role,
                template_revision: authority.revision,
                requested_envelope: &authority,
                idempotency_key: &format!("direct-envelope-request-{suffix}"),
                actor: identity.user_id.as_str(),
            })
            .await?;
        let envelope_instance_id = format!("env_direct_{suffix}");
        let envelope_digest = format!("sha256:{}", "b".repeat(64));
        store
            .append_envelope_request_status(
                envelope_request.record.id,
                EnvelopeRequestStatusUpdate {
                    from: EnvelopeRequestStatus::Pending,
                    to: EnvelopeRequestStatus::Provisioned,
                    approval_id: None,
                    envelope_instance_id: Some(&envelope_instance_id),
                    envelope_digest: Some(&envelope_digest),
                    reason: None,
                    approved_envelope: Some(&authority),
                    actor: identity.user_id.as_str(),
                },
            )
            .await?;
        Ok(Self {
            store,
            suffix,
            service,
            member_role,
            owner_user_id: identity.user_id,
            owner: identity.display_email,
            authority,
            envelope_request_id: envelope_request.record.id,
            envelope_instance_id,
            envelope_digest,
            service_envelope_digest: format!("sha256:{}", "e".repeat(64)),
        })
    }

    async fn try_reserve_task(
        &self,
        label: &str,
    ) -> Result<Result<steward_store::TaskReservation, StoreError>, Box<dyn Error>> {
        let task_uid = Uuid::new_v4();
        let operation_id = Uuid::new_v4();
        let runtime_name = format!("task-{}", operation_id.simple());
        let mut spec = proposed_spec();
        spec.principal = Principal::Service {
            name: self.service.clone(),
            acting_user: Some(self.owner.clone()),
        };
        spec.owner = self.owner.clone();
        spec.canonical_authority = Some(CanonicalAuthorityBinding::new(
            self.owner_user_id.clone(),
            Some(self.owner_user_id.clone()),
        )?);
        let decision = evaluate(&spec, &self.authority)
            .map_err(|error| io::Error::other(format!("evaluate direct Task: {error:?}")))?;
        assert_eq!(decision, AdmissionDecision::Admit);
        let evidence = direct_task_evidence(
            task_uid,
            self.envelope_request_id,
            self.authority.revision,
            &self.envelope_digest,
        )?;
        let command = vec!["agent-v1".to_owned()];
        let candidate_digest = format!("sha256:{}", "c".repeat(64));
        let inert_digest = format!("sha256:{}", "d".repeat(64));
        Ok(self
            .store
            .reserve_task(&TaskReservationRequest {
                task_uid,
                operation_id,
                idempotency_key: &format!("direct-task-{label}-{}", self.suffix),
                submitter_service: &self.service,
                acting_user: Some(self.owner.as_str()),
                acting_user_id: Some(self.owner_user_id.as_str()),
                owner: self.owner.as_str(),
                owner_user_id: self.owner_user_id.as_str(),
                workflow: "direct:release-summary@v1",
                workflow_name: None,
                workflow_version: None,
                workflow_digest: None,
                user_envelope_instance_id: Some(&self.envelope_instance_id),
                user_envelope_revision: Some(self.authority.revision),
                user_envelope_digest: Some(&self.envelope_digest),
                coding_agent_runtime: "agent-v1",
                runtime_uid: None,
                runtime_namespace: "steward-test",
                runtime_name: &runtime_name,
                runtime_ownership: RuntimeOwnership::Provisioned,
                runtime_spec: &spec,
                agent_command: &command,
                execution_binding: None,
                direct_task_evidence: Some(&evidence),
                envelope_revision: self.authority.revision,
                service_envelope: &self.authority,
                service_envelope_digest: &self.service_envelope_digest,
                candidate_digest: &candidate_digest,
                admission_decision: &decision,
                inert_manifest_digest: &inert_digest,
                active_manifest_digest: &candidate_digest,
            })
            .await)
    }

    async fn reserve_task(&self, label: &str) -> Result<Uuid, Box<dyn Error>> {
        let reservation = self.try_reserve_task(label).await??;
        assert!(reservation.inserted);
        Ok(reservation.record.task_uid)
    }

    async fn supersede_envelope(&self, label: &str) -> Result<(), Box<dyn Error>> {
        let replacement = self
            .store
            .reserve_envelope_request(EnvelopeRequestReservationRequest {
                owner_user_id: &self.owner_user_id,
                template_id: &self.member_role,
                template_revision: self.authority.revision,
                requested_envelope: &self.authority,
                idempotency_key: &format!("replacement-{label}-{}", self.suffix),
                actor: self.owner_user_id.as_str(),
            })
            .await?;
        let replacement_instance_id = format!("env_direct_replacement_{label}_{}", self.suffix);
        // Reusing the same approved bytes and digest must still revoke the prior exact
        // request/instance selection.
        let replacement_digest = self.envelope_digest.clone();
        self.store
            .append_envelope_request_status(
                replacement.record.id,
                EnvelopeRequestStatusUpdate {
                    from: EnvelopeRequestStatus::Pending,
                    to: EnvelopeRequestStatus::Provisioned,
                    approval_id: None,
                    envelope_instance_id: Some(&replacement_instance_id),
                    envelope_digest: Some(&replacement_digest),
                    reason: None,
                    approved_envelope: Some(&self.authority),
                    actor: self.owner_user_id.as_str(),
                },
            )
            .await?;
        Ok(())
    }

    async fn activate_task(&self, task_uid: Uuid, runtime_uid: &str) -> Result<(), Box<dyn Error>> {
        assert!(matches!(
            self.store
                .authorize_task_runtime_creation(task_uid, 1, "controller-a")
                .await?,
            TaskOperationTransition::Applied(_)
        ));
        assert!(matches!(
            self.store
                .record_task_runtime_observed(
                    task_uid,
                    2,
                    runtime_uid,
                    "resource-version-a",
                    "controller-a",
                )
                .await?,
            TaskOperationTransition::Applied(_)
        ));
        assert!(matches!(
            self.store
                .decide_task_runtime_authority(
                    task_uid,
                    3,
                    &self.authority,
                    &self.service_envelope_digest,
                    "controller-a",
                )
                .await?,
            TaskOperationTransition::Applied(_)
        ));
        assert!(matches!(
            self.store
                .decide_task_runtime_authority(
                    task_uid,
                    4,
                    &self.authority,
                    &self.service_envelope_digest,
                    "controller-a",
                )
                .await?,
            TaskOperationTransition::Applied(_)
        ));
        assert!(matches!(
            self.store
                .record_task_activation_observed(
                    task_uid,
                    5,
                    &TaskActivationObservation {
                        runtime_uid,
                        resource_version: "resource-version-active",
                        active_manifest_digest: &format!("sha256:{}", "c".repeat(64)),
                        provider_set_ready: true,
                    },
                    "controller-a",
                )
                .await?,
            TaskOperationTransition::Applied(_)
        ));
        Ok(())
    }
}

#[tokio::test]
async fn direct_task_stale_user_envelope_fences_attempt_and_start() -> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the Task Postgres test")
    })?;

    let before_attempt = ActiveDirectTaskFixture::new(&database_url, "before-attempt").await?;
    let attempt_task = before_attempt.reserve_task("attempt").await?;
    before_attempt
        .activate_task(attempt_task, "runtime-before-attempt")
        .await?;
    before_attempt
        .store
        .put_task_inputs(
            attempt_task,
            &before_attempt.service,
            before_attempt.owner_user_id.as_str(),
            b"fixture-input",
        )
        .await?;
    before_attempt
        .store
        .request_task_execution(
            attempt_task,
            &before_attempt.service,
            before_attempt.owner_user_id.as_str(),
        )
        .await?;
    before_attempt.supersede_envelope("attempt").await?;
    assert!(matches!(
        before_attempt
            .store
            .claim_task_execution_attempt(
                attempt_task,
                &format!("sha256:{}", "1".repeat(64)),
                &format!("sha256:{}", "2".repeat(64)),
                "controller-a",
            )
            .await?,
        TaskExecutionTransition::AuthorityInactive { attempt: None, .. }
    ));
    assert!(
        before_attempt
            .store
            .task_execution_attempt(attempt_task)
            .await?
            .is_none()
    );

    let before_start = ActiveDirectTaskFixture::new(&database_url, "before-start").await?;
    let start_task = before_start.reserve_task("start").await?;
    before_start
        .activate_task(start_task, "runtime-before-start")
        .await?;
    before_start
        .store
        .put_task_inputs(
            start_task,
            &before_start.service,
            before_start.owner_user_id.as_str(),
            b"fixture-input",
        )
        .await?;
    before_start
        .store
        .request_task_execution(
            start_task,
            &before_start.service,
            before_start.owner_user_id.as_str(),
        )
        .await?;
    let attempt = match before_start
        .store
        .claim_task_execution_attempt(
            start_task,
            &format!("sha256:{}", "1".repeat(64)),
            &format!("sha256:{}", "2".repeat(64)),
            "controller-a",
        )
        .await?
    {
        TaskExecutionTransition::Created(attempt) => attempt,
        other => return Err(io::Error::other(format!("unexpected attempt: {other:?}")).into()),
    };
    before_start.supersede_envelope("start").await?;
    let transition = before_start
        .store
        .authorize_task_execution_start(attempt.attempt_id, 1, "controller-a")
        .await?;
    assert!(matches!(
        transition,
        TaskExecutionTransition::AuthorityInactive { .. }
    ));
    let fenced = before_start
        .store
        .task_execution_attempt(start_task)
        .await?
        .ok_or(StoreError::TaskNotFound)?;
    assert!(fenced.start_invoked_at.is_none());
    assert_eq!(fenced.state, TaskExecutionAttemptState::NotStarted);
    assert!(
        !before_start
            .store
            .task_execution_holds_runtime_lease(fenced.attempt_id)
            .await?
    );
    let task = before_start
        .store
        .task(start_task)
        .await?
        .ok_or(StoreError::TaskNotFound)?;
    assert_eq!(task.phase, TaskPhase::Failed);
    assert!(task.finalize_requested);
    Ok(())
}

#[tokio::test]
async fn direct_task_reservation_revalidates_exact_user_envelope() -> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the Task Postgres test")
    })?;
    let fixture = ActiveDirectTaskFixture::new(&database_url, "before-reserve").await?;
    fixture.supersede_envelope("same-content").await?;
    assert!(matches!(
        fixture.try_reserve_task("stale").await?,
        Err(StoreError::StaleEnvelope)
    ));
    let task_count = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM task_submissions")
        .fetch_one(fixture.store.pool())
        .await?;
    let operation_count =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM task_runtime_operations")
            .fetch_one(fixture.store.pool())
            .await?;
    assert_eq!(task_count, 0);
    assert_eq!(operation_count, 0);
    Ok(())
}

#[tokio::test]
async fn direct_task_runtime_create_pending_revalidates_before_effect() -> Result<(), Box<dyn Error>>
{
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the Task Postgres test")
    })?;
    let fixture = ActiveDirectTaskFixture::new(&database_url, "create-pending").await?;
    let task_uid = fixture.reserve_task("create-pending").await?;
    assert!(matches!(
        fixture
            .store
            .authorize_task_runtime_creation(task_uid, 1, "controller-a")
            .await?,
        TaskOperationTransition::Applied(_)
    ));
    fixture.supersede_envelope("create-pending").await?;
    assert!(matches!(
        fixture
            .store
            .authorize_task_runtime_creation(task_uid, 2, "controller-b")
            .await?,
        TaskOperationTransition::AuthorityInactive { .. }
    ));
    let operation = fixture
        .store
        .task_runtime_operation(task_uid)
        .await?
        .ok_or(StoreError::TaskNotFound)?;
    assert_eq!(operation.state, TaskOrchestrationState::CleanupPending);
    assert!(operation.runtime_uid.is_none());
    assert!(matches!(
        fixture
            .store
            .authorize_task_runtime_creation(task_uid, 2, "controller-c")
            .await?,
        TaskOperationTransition::Superseded(_)
    ));
    Ok(())
}

#[tokio::test]
async fn direct_task_runtime_authority_rejects_stale_exact_user_envelope()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the Task Postgres test")
    })?;
    let fixture = ActiveDirectTaskFixture::new(&database_url, "runtime-observed").await?;
    let task_uid = fixture.reserve_task("runtime-observed").await?;
    assert!(matches!(
        fixture
            .store
            .authorize_task_runtime_creation(task_uid, 1, "controller-a")
            .await?,
        TaskOperationTransition::Applied(_)
    ));
    assert!(matches!(
        fixture
            .store
            .record_task_runtime_observed(
                task_uid,
                2,
                "runtime-observed-uid",
                "resource-version-observed",
                "controller-a",
            )
            .await?,
        TaskOperationTransition::Applied(_)
    ));
    fixture.supersede_envelope("runtime-observed").await?;
    assert!(matches!(
        fixture
            .store
            .decide_task_runtime_authority(
                task_uid,
                3,
                &fixture.authority,
                &fixture.service_envelope_digest,
                "controller-b",
            )
            .await?,
        TaskOperationTransition::AuthorityInactive { .. }
    ));
    let approval_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM approvals WHERE id IN (\
             SELECT approval_id FROM external_effect_outbox WHERE task_uid = $1)",
    )
    .bind(task_uid)
    .fetch_one(fixture.store.pool())
    .await?;
    let effect_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM external_effect_outbox WHERE task_uid = $1",
    )
    .bind(task_uid)
    .fetch_one(fixture.store.pool())
    .await?;
    assert_eq!(approval_count, 0);
    assert_eq!(effect_count, 0);
    Ok(())
}

#[tokio::test]
async fn direct_task_stale_user_envelope_terminalizes_before_runtime_creation()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the Task Postgres test")
    })?;
    let fixture = ActiveDirectTaskFixture::new(&database_url, "before-create").await?;
    let task_uid = fixture.reserve_task("before-create").await?;
    fixture.supersede_envelope("before-create").await?;
    let transition = fixture
        .store
        .authorize_task_runtime_creation(task_uid, 1, "controller-a")
        .await?;
    assert!(matches!(
        transition,
        TaskOperationTransition::AuthorityInactive { .. }
    ));
    let operation = fixture
        .store
        .task_runtime_operation(task_uid)
        .await?
        .ok_or(StoreError::TaskNotFound)?;
    assert_eq!(operation.state, TaskOrchestrationState::CleanupPending);
    assert!(operation.runtime_create_authorized_at.is_none());
    let task = fixture
        .store
        .task(task_uid)
        .await?
        .ok_or(StoreError::TaskNotFound)?;
    assert_eq!(task.phase, TaskPhase::Failed);
    assert!(task.finalize_requested);
    assert_eq!(
        task.failure_reason.as_deref(),
        Some("user_envelope_inactive")
    );

    assert!(matches!(
        fixture
            .store
            .authorize_task_runtime_creation(task_uid, 1, "controller-b")
            .await?,
        TaskOperationTransition::Superseded(_)
    ));
    assert!(matches!(
        fixture
            .store
            .record_task_cleanup_complete(
                task_uid,
                2,
                TaskCleanupObservation {
                    exact_runtime_absent: false,
                },
                "controller-b",
            )
            .await?,
        TaskOperationTransition::Applied(_)
    ));
    let finalized = fixture
        .store
        .task_runtime_operation(task_uid)
        .await?
        .ok_or(StoreError::TaskNotFound)?;
    assert_eq!(finalized.state, TaskOrchestrationState::Finalized);
    let inactive_events = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM task_orchestration_journal \
         WHERE task_uid = $1 AND event_kind = 'runtime_creation_user_envelope_inactive'",
    )
    .bind(task_uid)
    .fetch_one(fixture.store.pool())
    .await?;
    assert_eq!(inactive_events, 1);
    Ok(())
}

#[tokio::test]
async fn canonical_identity_requires_exact_subject_mapping_and_explicit_reconnect()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the identity Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let organization = OrganizationId::parse("org_example")?;
    let email = Email::parse(format!("identity-{suffix}@example.com"))?;
    let google = google_identity(format!("google-subject-{suffix}"), email.as_str())?;

    let principal = store
        .register_canonical_identity(&google, "identity-admin")
        .await?;
    assert_eq!(
        store.resolve_canonical_identity(&google).await?,
        principal,
        "an exact reviewed Google issuer/subject/hosted-domain mapping must resolve"
    );
    assert_eq!(
        store
            .resolve_canonical_principal(&principal.user_id, &email)
            .await?,
        principal,
        "a trusted workload reference must resolve by opaque ID plus current display email"
    );
    assert_eq!(
        store
            .register_canonical_identity(&google, "identity-admin")
            .await?,
        principal,
        "exact repeat registration must be idempotent"
    );

    let changed_subject =
        google_identity(format!("different-google-subject-{suffix}"), email.as_str())?;
    assert_eq!(
        store
            .register_canonical_identity(&changed_subject, "identity-admin")
            .await,
        Err(StoreError::CanonicalIdentityAmbiguousEmail),
        "an email match must never silently adopt a different subject"
    );

    let renamed_email = Email::parse(format!("identity-renamed-{suffix}@example.com"))?;
    let renamed_google = google_identity(google.subject(), renamed_email.as_str())?;
    assert_eq!(
        store.resolve_canonical_identity(&renamed_google).await,
        Err(StoreError::CanonicalIdentityStale),
        "a changed email requires an explicit audited reconnect"
    );
    assert_eq!(
        store
            .resolve_canonical_principal(&principal.user_id, &renamed_email)
            .await,
        Err(StoreError::CanonicalIdentityStale),
        "a workload mapper cannot pair an existing user ID with an unreviewed email"
    );
    store
        .change_canonical_identity_email(
            &principal.user_id,
            &email,
            &renamed_email,
            "identity-admin",
        )
        .await?;
    assert_eq!(
        store
            .resolve_canonical_identity(&renamed_google)
            .await?
            .user_id,
        principal.user_id
    );
    assert_eq!(
        store
            .resolve_canonical_principal(&principal.user_id, &renamed_email)
            .await?
            .user_id,
        principal.user_id
    );

    let future_issuer = OrganizationIdentityMigration::new_reviewed(
        "https://login.example.test",
        format!("future-subject-{suffix}"),
        "example.com",
        organization,
        renamed_email,
    )?;
    let migrated = store
        .attach_canonical_identity_subject(&principal.user_id, &future_issuer, "identity-admin")
        .await?;
    assert_eq!(migrated.user_id, principal.user_id);
    assert_eq!(
        store
            .attach_canonical_identity_subject(
                &principal.user_id,
                &future_issuer,
                "identity-admin",
            )
            .await?,
        migrated,
        "retrying an exact attachment to the same canonical user must be idempotent"
    );
    assert_eq!(
        store
            .resolve_migrated_canonical_identity(&future_issuer)
            .await?
            .user_id,
        principal.user_id,
        "an explicitly reviewed issuer migration must preserve the opaque user ID"
    );
    Ok(())
}

#[tokio::test]
async fn browser_rbac_is_canonical_user_scoped_append_only_and_revocable()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the browser RBAC Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let user = store
        .register_canonical_identity(
            &google_identity(
                format!("browser-rbac-user-{suffix}"),
                format!("browser-rbac-user-{suffix}@example.com"),
            )?,
            "identity-admin",
        )
        .await?
        .user_id;
    let other_user = store
        .register_canonical_identity(
            &google_identity(
                format!("browser-rbac-other-{suffix}"),
                format!("browser-rbac-other-{suffix}@example.com"),
            )?,
            "identity-admin",
        )
        .await?
        .user_id;
    let administrator = BrowserRbacAssignment::Administrator;
    let engineer = BrowserRbacAssignment::MemberRole("engineer".to_owned());
    for assignment in [&administrator, &engineer] {
        store
            .append_browser_rbac_assignment(BrowserRbacAssignmentChange {
                user_id: &user,
                assignment,
                action: BrowserRbacAssignmentAction::Grant,
                actor: "rbac-operator",
            })
            .await?;
    }
    assert_eq!(
        store.browser_rbac_assignments(&user).await?.member_roles,
        ["engineer"],
        "the canonical user receives only their explicit member-role assignment"
    );
    assert!(store.browser_rbac_assignments(&user).await?.is_admin);
    assert_eq!(
        store.browser_rbac_assignments(&other_user).await?,
        Default::default(),
        "a role event for one canonical user cannot grant another user authority"
    );

    store
        .append_browser_rbac_assignment(BrowserRbacAssignmentChange {
            user_id: &user,
            assignment: &engineer,
            action: BrowserRbacAssignmentAction::Revoke,
            actor: "rbac-operator",
        })
        .await?;
    let after_revoke = store.browser_rbac_assignments(&user).await?;
    assert!(
        after_revoke.is_admin,
        "an unrelated administrator grant remains active"
    );
    assert!(after_revoke.member_roles.is_empty());
    let mutation = sqlx::query(
        "UPDATE browser_rbac_assignment_events SET actor = 'mutation-attempt' WHERE user_id = $1",
    )
    .bind(user.as_str())
    .execute(store.pool())
    .await;
    assert!(
        mutation.is_err(),
        "RBAC history must be revoked by an appended event, never modified in place"
    );
    Ok(())
}

#[tokio::test]
async fn canonical_external_subject_is_globally_unique_across_google_organizations()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the identity Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let shared_subject = format!("google-shared-subject-{suffix}");
    let first = google_identity_for(
        "example.com",
        OrganizationId::parse("org_example")?,
        &shared_subject,
        format!("first-{suffix}@example.com"),
    )?;
    let other_organization = google_identity_for(
        "other.example",
        OrganizationId::parse("org_other")?,
        &shared_subject,
        format!("second-{suffix}@other.example"),
    )?;

    let first_principal = store
        .register_canonical_identity(&first, "identity-admin")
        .await?;
    assert_eq!(
        store
            .register_canonical_identity(&other_organization, "identity-admin")
            .await,
        Err(StoreError::CanonicalIdentityConflict),
        "one Google (issuer, subject) pair must not identify people in two organizations"
    );
    assert_eq!(
        store.resolve_canonical_identity(&first).await?,
        first_principal
    );
    assert_eq!(
        store.resolve_canonical_identity(&other_organization).await,
        Err(StoreError::CanonicalIdentityNotFound)
    );
    let pair_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM canonical_identity_subjects WHERE issuer = $1 AND subject = $2",
    )
    .bind(first.issuer())
    .bind(first.subject())
    .fetch_one(store.pool())
    .await?;
    assert_eq!(
        pair_count, 1,
        "the external pair must have exactly one owner"
    );
    Ok(())
}

#[tokio::test]
async fn canonical_external_subject_cannot_be_attached_to_another_user()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the identity Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let shared_subject = format!("migrated-attach-subject-{suffix}");
    let first = google_identity_for(
        "example.com",
        OrganizationId::parse("org_example")?,
        format!("google-first-subject-{suffix}"),
        format!("first-attach-{suffix}@example.com"),
    )?;
    let second = google_identity_for(
        "other.example",
        OrganizationId::parse("org_other")?,
        format!("google-second-subject-{suffix}"),
        format!("second-attach-{suffix}@other.example"),
    )?;
    let first_principal = store
        .register_canonical_identity(&first, "identity-admin")
        .await?;
    let first_attachment = OrganizationIdentityMigration::new_reviewed(
        "https://login.example.test",
        &shared_subject,
        "example.com",
        OrganizationId::parse("org_example")?,
        Email::parse(format!("first-attach-{suffix}@example.com"))?,
    )?;
    store
        .attach_canonical_identity_subject(
            &first_principal.user_id,
            &first_attachment,
            "identity-admin",
        )
        .await?;
    let second_principal = store
        .register_canonical_identity(&second, "identity-admin")
        .await?;
    let conflicting_attachment = OrganizationIdentityMigration::new_reviewed(
        "https://login.example.test",
        &shared_subject,
        "other.example",
        OrganizationId::parse("org_other")?,
        Email::parse(format!("second-attach-{suffix}@other.example"))?,
    )?;

    assert_eq!(
        store
            .attach_canonical_identity_subject(
                &second_principal.user_id,
                &conflicting_attachment,
                "identity-admin",
            )
            .await,
        Err(StoreError::CanonicalIdentityConflict),
        "an attachment must not move an external pair to another canonical user"
    );
    Ok(())
}

#[tokio::test]
async fn concurrent_exact_registration_converges_on_one_canonical_user()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the identity Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let identity = google_identity(
        format!("google-concurrent-subject-{suffix}"),
        format!("concurrent-{suffix}@example.com"),
    )?;

    let left_store = store.clone();
    let right_store = store.clone();
    let (left, right) = tokio::join!(
        left_store.register_canonical_identity(&identity, "identity-admin-left"),
        right_store.register_canonical_identity(&identity, "identity-admin-right"),
    );
    let left = left?;
    let right = right?;
    assert_eq!(
        left, right,
        "concurrent exact registrations must resolve to one canonical user"
    );
    let pair_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM canonical_identity_subjects WHERE issuer = $1 AND subject = $2",
    )
    .bind(identity.issuer())
    .bind(identity.subject())
    .fetch_one(store.pool())
    .await?;
    assert_eq!(pair_count, 1, "the external pair must be persisted once");
    Ok(())
}

#[tokio::test]
async fn alternative_issuer_migration_cannot_allocate_a_canonical_user()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the identity Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let migration = OrganizationIdentityMigration::new_reviewed(
        "https://login.example.test",
        format!("migration-only-subject-{suffix}"),
        "example.com",
        OrganizationId::parse("org_example")?,
        Email::parse(format!("migration-only-{suffix}@example.com"))?,
    )?;
    let missing_user =
        steward_types::CanonicalUserId::parse("usr_00000000000000000000000000000000")?;
    let unrelated_identity = google_identity(
        format!("unrelated-concurrent-subject-{suffix}"),
        format!("unrelated-concurrent-{suffix}@example.com"),
    )?;
    let migration_store = store.clone();
    let unrelated_store = store.clone();
    let (migration_result, unrelated_result) = tokio::join!(
        migration_store.attach_canonical_identity_subject(
            &missing_user,
            &migration,
            "identity-admin",
        ),
        unrelated_store.register_canonical_identity(&unrelated_identity, "identity-admin"),
    );

    assert_eq!(
        migration_result,
        Err(StoreError::CanonicalIdentityNotFound),
        "a migration can attach only to an existing canonical user"
    );
    let unrelated_principal = unrelated_result?;
    assert_eq!(
        store
            .resolve_canonical_identity(&unrelated_identity)
            .await?,
        unrelated_principal,
        "the unrelated concurrent registration must complete"
    );
    let migration_user_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM canonical_users \
         WHERE organization_id = $1 AND lower(display_email) = lower($2)",
    )
    .bind(migration.organization_id().as_str())
    .bind(migration.verified_email().as_str())
    .fetch_one(store.pool())
    .await?;
    let migration_subject_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM canonical_identity_subjects WHERE issuer = $1 AND subject = $2",
    )
    .bind(migration.issuer())
    .bind(migration.subject())
    .fetch_one(store.pool())
    .await?;
    assert_eq!(
        migration_user_count, 0,
        "migration must not allocate its requested user"
    );
    assert_eq!(
        migration_subject_count, 0,
        "migration must not allocate its requested external-subject mapping"
    );
    Ok(())
}

#[tokio::test]
async fn canonical_subject_row_rejects_and_detects_wrong_user_organization()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the identity Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(3)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let owner_identity = google_identity(
        format!("organization-owner-subject-{suffix}"),
        format!("organization-owner-{suffix}@example.com"),
    )?;
    let owner = store
        .register_canonical_identity(&owner_identity, "identity-admin")
        .await?;
    let wrong_organization_identity = google_identity_for(
        "other.example",
        OrganizationId::parse("org_other")?,
        format!("wrong-organization-subject-{suffix}"),
        format!("wrong-organization-{suffix}@other.example"),
    )?;
    let insert_wrong_organization = sqlx::query(
        "INSERT INTO canonical_identity_subjects \
         (issuer, subject, organization_claim, organization_id, user_id, verified_email) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(wrong_organization_identity.issuer())
    .bind(wrong_organization_identity.subject())
    .bind(wrong_organization_identity.organization_claim())
    .bind(wrong_organization_identity.organization_id().as_str())
    .bind(owner.user_id.as_str())
    .bind(wrong_organization_identity.verified_email().as_str())
    .execute(store.pool())
    .await;
    assert!(
        insert_wrong_organization.is_err(),
        "the database must reject a subject row whose organization differs from its user"
    );

    // Exercise the resolver defense independently of the FK by simulating a pre-constraint
    // corrupt row. This is test-only superuser state and is restored before the connection is
    // returned to the pool.
    let mut corrupt_row = store.pool().begin().await?;
    sqlx::query("SET LOCAL session_replication_role = replica")
        .execute(&mut *corrupt_row)
        .await?;
    sqlx::query(
        "INSERT INTO canonical_identity_subjects \
         (issuer, subject, organization_claim, organization_id, user_id, verified_email) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(wrong_organization_identity.issuer())
    .bind(wrong_organization_identity.subject())
    .bind(wrong_organization_identity.organization_claim())
    .bind(wrong_organization_identity.organization_id().as_str())
    .bind(owner.user_id.as_str())
    .bind(wrong_organization_identity.verified_email().as_str())
    .execute(&mut *corrupt_row)
    .await?;
    corrupt_row.commit().await?;

    assert_eq!(
        store
            .resolve_canonical_identity(&wrong_organization_identity)
            .await,
        Err(StoreError::CanonicalIdentityInvalidRecord),
        "resolution must fail closed even if a corrupt row bypassed the schema constraint"
    );
    Ok(())
}

#[tokio::test]
async fn task_submission_state_is_idempotent_durable_and_single_claimed()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the Task Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let idempotency_key = format!("job-{suffix}");
    let runtime_uid = format!("runtime-{suffix}");
    let canonical = store
        .register_canonical_identity(
            &google_identity(
                format!("google-subject-{suffix}"),
                format!("alice-{suffix}@example.com"),
            )?,
            "test-bootstrap",
        )
        .await?;
    let mut spec = proposed_spec();
    spec.principal = Principal::Service {
        name: "steward-run".to_owned(),
        acting_user: Some(Email("alice@example.com".to_owned())),
    };
    spec.canonical_authority = Some(CanonicalAuthorityBinding::new(
        canonical.user_id.clone(),
        Some(canonical.user_id.clone()),
    )?);
    let service_envelope = envelope("250.00", 1);
    store
        .insert_service_envelope("steward-run", &service_envelope, "admin@example.com")
        .await?;
    let admission = AdmissionDecision::Admit;
    let candidate_digest = format!("sha256:{}", "d".repeat(64));
    let service_envelope_digest = format!("sha256:{}", "e".repeat(64));
    let inert_manifest_digest = format!("sha256:{}", "f".repeat(64));
    let command = vec!["agent-v1".to_owned()];
    let legacy = sqlx::query(
        "INSERT INTO task_submissions \
         (task_uid, idempotency_key, submitter_service, acting_user, owner, workflow, \
          coding_agent_runtime, runtime_namespace, runtime_name, runtime_ownership, phase, \
          runtime_spec, agent_command) \
         VALUES (gen_random_uuid(), $1, 'legacy-service', 'legacy@example.com', \
                 'legacy@example.com', 'legacy-workflow', 'legacy-runtime', 'legacy', $2, \
                 'provisioned', 'submitted', '{}'::jsonb, '[]'::jsonb) \
         RETURNING identity_binding_state, acting_user_id, owner_user_id, runtime_spec",
    )
    .bind(format!("legacy-{suffix}"))
    .bind(format!("legacy-{suffix}"))
    .fetch_one(store.pool())
    .await;
    assert!(
        legacy.is_err(),
        "the staged rollout fence must reject a legacy writer after durable orchestration is installed"
    );

    let task_uid = Uuid::new_v4();
    let operation_id = Uuid::new_v4();
    let runtime_name = format!("task-{}", operation_id.simple());
    let request = TaskReservationRequest {
        task_uid,
        operation_id,
        idempotency_key: &idempotency_key,
        submitter_service: "steward-run",
        acting_user: Some("alice@example.com"),
        acting_user_id: Some(canonical.user_id.as_str()),
        owner: "alice@example.com",
        owner_user_id: canonical.user_id.as_str(),
        workflow: "code-review",
        workflow_name: None,
        workflow_version: None,
        workflow_digest: None,
        user_envelope_instance_id: None,
        user_envelope_revision: None,
        user_envelope_digest: None,
        coding_agent_runtime: "agent-v1",
        runtime_uid: None,
        runtime_namespace: "team-a",
        runtime_name: &runtime_name,
        runtime_ownership: RuntimeOwnership::Provisioned,
        runtime_spec: &spec,
        agent_command: &command,
        execution_binding: None,
        direct_task_evidence: None,
        envelope_revision: 1,
        service_envelope: &service_envelope,
        service_envelope_digest: &service_envelope_digest,
        candidate_digest: &candidate_digest,
        admission_decision: &admission,
        inert_manifest_digest: &inert_manifest_digest,
        active_manifest_digest: &candidate_digest,
    };
    let first = store.reserve_task(&request).await?;
    assert!(first.inserted);
    assert_eq!(
        first.record.runtime_spec.canonical_authority, spec.canonical_authority,
        "the stable authority binding must survive Task persistence"
    );
    let second = store.reserve_task(&request).await?;
    assert!(!second.inserted);
    assert_eq!(second.record.task_uid, first.record.task_uid);

    store
        .insert_service_envelope("steward-run", &envelope("250.00", 2), "admin@example.com")
        .await?;
    assert!(matches!(
        store
            .authorize_task_runtime_creation(first.record.task_uid, 1, "controller-a")
            .await?,
        TaskOperationTransition::Applied(_)
    ));
    assert!(matches!(
        store
            .record_task_runtime_observed(
                first.record.task_uid,
                2,
                &runtime_uid,
                "resource-version-a",
                "controller-a",
            )
            .await?,
        TaskOperationTransition::Applied(_)
    ));
    let latest_envelope = envelope("250.00", 2);
    assert!(matches!(
        store
            .decide_task_runtime_authority(
                first.record.task_uid,
                3,
                &latest_envelope,
                &service_envelope_digest,
                "controller-a",
            )
            .await?,
        TaskOperationTransition::Applied(_)
    ));
    assert!(matches!(
        store
            .decide_task_runtime_authority(
                first.record.task_uid,
                4,
                &latest_envelope,
                &service_envelope_digest,
                "controller-a",
            )
            .await?,
        TaskOperationTransition::Applied(_)
    ));
    assert!(matches!(
        store
            .record_task_activation_observed(
                first.record.task_uid,
                5,
                &TaskActivationObservation {
                    runtime_uid: &runtime_uid,
                    resource_version: "resource-version-active",
                    active_manifest_digest: &candidate_digest,
                    provider_set_ready: true,
                },
                "controller-a",
            )
            .await?,
        TaskOperationTransition::Applied(_)
    ));
    let adopted_key = format!("adopted-{suffix}");
    let adopted_runtime_uid = format!("adopted-runtime-{suffix}");
    let adopted_task_uid = Uuid::new_v4();
    let adopted_operation_id = Uuid::new_v4();
    let adopted_request = TaskReservationRequest {
        task_uid: adopted_task_uid,
        operation_id: adopted_operation_id,
        idempotency_key: &adopted_key,
        envelope_revision: latest_envelope.revision,
        service_envelope: &latest_envelope,
        runtime_uid: Some(&adopted_runtime_uid),
        runtime_ownership: RuntimeOwnership::Adopted,
        ..request
    };
    let adopted = store.reserve_task(&adopted_request).await?;
    assert!(adopted.inserted);
    assert!(adopted.record.runtime_uid.is_none());
    assert_eq!(
        adopted.operation.expected_runtime_uid.as_deref(),
        Some(adopted_runtime_uid.as_str()),
        "a shared runtime remains unbound until independently observed"
    );
    assert_eq!(
        adopted.operation.state,
        TaskOrchestrationState::IntentRecorded
    );
    let replacement_runtime_uid = format!("replacement-runtime-{suffix}");
    let recreated_runtime_request = TaskReservationRequest {
        runtime_uid: Some(&replacement_runtime_uid),
        ..adopted_request
    };
    assert_eq!(
        store.reserve_task(&recreated_runtime_request).await,
        Err(StoreError::TaskIdempotencyConflict),
        "a same-name replacement UID must not match the durable adopted-runtime reservation"
    );

    let second_adopted_task_uid = Uuid::new_v4();
    let second_adopted_operation_id = Uuid::new_v4();
    let second_adopted_key = format!("adopted-second-{suffix}");
    let second_adopted_request = TaskReservationRequest {
        task_uid: second_adopted_task_uid,
        operation_id: second_adopted_operation_id,
        idempotency_key: &second_adopted_key,
        ..adopted_request
    };
    assert!(store.reserve_task(&second_adopted_request).await?.inserted);
    for task_uid in [adopted_task_uid, second_adopted_task_uid] {
        assert!(matches!(
            store
                .record_task_runtime_observed(
                    task_uid,
                    1,
                    &adopted_runtime_uid,
                    "adopted-resource-version",
                    "controller-a",
                )
                .await?,
            TaskOperationTransition::Applied(_)
        ));
        assert!(matches!(
            store
                .decide_task_runtime_authority(
                    task_uid,
                    2,
                    &latest_envelope,
                    &service_envelope_digest,
                    "controller-a",
                )
                .await?,
            TaskOperationTransition::Applied(_)
        ));
        assert!(matches!(
            store
                .decide_task_runtime_authority(
                    task_uid,
                    3,
                    &latest_envelope,
                    &service_envelope_digest,
                    "controller-a",
                )
                .await?,
            TaskOperationTransition::Applied(_)
        ));
        assert!(matches!(
            store
                .record_task_activation_observed(
                    task_uid,
                    4,
                    &TaskActivationObservation {
                        runtime_uid: &adopted_runtime_uid,
                        resource_version: "adopted-resource-version",
                        active_manifest_digest: &candidate_digest,
                        provider_set_ready: true,
                    },
                    "controller-a",
                )
                .await?,
            TaskOperationTransition::Applied(_)
        ));
        store
            .put_task_inputs(
                task_uid,
                "steward-run",
                canonical.user_id.as_str(),
                b"shared-runtime-input",
            )
            .await?;
        store
            .request_task_execution(task_uid, "steward-run", canonical.user_id.as_str())
            .await?;
    }
    let left_store = store.clone();
    let right_store = store.clone();
    let shared_command_digest = format!("sha256:{}", "1".repeat(64));
    let shared_input_digest = format!("sha256:{}", "2".repeat(64));
    let (left, right) = tokio::join!(
        left_store.claim_task_execution_attempt(
            adopted_task_uid,
            &shared_command_digest,
            &shared_input_digest,
            "controller-a",
        ),
        right_store.claim_task_execution_attempt(
            second_adopted_task_uid,
            &shared_command_digest,
            &shared_input_digest,
            "controller-b",
        ),
    );
    let shared_runtime_claims = [left?, right?];
    assert_eq!(
        shared_runtime_claims
            .iter()
            .filter(|claim| matches!(claim, TaskExecutionTransition::Created(_)))
            .count(),
        1,
        "one runtime UID must have at most one active Task execution lease"
    );
    assert!(
        shared_runtime_claims.iter().any(|claim| matches!(
            claim,
            TaskExecutionTransition::InvariantViolation {
                reason: "runtime_execution_lease_held",
                ..
            }
        )),
        "the competing Task must remain queued while the shared runtime lease is held"
    );

    let lease_owner = shared_runtime_claims
        .iter()
        .find_map(|claim| match claim {
            TaskExecutionTransition::Created(attempt) => Some(attempt.clone()),
            _ => None,
        })
        .ok_or_else(|| io::Error::other("no lease owner"))?;
    let waiting_task = if lease_owner.task_uid == adopted_task_uid {
        second_adopted_task_uid
    } else {
        adopted_task_uid
    };
    let authorized = match store
        .authorize_task_execution_start(
            lease_owner.attempt_id,
            lease_owner.generation,
            "controller-a",
        )
        .await?
    {
        TaskExecutionTransition::Applied(attempt) => attempt,
        other => return Err(io::Error::other(format!("start not authorized: {other:?}")).into()),
    };
    store
        .record_task_execution_observation(
            authorized.attempt_id,
            authorized.generation,
            TaskExecutionObservation::OutcomeUnknown {
                reason: "cancellation_not_proven",
            },
            "controller-a",
        )
        .await?;
    let cleanup = store
        .task_runtime_operation(lease_owner.task_uid)
        .await?
        .ok_or(StoreError::TaskNotFound)?;
    store
        .record_task_cleanup_complete(
            lease_owner.task_uid,
            cleanup.generation,
            TaskCleanupObservation {
                exact_runtime_absent: false,
            },
            "controller-a",
        )
        .await?;
    assert!(
        store
            .task(lease_owner.task_uid)
            .await?
            .ok_or(StoreError::TaskNotFound)?
            .finalized
    );
    // A new store instance models recovery without any process-local lease state.
    let restarted = PgStore::new(store.pool().clone());
    assert!(
        matches!(
            restarted
                .claim_task_execution_attempt(
                    waiting_task,
                    &shared_command_digest,
                    &shared_input_digest,
                    "controller-b",
                )
                .await?,
            TaskExecutionTransition::InvariantViolation {
                reason: "runtime_execution_lease_held",
                ..
            }
        ),
        "unknown execution must quarantine the exact UID after finalization and restart"
    );

    let unknown = restarted
        .task_execution_attempt(lease_owner.task_uid)
        .await?
        .ok_or(StoreError::TaskNotFound)?;
    assert!(
        sqlx::query("DELETE FROM task_runtime_execution_leases WHERE attempt_id = $1")
            .bind(unknown.attempt_id)
            .execute(restarted.pool())
            .await
            .is_err(),
        "the database rejects release without retirement evidence"
    );
    let task_before_retirement = restarted
        .task(lease_owner.task_uid)
        .await?
        .ok_or(StoreError::TaskNotFound)?;
    restarted
        .record_task_execution_observation(
            unknown.attempt_id,
            unknown.generation,
            TaskExecutionObservation::Failed {
                adapter_observation_id: "exact-late-terminal",
                reason: "process_exited",
                execution_stdout: None,
                execution_stderr: None,
            },
            "controller-b",
        )
        .await?;
    assert_eq!(
        restarted
            .task_execution_attempt(lease_owner.task_uid)
            .await?
            .ok_or(StoreError::TaskNotFound)?,
        unknown,
        "late retirement must not rewrite the immutable unknown outcome"
    );
    assert_eq!(
        restarted
            .task(lease_owner.task_uid)
            .await?
            .ok_or(StoreError::TaskNotFound)?,
        task_before_retirement
    );
    assert!(
        matches!(
            restarted
                .claim_task_execution_attempt(
                    waiting_task,
                    &shared_command_digest,
                    &shared_input_digest,
                    "controller-b",
                )
                .await?,
            TaskExecutionTransition::Created(_)
        ),
        "exact terminal evidence permits subsequent reuse"
    );

    let other = store
        .register_canonical_identity(
            &google_identity(
                format!("google-other-subject-{suffix}"),
                format!("bob-{suffix}@example.com"),
            )?,
            "test-bootstrap",
        )
        .await?;
    let mut other_spec = proposed_spec();
    other_spec.canonical_authority = Some(CanonicalAuthorityBinding::new(
        other.user_id.clone(),
        Some(other.user_id.clone()),
    )?);
    let other_task_uid = Uuid::new_v4();
    let other_operation_id = Uuid::new_v4();
    let other_runtime_name = format!("task-{}", other_operation_id.simple());
    let other_request = TaskReservationRequest {
        task_uid: other_task_uid,
        operation_id: other_operation_id,
        envelope_revision: latest_envelope.revision,
        service_envelope: &latest_envelope,
        acting_user_id: Some(other.user_id.as_str()),
        owner_user_id: other.user_id.as_str(),
        runtime_name: &other_runtime_name,
        runtime_spec: &other_spec,
        ..request
    };
    let other_task = store.reserve_task(&other_request).await?;
    assert!(other_task.inserted);
    assert_ne!(other_task.record.task_uid, first.record.task_uid);
    assert_ne!(other_task.record.runtime_name, first.record.runtime_name);
    let other_retry = store.reserve_task(&other_request).await?;
    assert!(!other_retry.inserted);
    assert_eq!(other_retry.record.task_uid, other_task.record.task_uid);

    let injected_operation_id = Uuid::new_v4();
    let injected_runtime_name = format!("task-{}", injected_operation_id.simple());
    let injected_same_owner = TaskReservationRequest {
        task_uid: Uuid::new_v4(),
        operation_id: injected_operation_id,
        runtime_name: &injected_runtime_name,
        ..request
    };
    let injected_retry = store.reserve_task(&injected_same_owner).await?;
    assert!(!injected_retry.inserted);
    assert_eq!(
        injected_retry.record.runtime_name,
        first.record.runtime_name
    );
    assert!(
        store
            .task_for_submitter(first.record.task_uid, "steward-run", other.user_id.as_str(),)
            .await?
            .is_none(),
        "a different canonical owner must not observe the first owner's Task"
    );
    assert_eq!(
        store
            .request_task_finalization(
                first.record.task_uid,
                "steward-run",
                other.user_id.as_str(),
            )
            .await,
        Err(StoreError::TaskNotFound),
        "a different canonical owner must not delete the first owner's runtime through Task finalization"
    );

    let legacy_key = format!("legacy-{suffix}");
    let reconnect_operation_id = Uuid::new_v4();
    let rebound_runtime_name = format!("task-{}", reconnect_operation_id.simple());
    let legacy_reconnect = TaskReservationRequest {
        task_uid: Uuid::new_v4(),
        operation_id: reconnect_operation_id,
        idempotency_key: &legacy_key,
        envelope_revision: latest_envelope.revision,
        service_envelope: &latest_envelope,
        runtime_name: &rebound_runtime_name,
        ..request
    };
    let reconnected = store.reserve_task(&legacy_reconnect).await?;
    assert!(reconnected.inserted);
    assert_eq!(reconnected.record.runtime_name, rebound_runtime_name);
    assert_ne!(
        reconnected.record.identity_binding_state, "legacy_reconnect_required",
        "a canonical reconnect creates a new bound row instead of adopting the legacy row"
    );

    let mismatched_columns_key = format!("mismatched-columns-{suffix}");
    let mismatched_columns = TaskReservationRequest {
        task_uid: Uuid::new_v4(),
        operation_id: request.operation_id,
        idempotency_key: &mismatched_columns_key,
        acting_user_id: Some(other.user_id.as_str()),
        ..request
    };
    assert_eq!(
        store.reserve_task(&mismatched_columns).await,
        Err(StoreError::InvalidTaskIdentityBinding),
        "delegated v1 reservations must reject acting_user_id != owner_user_id"
    );

    let mismatched_runtime_key = format!("mismatched-runtime-{suffix}");
    let mismatched_runtime_authority = TaskReservationRequest {
        task_uid: Uuid::new_v4(),
        operation_id: request.operation_id,
        idempotency_key: &mismatched_runtime_key,
        owner_user_id: other.user_id.as_str(),
        acting_user_id: Some(other.user_id.as_str()),
        ..request
    };
    assert_eq!(
        store.reserve_task(&mismatched_runtime_authority).await,
        Err(StoreError::InvalidTaskIdentityBinding),
        "Task columns must match the server-authored runtime canonical authority"
    );

    let direct_mismatch = sqlx::query(
        "INSERT INTO task_submissions \
         (task_uid, idempotency_key, submitter_service, acting_user, acting_user_id, owner, \
          owner_user_id, identity_binding_state, workflow, coding_agent_runtime, \
          runtime_namespace, runtime_name, runtime_ownership, phase, runtime_spec, agent_command) \
         VALUES (gen_random_uuid(), $1, 'steward-run', 'alice@example.com', $2, \
                 'alice@example.com', $3, 'bound', 'code-review', 'agent-v1', 'team-a', $4, \
                 'provisioned', 'submitted', $5, '[]'::jsonb)",
    )
    .bind(format!("direct-mismatch-{suffix}"))
    .bind(other.user_id.as_str())
    .bind(canonical.user_id.as_str())
    .bind(format!("direct-mismatch-{suffix}"))
    .bind(sqlx::types::Json(&spec))
    .execute(store.pool())
    .await;
    assert!(
        direct_mismatch.is_err(),
        "the database must reject delegated acting_user_id != owner_user_id"
    );

    let escaped_operation_id = Uuid::new_v4();
    let escaped_runtime_name = format!("task-{}", escaped_operation_id.simple());
    let escaped_idempotency_key = format!("historical-escape-{suffix}");
    let mut escaped_spec = spec.clone();
    escaped_spec.budget.monthly_limit = "260.00".to_owned();
    let escaped_request = TaskReservationRequest {
        task_uid: Uuid::new_v4(),
        operation_id: escaped_operation_id,
        idempotency_key: &escaped_idempotency_key,
        runtime_name: &escaped_runtime_name,
        runtime_spec: &escaped_spec,
        ..request
    };
    assert_eq!(
        store.reserve_task(&escaped_request).await,
        Err(StoreError::StaleEnvelope),
        "the reservation transaction must reject a candidate whose claimed decision is stale"
    );

    store
        .record_spend_observation(
            &runtime_uid,
            1,
            "task-read-model-spec",
            &SpendSummary {
                observed_amount: "1.25".to_owned(),
                currency: "USD".to_owned(),
            },
            false,
        )
        .await?;
    let archive = b"neutral-tar-fixture";
    store
        .put_task_inputs(
            first.record.task_uid,
            "steward-run",
            canonical.user_id.as_str(),
            archive,
        )
        .await?;
    let queued = store
        .request_task_execution(
            first.record.task_uid,
            "steward-run",
            canonical.user_id.as_str(),
        )
        .await?;
    assert_eq!(queued.phase, TaskPhase::Queued);
    let command_digest = format!("sha256:{}", "8".repeat(64));
    let input_digest = format!("sha256:{}", "9".repeat(64));
    let attempt = match store
        .claim_task_execution_attempt(
            first.record.task_uid,
            &command_digest,
            &input_digest,
            "controller-a",
        )
        .await?
    {
        TaskExecutionTransition::Created(attempt) => attempt,
        other => {
            return Err(io::Error::other(format!("unexpected attempt claim: {other:?}")).into());
        }
    };
    assert!(matches!(
        store
            .claim_task_execution_attempt(
                first.record.task_uid,
                &command_digest,
                &input_digest,
                "controller-b",
            )
            .await?,
        TaskExecutionTransition::AlreadyApplied(current)
            if current.attempt_id == attempt.attempt_id
    ));
    assert!(matches!(
        store
            .authorize_task_execution_start(attempt.attempt_id, 1, "controller-a")
            .await?,
        TaskExecutionTransition::Applied(_)
    ));
    assert!(matches!(
        store
            .record_task_execution_observation(
                attempt.attempt_id,
                2,
                TaskExecutionObservation::Succeeded {
                    adapter_observation_id: "adapter-attempt-a",
                    result_digest: &format!("sha256:{}", "a".repeat(64)),
                    result_reference: "adapter:attempt-a",
                    output_archive: b"neutral-output-tar",
                    execution_stdout: None,
                    execution_stderr: None,
                },
                "controller-a",
            )
            .await?,
        TaskExecutionTransition::Applied(_)
    ));
    let completed = store
        .task(first.record.task_uid)
        .await?
        .ok_or_else(|| io::Error::other("completed task disappeared"))?;
    assert_eq!(completed.phase, TaskPhase::Succeeded);
    assert_eq!(
        completed.output_archive.as_deref(),
        Some(b"neutral-output-tar".as_slice())
    );
    assert!(
        sqlx::query("UPDATE task_submissions SET output_archive = NULL, finalize_requested = true WHERE task_uid = $1")
            .bind(first.record.task_uid)
            .execute(store.pool())
            .await
            .is_err(),
        "ordinary Task output remains immutable even when cleanup is requested"
    );
    let execution_history = store
        .agent_run_timeline(first.record.task_uid)
        .await?
        .ok_or_else(|| io::Error::other("completed task timeline disappeared"))?;
    assert_eq!(
        execution_history
            .iter()
            .filter_map(|event| match event.kind {
                AgentRunTimelineKind::Phase(phase) => Some(phase),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec![
            TaskPhase::Submitted,
            TaskPhase::Queued,
            TaskPhase::Running,
            TaskPhase::Succeeded,
        ],
        "a terminal-first adapter observation must still preserve durable running evidence"
    );
    store
        .request_task_finalization(
            first.record.task_uid,
            "steward-run",
            canonical.user_id.as_str(),
        )
        .await?;
    assert!(matches!(
        store
            .enter_task_cleanup(
                first.record.task_uid,
                6,
                steward_store::TaskCleanupCause::FinalizationRequested,
                "controller-a",
            )
            .await?,
        TaskOperationTransition::Applied(_)
    ));
    assert!(matches!(
        store
            .record_task_cleanup_complete(
                first.record.task_uid,
                7,
                TaskCleanupObservation {
                    exact_runtime_absent: true,
                },
                "controller-a",
            )
            .await?,
        TaskOperationTransition::Applied(_)
    ));
    assert!(
        store
            .task(first.record.task_uid)
            .await?
            .ok_or_else(|| io::Error::other("finalized task disappeared"))?
            .finalized
    );
    let page = store
        .agent_runs(&AgentRunQuery {
            limit: 10,
            cursor: None,
            phase: Some(TaskPhase::Succeeded),
            workflow: Some("code-review".to_owned()),
            owner_user_id: None,
            runtime_uid: None,
            user_envelope_instance_id: None,
            task_uid: None,
        })
        .await?;
    let read_model = page
        .records
        .iter()
        .find(|record| record.task_uid == first.record.task_uid)
        .ok_or_else(|| io::Error::other("completed task is absent from Agent Runs"))?;
    assert_eq!(read_model.envelope_revision, Some(1));
    assert_eq!(read_model.runtime_spec, spec);
    assert_eq!(
        read_model
            .spend
            .as_ref()
            .map(|spend| spend.observed_amount.as_str()),
        Some("1.25")
    );
    assert!(!read_model.history_partial);
    let owner_scoped = store
        .agent_runs(&AgentRunQuery {
            limit: 10,
            cursor: None,
            phase: Some(TaskPhase::Succeeded),
            workflow: Some("code-review".to_owned()),
            owner_user_id: Some(canonical.user_id.as_str().to_owned()),
            runtime_uid: None,
            user_envelope_instance_id: None,
            task_uid: None,
        })
        .await?;
    assert!(
        owner_scoped
            .records
            .iter()
            .any(|record| record.task_uid == first.record.task_uid),
        "the canonical owner scope must return the caller's run"
    );
    assert_eq!(
        store
            .agent_runs(&AgentRunQuery {
                limit: 10,
                cursor: Some(first.record.task_uid),
                phase: None,
                workflow: None,
                owner_user_id: Some(other.user_id.as_str().to_owned()),
                runtime_uid: None,
                user_envelope_instance_id: None,
                task_uid: None,
            })
            .await,
        Err(StoreError::InvalidRunCursor),
        "an owner-scoped cursor must not reveal another user's run boundary"
    );
    let timeline = store
        .agent_run_timeline(first.record.task_uid)
        .await?
        .ok_or_else(|| io::Error::other("completed task timeline disappeared"))?;
    assert!(
        timeline
            .iter()
            .all(|event| { event.provenance == AgentRunTimelineProvenance::Recorded })
    );
    assert!(
        timeline
            .iter()
            .any(|event| { event.kind == AgentRunTimelineKind::Phase(TaskPhase::Succeeded) })
    );
    assert!(
        timeline
            .iter()
            .any(|event| { event.kind == AgentRunTimelineKind::FinalizationRequested })
    );
    assert!(
        timeline
            .iter()
            .any(|event| { event.kind == AgentRunTimelineKind::Finalized })
    );
    Ok(())
}

fn budget_deltas() -> Vec<AdmissionDelta> {
    vec![AdmissionDelta::Budget {
        requested: "220.00".to_owned(),
        ceiling: "200.00".to_owned(),
        currency: "USD".to_owned(),
    }]
}

fn base_spec() -> AgentRuntimeSpec {
    let mut spec = proposed_spec();
    spec.budget.monthly_limit = "100.00".to_owned();
    spec
}

fn envelope(member_limit: &str, revision: i64) -> Envelope {
    let spec = proposed_spec();
    Envelope {
        revision,
        spec: EnvelopeSpec {
            llms: spec.llms,
            tools: spec.tools,
            budget: Budget {
                monthly_limit: member_limit.to_owned(),
                single_run_limit: None,
                currency: spec.budget.currency,
            },
            ttl: spec.ttl,
            runner: RunnerRequirements::default(),
        },
    }
}

#[tokio::test]
async fn s4_service_envelopes_and_grants_are_isolated_from_equal_role_names()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the S4 Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let scope_ref = format!("shared-scope-{suffix}");
    let runtime_uid = format!("runtime-service-{suffix}");

    store
        .insert_envelope(&scope_ref, &envelope("200.00", 1), "admin@example.com")
        .await?;
    store
        .insert_service_envelope(&scope_ref, &envelope("50.00", 1), "admin@example.com")
        .await?;
    assert_eq!(
        store
            .latest_envelope(&scope_ref)
            .await?
            .ok_or_else(|| io::Error::other("member-role envelope disappeared"))?
            .spec
            .budget
            .monthly_limit,
        "200.00"
    );
    assert_eq!(
        store
            .latest_service_envelope(&scope_ref)
            .await?
            .ok_or_else(|| io::Error::other("service envelope disappeared"))?
            .spec
            .budget
            .monthly_limit,
        "50.00"
    );

    let mut proposed = proposed_spec();
    proposed.principal = Principal::Service {
        name: scope_ref.clone(),
        acting_user: None,
    };
    proposed.owner = Email("alice@example.com".to_owned());
    proposed.budget.monthly_limit = "60.00".to_owned();
    let mut base = proposed.clone();
    base.budget.monthly_limit = "0.00".to_owned();
    let deltas = vec![AdmissionDelta::Budget {
        requested: "60.00".to_owned(),
        ceiling: "50.00".to_owned(),
        currency: "USD".to_owned(),
    }];
    let parked = store
        .park_rejection(ParkRejection {
            task_uid: None,
            runtime_uid: &runtime_uid,
            runtime_namespace: "team-a",
            runtime_name: &runtime_uid,
            spec_digest: "service-proposed-digest",
            base_spec_digest: "service-base-digest",
            base_pending_approval_digest: None,
            base_spec: &base,
            envelope_revision: 1,
            deltas: &deltas,
            proposed_spec: &proposed,
            actor: &scope_ref,
            member_role: &scope_ref,
        })
        .await?;
    store
        .link_decision_reference(
            parked.approval_id,
            "PROJ-123",
            "https://jira.example.com/browse/PROJ-123",
        )
        .await?;
    store
        .approve_admission(ApproveAdmission {
            approval_id: parked.approval_id,
            decided_by: "admin@example.com",
            rationale: "approve one service runtime",
            evidence_url: "https://jira.example.com/browse/PROJ-123",
            expires_at: "2999-01-01T00:00:00Z",
        })
        .await?;

    assert_eq!(
        store
            .grants_for_runtime_scoped(&runtime_uid, EnvelopeScopeKind::Service, &scope_ref, 1,)
            .await?,
        deltas
    );
    assert!(
        store
            .grants_for_runtime(&runtime_uid, &scope_ref, 1)
            .await?
            .is_empty(),
        "an equal member-role scope must not inherit the service grant"
    );

    store
        .insert_envelope(&scope_ref, &envelope("250.00", 2), "admin@example.com")
        .await?;
    assert_eq!(
        store
            .grants_for_runtime_scoped(&runtime_uid, EnvelopeScopeKind::Service, &scope_ref, 1,)
            .await?,
        deltas,
        "a role-envelope revision must not revoke an equal-named service grant"
    );
    store
        .insert_service_envelope(&scope_ref, &envelope("75.00", 2), "admin@example.com")
        .await?;
    assert!(
        store
            .grants_for_runtime_scoped(&runtime_uid, EnvelopeScopeKind::Service, &scope_ref, 1,)
            .await?
            .is_empty(),
        "a new service-envelope revision must revoke the stale service grant"
    );
    Ok(())
}

#[tokio::test]
async fn s4_grants_are_append_only_and_bound_to_one_runtime() -> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the S4 Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;

    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let runtime_a = format!("runtime-a-{suffix}");
    let runtime_b = format!("runtime-b-{suffix}");
    let decision_id = sqlx::query("SELECT gen_random_uuid()::text AS id")
        .fetch_one(store.pool())
        .await?
        .try_get::<String, _>("id")?;
    let approval_id = sqlx::query("SELECT gen_random_uuid()::text AS id")
        .fetch_one(store.pool())
        .await?
        .try_get::<String, _>("id")?;
    let grant_id = sqlx::query("SELECT gen_random_uuid()::text AS id")
        .fetch_one(store.pool())
        .await?
        .try_get::<String, _>("id")?;

    sqlx::query(
        "INSERT INTO admission_decisions \
         (id, runtime_uid, spec_digest, envelope_rev, verdict, deltas, proposed_spec, actor, \
          member_role, base_spec_digest, base_spec, runtime_namespace, runtime_name) \
         VALUES ($1::uuid, $2, 'digest-a', 1, 'reject', '[]'::jsonb, $3::jsonb, \
                 'alice@example.com', 'engineer', 'base-digest-a', $4::jsonb, \
                 'team-a', 'runtime-a')",
    )
    .bind(&decision_id)
    .bind(&runtime_a)
    .bind(serde_json::to_value(proposed_spec())?)
    .bind(serde_json::to_value(base_spec())?)
    .execute(store.pool())
    .await?;
    sqlx::query(
        "INSERT INTO approvals \
         (id, runtime_uid, admission_decision_id, state, decision_key) \
         VALUES ($1::uuid, $2, $3::uuid, 'pending', 'PROJ-123')",
    )
    .bind(&approval_id)
    .bind(&runtime_a)
    .bind(&decision_id)
    .execute(store.pool())
    .await?;

    let inserted = sqlx::query(
        "INSERT INTO grants \
         (id, runtime_uid, dimension, granted_value, approval_id, envelope_revision, expires_at) \
         VALUES ($1::uuid, $2, 'budget', \
                 '{\"dimension\":\"budget\",\"requested\":\"220.00\",\
                   \"ceiling\":\"200.00\",\"currency\":\"USD\"}'::jsonb, \
                 $3::uuid, 1, '2999-01-01T00:00:00Z')",
    )
    .bind(&grant_id)
    .bind(&runtime_a)
    .bind(&approval_id)
    .execute(store.pool())
    .await;
    assert!(
        inserted.is_ok(),
        "S4 must persist an instance-bound grant row before provisioning: {inserted:?}"
    );

    let rebound = sqlx::query("UPDATE grants SET runtime_uid = $1 WHERE id = $2::uuid")
        .bind(&runtime_b)
        .bind(&grant_id)
        .execute(store.pool())
        .await;
    assert!(
        rebound.is_err(),
        "a granted exception must never be rebound to a second runtime UID"
    );
    let visible_to_second_runtime =
        sqlx::query("SELECT id FROM grants WHERE runtime_uid = $1 AND id = $2::uuid")
            .bind(&runtime_b)
            .bind(&grant_id)
            .fetch_optional(store.pool())
            .await?;
    assert!(
        visible_to_second_runtime.is_none(),
        "a second runtime must not observe the first runtime's grant"
    );

    let deleted = sqlx::query("DELETE FROM grants WHERE id = $1::uuid")
        .bind(&grant_id)
        .execute(store.pool())
        .await;
    assert!(
        deleted.is_err(),
        "grant history must be append-only so approval evidence cannot disappear"
    );
    Ok(())
}

#[tokio::test]
async fn s4_repeated_parking_reuses_one_approval_and_one_channel_marker()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the S4 Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let proposed_spec = proposed_spec();
    let base_spec = base_spec();
    let deltas = budget_deltas();
    let request = || ParkRejection {
        task_uid: None,
        runtime_uid: "runtime-retry-a",
        runtime_namespace: "team-a",
        runtime_name: "runtime-retry-a",
        spec_digest: "digest-retry-a",
        base_spec_digest: "base-digest-retry-a",
        base_pending_approval_digest: None,
        base_spec: &base_spec,
        envelope_revision: 1,
        deltas: &deltas,
        proposed_spec: &proposed_spec,
        actor: "alice@example.com",
        member_role: "engineer",
    };

    let first = store.park_rejection(request()).await?;
    let second = store.park_rejection(request()).await?;
    assert_eq!(
        first, second,
        "retrying the same rejected manifest must reuse its approval so a failed channel request can be retried"
    );
    let decision_count = sqlx::query(
        "SELECT count(*)::bigint AS count \
         FROM admission_decisions \
         WHERE runtime_uid = 'runtime-retry-a' AND spec_digest = 'digest-retry-a'",
    )
    .fetch_one(store.pool())
    .await?
    .try_get::<i64, _>("count")?;
    assert_eq!(decision_count, 1);
    Ok(())
}

#[tokio::test]
async fn s4_task_admission_correlation_is_nullable_unique_and_immutable()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the S4 Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;

    let nullable = sqlx::query_scalar::<_, String>(
        "SELECT is_nullable FROM information_schema.columns \
         WHERE table_schema = 'public' AND table_name = 'admission_decisions' \
           AND column_name = 'task_uid'",
    )
    .fetch_optional(store.pool())
    .await?;
    assert_eq!(
        nullable.as_deref(),
        Some("YES"),
        "Task admission correlation must be optional for non-Task and historical decisions"
    );

    let index_definition = sqlx::query_scalar::<_, String>(
        "SELECT indexdef FROM pg_indexes \
         WHERE schemaname = 'public' AND tablename = 'admission_decisions' \
           AND indexname = 'admission_decisions_task_uid_unique'",
    )
    .fetch_optional(store.pool())
    .await?;
    let index_definition = index_definition
        .ok_or_else(|| io::Error::other("Task admission unique partial index is missing"))?;
    assert!(index_definition.contains("UNIQUE INDEX"));
    assert!(index_definition.contains("WHERE (task_uid IS NOT NULL)"));

    let append_only_trigger = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS ( \
             SELECT 1 FROM pg_trigger \
             WHERE tgrelid = 'admission_decisions'::regclass \
               AND tgname = 'admission_decisions_are_append_only' \
               AND NOT tgisinternal \
         )",
    )
    .fetch_one(store.pool())
    .await?;
    assert!(
        append_only_trigger,
        "the correlation must inherit append-only admission-decision immutability"
    );
    Ok(())
}

#[tokio::test]
async fn s4_active_grants_expire_and_can_be_revoked_without_erasing_history()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the S4 Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let proposed_spec = proposed_spec();
    let base_spec = base_spec();
    let deltas = budget_deltas();
    store
        .insert_envelope(
            "engineer-revocation",
            &envelope("200.00", 7),
            "admin@example.com",
        )
        .await?;
    let parked = store
        .park_rejection(ParkRejection {
            task_uid: None,
            runtime_uid: "runtime-revocation-a",
            runtime_namespace: "team-a",
            runtime_name: "runtime-revocation-a",
            spec_digest: "digest-revocation-a",
            base_spec_digest: "base-digest-revocation-a",
            base_pending_approval_digest: None,
            base_spec: &base_spec,
            envelope_revision: 7,
            deltas: &deltas,
            proposed_spec: &proposed_spec,
            actor: "alice@example.com",
            member_role: "engineer-revocation",
        })
        .await?;
    store
        .link_decision_reference(
            parked.approval_id,
            "PROJ-123",
            "https://jira.example.com/browse/PROJ-123",
        )
        .await?;
    let expired = store
        .approve_admission(ApproveAdmission {
            approval_id: parked.approval_id,
            decided_by: "admin@example.com",
            rationale: "unbounded exception attempt",
            evidence_url: "https://jira.example.com/browse/PROJ-123",
            expires_at: "2000-01-01T00:00:00Z",
        })
        .await;
    assert_eq!(
        expired,
        Err(StoreError::InvalidGrantExpiry),
        "an approval must not create authority with an absent or elapsed lifetime",
    );
    store
        .approve_admission(ApproveAdmission {
            approval_id: parked.approval_id,
            decided_by: "admin@example.com",
            rationale: "bounded exception",
            evidence_url: "https://jira.example.com/browse/PROJ-123",
            expires_at: "2999-01-01T00:00:00Z",
        })
        .await?;

    let context = sqlx::query(
        "SELECT envelope_revision, expires_at IS NOT NULL AS expires \
         FROM grants WHERE approval_id = $1",
    )
    .bind(parked.approval_id)
    .fetch_one(store.pool())
    .await?;
    assert_eq!(context.try_get::<i64, _>("envelope_revision")?, 7);
    assert!(context.try_get::<bool, _>("expires")?);
    assert!(
        store
            .grants_for_runtime("runtime-revocation-a", "engineer-revocation", 8)
            .await?
            .is_empty(),
        "a grant must not survive a change from the envelope revision it approved",
    );
    assert_eq!(
        store
            .revoke_runtime_grants(
                "runtime-revocation-a",
                "admin@example.com",
                "scope narrowed",
            )
            .await?,
        1,
    );
    assert!(
        store
            .grants_for_runtime("runtime-revocation-a", "engineer-revocation", 7)
            .await?
            .is_empty(),
        "an append-only revocation must remove the grant from active authority"
    );
    let retained =
        sqlx::query("SELECT count(*)::bigint AS count FROM grants WHERE approval_id = $1")
            .bind(parked.approval_id)
            .fetch_one(store.pool())
            .await?
            .try_get::<i64, _>("count")?;
    assert_eq!(
        retained, 1,
        "revocation must retain immutable grant evidence"
    );
    Ok(())
}

#[tokio::test]
async fn s4_application_requires_every_granted_dimension_to_remain_active()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the S4 Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let runtime_uid = format!("runtime-multigrant-{suffix}");
    let member_role = format!("engineer-multigrant-{suffix}");
    let mut proposed = proposed_spec();
    proposed.ttl = Duration("48h".to_owned());
    let base = base_spec();
    let deltas = vec![
        AdmissionDelta::Budget {
            requested: "220.00".to_owned(),
            ceiling: "200.00".to_owned(),
            currency: "USD".to_owned(),
        },
        AdmissionDelta::Ttl {
            requested: "48h".to_owned(),
            ceiling: "24h".to_owned(),
        },
    ];
    store
        .insert_envelope(&member_role, &envelope("200.00", 1), "admin@example.com")
        .await?;
    let parked = store
        .park_rejection(ParkRejection {
            task_uid: None,
            runtime_uid: &runtime_uid,
            runtime_namespace: "team-a",
            runtime_name: "runtime-multigrant",
            spec_digest: &format!("digest-{suffix}"),
            base_spec_digest: &format!("base-digest-{suffix}"),
            base_pending_approval_digest: None,
            base_spec: &base,
            envelope_revision: 1,
            deltas: &deltas,
            proposed_spec: &proposed,
            actor: "alice@example.com",
            member_role: &member_role,
        })
        .await?;
    store
        .link_decision_reference(
            parked.approval_id,
            "PROJ-123",
            "https://jira.example.com/browse/PROJ-123",
        )
        .await?;
    store
        .approve_admission(ApproveAdmission {
            approval_id: parked.approval_id,
            decided_by: "admin@example.com",
            rationale: "bounded multi-dimension exception",
            evidence_url: "https://jira.example.com/browse/PROJ-123",
            expires_at: "2999-01-01T00:00:00Z",
        })
        .await?;
    assert!(store.grant_application(&runtime_uid).await?.is_some());
    let revoked_grant = sqlx::query_scalar::<_, String>(
        "SELECT id::text FROM grants WHERE approval_id = $1 AND dimension = 'budget'",
    )
    .bind(parked.approval_id)
    .fetch_one(store.pool())
    .await?;
    sqlx::query(
        "INSERT INTO grant_revocations (grant_id, revoked_by, reason) \
         VALUES (($1::text)::uuid, $2, $3)",
    )
    .bind(revoked_grant)
    .bind("admin@example.com")
    .bind("one dimension revoked")
    .execute(store.pool())
    .await?;
    assert!(
        store.grant_application(&runtime_uid).await?.is_none(),
        "a partially revoked approval must not restore its complete proposed spec"
    );
    assert!(
        store.grant_reversion(&runtime_uid).await?.is_some(),
        "partial revocation must schedule restoration of the pre-grant spec"
    );
    Ok(())
}

#[tokio::test]
async fn s4_approval_rejects_evidence_not_bound_to_the_parked_issue() -> Result<(), Box<dyn Error>>
{
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the S4 Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;

    let proposed_spec = AgentRuntimeSpec {
        principal: Principal::User {
            acting_user: Email("alice@example.com".to_owned()),
        },
        owner: Email("alice@example.com".to_owned()),
        canonical_authority: None,
        agent_type: AgentType {
            name: "base".to_owned(),
        },
        llms: vec![ModelRef {
            provider: "provider-a".to_owned(),
            model: "model-a".to_owned(),
        }],
        tools: Vec::new(),
        budget: Budget {
            monthly_limit: "220.00".to_owned(),
            single_run_limit: None,
            currency: "USD".to_owned(),
        },
        ttl: Duration("24h".to_owned()),
        runner: RunnerRequirements::default(),
        bindings: None,
    };
    let deltas = vec![AdmissionDelta::Budget {
        requested: "220.00".to_owned(),
        ceiling: "200.00".to_owned(),
        currency: "USD".to_owned(),
    }];
    let mut base_spec = proposed_spec.clone();
    base_spec.budget.monthly_limit = "100.00".to_owned();
    store
        .insert_envelope(
            "engineer-evidence",
            &envelope("200.00", 1),
            "admin@example.com",
        )
        .await?;
    assert_eq!(
        store
            .insert_envelope(
                "engineer-evidence",
                &envelope("200.00", 1),
                "admin@example.com",
            )
            .await,
        Err(StoreError::EnvelopeRevisionNotIncreasing),
        "an older revision must not become the event that invalidates current grants"
    );
    let parked = store
        .park_rejection(ParkRejection {
            task_uid: None,
            runtime_uid: "runtime-evidence-a",
            runtime_namespace: "team-a",
            runtime_name: "runtime-evidence-a",
            spec_digest: "digest-evidence-a",
            base_spec_digest: "base-digest-evidence-a",
            base_pending_approval_digest: None,
            base_spec: &base_spec,
            envelope_revision: 1,
            deltas: &deltas,
            proposed_spec: &proposed_spec,
            actor: "alice@example.com",
            member_role: "engineer-evidence",
        })
        .await?;
    store
        .link_decision_reference(
            parked.approval_id,
            "PROJ-123",
            "https://jira.example.com/browse/PROJ-123",
        )
        .await
        .map_err(|error| {
            io::Error::other(format!(
                "the Jira reference must bind to the parked approval: {error}"
            ))
        })?;

    let result = store
        .approve_admission(ApproveAdmission {
            approval_id: parked.approval_id,
            decided_by: "admin@example.com",
            rationale: "approved for this runtime",
            evidence_url: "https://jira.example.com/browse/PROJ-999",
            expires_at: "2999-01-01T00:00:00Z",
        })
        .await;
    assert_eq!(
        result,
        Err(StoreError::EvidenceMismatch),
        "Steward must reject approval evidence that is not the parked request's Jira link"
    );

    let approved = store
        .approve_admission(ApproveAdmission {
            approval_id: parked.approval_id,
            decided_by: "admin@example.com",
            rationale: "approved for this runtime",
            evidence_url: "https://jira.example.com/browse/PROJ-123",
            expires_at: "2999-01-01T00:00:00Z",
        })
        .await;
    let approved = approved.map_err(|error| {
        io::Error::other(format!(
            "correctly bound evidence must approve the parked request: {error}"
        ))
    })?;
    assert_eq!(approved.approval_id, parked.approval_id);
    assert_eq!(approved.decision_id, parked.decision_id);
    assert_eq!(approved.runtime_uid, "runtime-evidence-a");
    assert_eq!(approved.proposed_spec, proposed_spec);
    assert_eq!(approved.actor, "alice@example.com");
    assert_eq!(approved.member_role, "engineer-evidence");
    assert_eq!(approved.decision_key, "PROJ-123");
    assert_eq!(
        approved.evidence_url,
        "https://jira.example.com/browse/PROJ-123"
    );
    assert_eq!(approved.grants, deltas);

    let approval_state =
        sqlx::query("SELECT state, decided_by, rationale FROM approvals WHERE id = $1")
            .bind(parked.approval_id)
            .fetch_one(store.pool())
            .await?;
    assert_eq!(approval_state.try_get::<String, _>("state")?, "approved");
    assert_eq!(
        approval_state.try_get::<String, _>("decided_by")?,
        "admin@example.com"
    );
    assert_eq!(
        approval_state.try_get::<String, _>("rationale")?,
        "approved for this runtime"
    );
    let grant = sqlx::query(
        "SELECT runtime_uid, dimension, granted_value \
         FROM grants WHERE approval_id = $1",
    )
    .bind(parked.approval_id)
    .fetch_one(store.pool())
    .await?;
    assert_eq!(
        grant.try_get::<String, _>("runtime_uid")?,
        "runtime-evidence-a"
    );
    assert_eq!(grant.try_get::<String, _>("dimension")?, "budget");
    assert_eq!(
        grant.try_get::<serde_json::Value, _>("granted_value")?,
        serde_json::to_value(&deltas[0])?
    );
    assert_eq!(
        store
            .grants_for_runtime("runtime-evidence-a", "engineer-evidence", 1)
            .await?,
        deltas,
        "the approved runtime must read back its structured grant"
    );
    assert!(
        store
            .grants_for_runtime("runtime-evidence-a", "analyst-evidence", 1)
            .await?
            .is_empty(),
        "equal revision numbers in different member-role envelope streams must not share authority"
    );
    assert!(
        store
            .grants_for_runtime("runtime-evidence-b", "engineer-evidence", 1)
            .await?
            .is_empty(),
        "a second runtime must never inherit the first runtime's grant"
    );
    let retried = store
        .approve_admission(ApproveAdmission {
            approval_id: parked.approval_id,
            decided_by: "backup-admin@example.org",
            rationale: "recover the previously authorized apply",
            evidence_url: "https://jira.example.com/browse/PROJ-123",
            expires_at: "2999-01-01T00:00:00Z",
        })
        .await
        .map_err(|error| {
            io::Error::other(format!(
                "another admin must be able to retry an authorized apply after a transient failure: {error}"
            ))
        })?;
    assert_eq!(retried, approved);
    let grant_count =
        sqlx::query("SELECT count(*)::bigint AS count FROM grants WHERE approval_id = $1")
            .bind(parked.approval_id)
            .fetch_one(store.pool())
            .await?
            .try_get::<i64, _>("count")?;
    assert_eq!(
        grant_count, 1,
        "an approval retry must not duplicate its grant rows"
    );
    let next_escalation = store
        .park_rejection(ParkRejection {
            task_uid: None,
            runtime_uid: "runtime-evidence-a",
            runtime_namespace: "team-a",
            runtime_name: "runtime-evidence-a",
            spec_digest: "digest-evidence-a",
            base_spec_digest: "base-digest-evidence-a",
            base_pending_approval_digest: None,
            base_spec: &base_spec,
            envelope_revision: 1,
            deltas: &deltas,
            proposed_spec: &proposed_spec,
            actor: "alice@example.com",
            member_role: "engineer-evidence",
        })
        .await?;
    assert_ne!(
        next_escalation.approval_id, parked.approval_id,
        "a completed approval must not permanently capture a later identical escalation"
    );
    assert_eq!(next_escalation.decision_key, None);
    assert_eq!(next_escalation.evidence_url, None);
    let active_application = store
        .grant_application("runtime-evidence-a")
        .await?
        .ok_or_else(|| {
            io::Error::other(
                "an approved grant must remain durable controller work until its spec converges",
            )
        })?;
    let filing_claim = store
        .claim_decision_filing(next_escalation.approval_id)
        .await?;
    let filing_token = filing_claim
        .token
        .ok_or_else(|| io::Error::other("new escalation did not receive a filing lease"))?;
    assert!(
        store
            .retire_pending_approval_if_superseded(
                next_escalation.approval_id,
                active_application.approval_id,
                "runtime-evidence-a",
                "steward-apiserver",
                "superseded by an active approval during create convergence",
            )
            .await?
            .is_some(),
        "an active filing lease must not keep a superseded approval pending"
    );
    assert!(
        !store
            .pending_approvals()
            .await?
            .iter()
            .any(|pending| pending.approval_id == next_escalation.approval_id),
        "a retired loser must not remain reachable through the approval queue"
    );
    store
        .complete_decision_filing(
            next_escalation.approval_id,
            filing_token,
            "PROJ-456",
            "https://jira.example.com/browse/PROJ-456",
        )
        .await?;
    assert_eq!(
        store.approval_for_filing(next_escalation.approval_id).await,
        Err(StoreError::ApprovalNotPending),
        "a retired loser must never be filed or approved later"
    );
    let retired_reference = sqlx::query(
        "SELECT state, decision_key, evidence_url \
         FROM approvals \
         WHERE id = $1",
    )
    .bind(next_escalation.approval_id)
    .fetch_one(store.pool())
    .await?;
    assert_eq!(retired_reference.try_get::<String, _>("state")?, "rejected");
    assert_eq!(
        retired_reference.try_get::<Option<String>, _>("decision_key")?,
        Some("PROJ-456".to_owned()),
        "retirement must preserve the completed external decision reference"
    );
    assert_eq!(
        retired_reference.try_get::<Option<String>, _>("evidence_url")?,
        Some("https://jira.example.com/browse/PROJ-456".to_owned())
    );
    let recoverable_escalation = store
        .park_rejection(ParkRejection {
            task_uid: None,
            runtime_uid: "runtime-evidence-a",
            runtime_namespace: "team-a",
            runtime_name: "runtime-evidence-a",
            spec_digest: "digest-evidence-a",
            base_spec_digest: "base-digest-evidence-a",
            base_pending_approval_digest: None,
            base_spec: &base_spec,
            envelope_revision: 1,
            deltas: &deltas,
            proposed_spec: &proposed_spec,
            actor: "alice@example.com",
            member_role: "engineer-evidence",
        })
        .await?;
    let abandoned_claim = store
        .claim_decision_filing(recoverable_escalation.approval_id)
        .await?;
    let abandoned_token = abandoned_claim
        .token
        .ok_or_else(|| io::Error::other("recoverable escalation did not receive a filing lease"))?;
    assert!(
        store
            .retire_pending_approval_if_superseded(
                recoverable_escalation.approval_id,
                active_application.approval_id,
                "runtime-evidence-a",
                "steward-apiserver",
                "superseded by an active approval during create convergence",
            )
            .await?
            .is_some()
    );
    sqlx::query(
        "UPDATE approvals \
         SET decision_filing_started_at = clock_timestamp() - interval '6 minutes' \
         WHERE id = $1",
    )
    .bind(recoverable_escalation.approval_id)
    .execute(store.pool())
    .await?;
    let recovered_claim = store
        .claim_decision_filing(recoverable_escalation.approval_id)
        .await?;
    let recovered_token = recovered_claim
        .token
        .ok_or_else(|| io::Error::other("expired filing lease was not recovered"))?;
    assert_ne!(
        recovered_token, abandoned_token,
        "recovery must replace the abandoned filing lease"
    );
    store
        .complete_decision_filing(
            recoverable_escalation.approval_id,
            recovered_token,
            "PROJ-789",
            "https://jira.example.com/browse/PROJ-789",
        )
        .await?;
    let recovered_reference = sqlx::query(
        "SELECT state, decision_key, evidence_url \
         FROM approvals \
         WHERE id = $1",
    )
    .bind(recoverable_escalation.approval_id)
    .fetch_one(store.pool())
    .await?;
    assert_eq!(
        recovered_reference.try_get::<String, _>("state")?,
        "rejected"
    );
    assert_eq!(
        recovered_reference.try_get::<Option<String>, _>("decision_key")?,
        Some("PROJ-789".to_owned()),
        "a replacement worker must be able to finish the retired approval's external record"
    );
    let current_escalation = store
        .park_rejection(ParkRejection {
            task_uid: None,
            runtime_uid: "runtime-evidence-a",
            runtime_namespace: "team-a",
            runtime_name: "runtime-evidence-a",
            spec_digest: "digest-evidence-a",
            base_spec_digest: "base-digest-evidence-a",
            base_pending_approval_digest: None,
            base_spec: &base_spec,
            envelope_revision: 1,
            deltas: &deltas,
            proposed_spec: &proposed_spec,
            actor: "alice@example.com",
            member_role: "engineer-evidence",
        })
        .await?;
    assert_eq!(
        store
            .revoke_runtime_grants(
                "runtime-evidence-a",
                "admin@example.com",
                "winner revoked before convergence",
            )
            .await?,
        1
    );
    assert!(
        store
            .retire_pending_approval_if_superseded(
                current_escalation.approval_id,
                active_application.approval_id,
                "runtime-evidence-a",
                "steward-apiserver",
                "superseded by an active approval during create convergence",
            )
            .await?
            .is_none(),
        "a revoked winner must not authorize retirement"
    );
    assert!(
        store
            .pending_approvals()
            .await?
            .iter()
            .any(|pending| pending.approval_id == current_escalation.approval_id),
        "the current escalation must remain pending after winner revocation"
    );
    store
        .insert_envelope(
            "engineer-evidence",
            &envelope("150.00", 2),
            "admin@example.com",
        )
        .await?;
    assert!(
        store
            .grants_for_runtime("runtime-evidence-a", "engineer-evidence", 1)
            .await?
            .is_empty(),
        "authoring a new role envelope must atomically retire older authority"
    );
    assert!(
        store.grant_reversion("runtime-evidence-a").await?.is_some(),
        "superseding an unapplied grant must produce durable reconciliation work"
    );
    Ok(())
}

#[tokio::test]
async fn s4_retirement_checks_expiry_after_waiting_for_authority_locks()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the S4 Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(3)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let runtime_uid = format!("runtime-expiry-{suffix}");
    let member_role = format!("engineer-expiry-{suffix}");
    let proposed = proposed_spec();
    let base = base_spec();
    let deltas = budget_deltas();
    store
        .insert_envelope(&member_role, &envelope("200.00", 1), "admin@example.com")
        .await?;
    let winner = store
        .park_rejection(ParkRejection {
            task_uid: None,
            runtime_uid: &runtime_uid,
            runtime_namespace: "team-a",
            runtime_name: "runtime-expiry",
            spec_digest: &format!("winner-digest-{suffix}"),
            base_spec_digest: &format!("winner-base-digest-{suffix}"),
            base_pending_approval_digest: None,
            base_spec: &base,
            envelope_revision: 1,
            deltas: &deltas,
            proposed_spec: &proposed,
            actor: "alice@example.com",
            member_role: &member_role,
        })
        .await?;
    store
        .link_decision_reference(
            winner.approval_id,
            "PROJ-123",
            "https://jira.example.com/browse/PROJ-123",
        )
        .await?;
    let expires_at =
        sqlx::query_scalar::<_, String>("SELECT (clock_timestamp() + interval '1 second')::text")
            .fetch_one(store.pool())
            .await?;
    store
        .approve_admission(ApproveAdmission {
            approval_id: winner.approval_id,
            decided_by: "admin@example.com",
            rationale: "short-lived authority for lock timing",
            evidence_url: "https://jira.example.com/browse/PROJ-123",
            expires_at: &expires_at,
        })
        .await?;
    let loser = store
        .park_rejection(ParkRejection {
            task_uid: None,
            runtime_uid: &runtime_uid,
            runtime_namespace: "team-a",
            runtime_name: "runtime-expiry",
            spec_digest: &format!("loser-digest-{suffix}"),
            base_spec_digest: &format!("loser-base-digest-{suffix}"),
            base_pending_approval_digest: None,
            base_spec: &base,
            envelope_revision: 1,
            deltas: &deltas,
            proposed_spec: &proposed,
            actor: "alice@example.com",
            member_role: &member_role,
        })
        .await?;
    let mut blocker = store.pool().begin().await?;
    sqlx::query("SELECT id FROM approvals WHERE id = $1 FOR UPDATE")
        .bind(winner.approval_id)
        .execute(&mut *blocker)
        .await?;

    let retirement = store.retire_pending_approval_if_superseded(
        loser.approval_id,
        winner.approval_id,
        &runtime_uid,
        "steward-apiserver",
        "superseded by an active approval during create convergence",
    );
    let release = async move {
        sqlx::query("SELECT pg_sleep(2)")
            .execute(&mut *blocker)
            .await?;
        blocker.commit().await
    };
    let (retirement, release) = tokio::join!(retirement, release);
    release?;
    assert!(
        retirement?.is_none(),
        "authority that expires while retirement waits for its locks must not retire the loser"
    );
    assert!(
        store
            .pending_approvals()
            .await?
            .iter()
            .any(|approval| approval.approval_id == loser.approval_id),
        "the escalation must remain pending when the alleged winner has expired"
    );
    Ok(())
}

#[tokio::test]
async fn s4_decision_filing_claim_serializes_concurrent_retries() -> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the S4 Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let runtime_uid = format!("runtime-filing-{suffix}");
    let proposed = proposed_spec();
    let base = base_spec();
    let deltas = budget_deltas();
    let parked = store
        .park_rejection(ParkRejection {
            task_uid: None,
            runtime_uid: &runtime_uid,
            runtime_namespace: "team-a",
            runtime_name: "runtime-filing",
            spec_digest: &format!("digest-{suffix}"),
            base_spec_digest: &format!("base-digest-{suffix}"),
            base_pending_approval_digest: None,
            base_spec: &base,
            envelope_revision: 1,
            deltas: &deltas,
            proposed_spec: &proposed,
            actor: "alice@example.com",
            member_role: "engineer-filing",
        })
        .await?;

    let (left, right) = tokio::join!(
        store.claim_decision_filing(parked.approval_id),
        store.claim_decision_filing(parked.approval_id),
    );
    let (claim, blocked) = match (left, right) {
        (Ok(claim), Err(error)) | (Err(error), Ok(claim)) => (claim, error),
        result => {
            return Err(io::Error::other(format!(
                "exactly one concurrent filing claim must succeed: {result:?}"
            ))
            .into());
        }
    };
    assert_eq!(blocked, StoreError::DecisionFilingInProgress);
    let token = claim
        .token
        .ok_or_else(|| io::Error::other("new filing claim had no lease token"))?;
    store
        .complete_decision_filing(
            parked.approval_id,
            token,
            "PROJ-123",
            "https://jira.example.com/browse/PROJ-123",
        )
        .await?;
    let replay = store.claim_decision_filing(parked.approval_id).await?;
    assert_eq!(replay.token, None);
    assert_eq!(replay.filing.decision_key.as_deref(), Some("PROJ-123"));
    Ok(())
}

#[tokio::test]
async fn s4_pending_create_provenance_survives_every_authority_transition()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the S4 Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let runtime_uid = format!("runtime-provenance-{suffix}");
    let edit_runtime_uid = format!("runtime-edit-provenance-{suffix}");
    let invalid_runtime_uid = format!("runtime-invalid-provenance-{suffix}");
    let member_role = format!("engineer-provenance-{suffix}");
    let marker_digest = format!("request-digest-{suffix}");
    let proposed = proposed_spec();
    let base = base_spec();
    let deltas = budget_deltas();
    store
        .insert_envelope(&member_role, &envelope("200.00", 1), "admin@example.com")
        .await?;

    let parked = store
        .park_rejection(ParkRejection {
            task_uid: None,
            runtime_uid: &runtime_uid,
            runtime_namespace: "team-a",
            runtime_name: "runtime-provenance",
            spec_digest: &marker_digest,
            base_spec_digest: &format!("base-digest-{suffix}"),
            base_pending_approval_digest: Some(&marker_digest),
            base_spec: &base,
            envelope_revision: 1,
            deltas: &deltas,
            proposed_spec: &proposed,
            actor: "alice@example.com",
            member_role: &member_role,
        })
        .await?;
    let pending = store
        .pending_approvals()
        .await?
        .into_iter()
        .find(|approval| approval.approval_id == parked.approval_id)
        .ok_or_else(|| io::Error::other("parked approval was not queryable"))?;
    assert_eq!(
        pending.base_pending_approval_digest.as_deref(),
        Some(marker_digest.as_str()),
        "parking must retain the exact pending marker provenance"
    );

    store
        .link_decision_reference(
            parked.approval_id,
            "PROJ-123",
            "https://jira.example.com/browse/PROJ-123",
        )
        .await?;
    let candidate = store
        .approval_candidate(
            parked.approval_id,
            "https://jira.example.com/browse/PROJ-123",
        )
        .await?;
    assert_eq!(
        candidate.base_pending_approval_digest.as_deref(),
        Some(marker_digest.as_str())
    );
    assert_eq!(candidate.actor, "alice@example.com");
    store
        .approve_admission(ApproveAdmission {
            approval_id: parked.approval_id,
            decided_by: "admin@example.com",
            rationale: "bounded initial-create authority",
            evidence_url: "https://jira.example.com/browse/PROJ-123",
            expires_at: "2999-01-01T00:00:00Z",
        })
        .await?;

    let application = store
        .grant_application(&runtime_uid)
        .await?
        .ok_or_else(|| io::Error::other("active grant application was not queryable"))?;
    assert_eq!(
        application
            .application
            .base_pending_approval_digest
            .as_deref(),
        Some(marker_digest.as_str()),
        "application must retain initial-create provenance"
    );
    let retired = store
        .retire_pending_approval_if_superseded(
            parked.approval_id,
            parked.approval_id,
            &runtime_uid,
            "steward-controller",
            "validate authority before convergence",
        )
        .await?
        .ok_or_else(|| io::Error::other("active authority did not validate during retirement"))?;
    assert_eq!(
        retired.base_pending_approval_digest.as_deref(),
        Some(marker_digest.as_str()),
        "the locked retirement transition must return the same provenance"
    );

    assert_eq!(
        store
            .revoke_runtime_grants(&runtime_uid, "admin@example.com", "scope ended")
            .await?,
        1
    );
    let reversion = store
        .grant_reversion(&runtime_uid)
        .await?
        .ok_or_else(|| io::Error::other("inactive initial-create grant was not reversible"))?;
    assert_eq!(
        reversion.base_pending_approval_digest.as_deref(),
        Some(marker_digest.as_str()),
        "reversion must restore the exact marker persisted at parking"
    );

    let edit = store
        .park_rejection(ParkRejection {
            task_uid: None,
            runtime_uid: &edit_runtime_uid,
            runtime_namespace: "team-a",
            runtime_name: "runtime-edit-provenance",
            spec_digest: &format!("edit-digest-{suffix}"),
            base_spec_digest: &format!("edit-base-digest-{suffix}"),
            base_pending_approval_digest: None,
            base_spec: &base,
            envelope_revision: 1,
            deltas: &deltas,
            proposed_spec: &proposed,
            actor: "alice@example.com",
            member_role: &member_role,
        })
        .await?;
    let edit_pending = store
        .pending_approvals()
        .await?
        .into_iter()
        .find(|approval| approval.approval_id == edit.approval_id)
        .ok_or_else(|| io::Error::other("edit approval was not queryable"))?;
    assert_eq!(
        edit_pending.base_pending_approval_digest, None,
        "ordinary edit escalation must not acquire initial-create provenance"
    );

    let empty_marker = store
        .park_rejection(ParkRejection {
            task_uid: None,
            runtime_uid: &invalid_runtime_uid,
            runtime_namespace: "team-a",
            runtime_name: "runtime-invalid-provenance",
            spec_digest: &format!("invalid-digest-{suffix}"),
            base_spec_digest: &format!("invalid-base-digest-{suffix}"),
            base_pending_approval_digest: Some(""),
            base_spec: &base,
            envelope_revision: 1,
            deltas: &deltas,
            proposed_spec: &proposed,
            actor: "alice@example.com",
            member_role: &member_role,
        })
        .await;
    assert!(
        empty_marker.is_err(),
        "the migration must reject empty pending-marker provenance"
    );
    Ok(())
}

#[tokio::test]
async fn consumed_connection_output_retires_without_rewriting_execution_history()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL")?;
    let bootstrap = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await?;
    // Each case has its own schema. The run-owned Postgres instance is removed
    // unconditionally by the harness, including schemas from failed cases.
    for (upgrade, reject_result) in [(false, false), (false, true), (true, false), (true, true)] {
        let suffix = Uuid::new_v4().simple().to_string();
        let schema = format!("connection_output_{suffix}");
        sqlx::query(&format!("CREATE SCHEMA \"{schema}\""))
            .execute(&bootstrap)
            .await?;
        let options = database_url
            .parse::<PgConnectOptions>()?
            .options([("search_path", schema.as_str())]);
        let mut pool = PgPoolOptions::new()
            .max_connections(8)
            .connect_with(options.clone())
            .await?;
        let mut store = PgStore::new(pool.clone());
        if upgrade {
            let mut historical = sqlx::migrate!("../migrations");
            historical
                .migrations
                .to_mut()
                .retain(|migration| migration.version <= 32);
            historical.run(&pool).await?;
        } else {
            store.migrate().await?;
        }
        let email = Email::parse(format!("alice-{suffix}@example.com"))?;
        let principal = store
            .register_canonical_identity(
                &google_identity(&suffix, email.as_str())?,
                "identity-admin",
            )
            .await?;
        let reservation = reserve_governed_connection(
            &store,
            &principal.user_id,
            &email,
            ConnectionOperationKind::Status,
            false,
            &suffix,
        )
        .await?;
        let task_uid = reservation.record.task_uid;
        let runtime_uid = format!("runtime-{suffix}");
        store
            .authorize_task_runtime_creation(task_uid, 1, "controller-a")
            .await?;
        store
            .record_task_runtime_observed(task_uid, 2, &runtime_uid, "1", "controller-a")
            .await?;
        let task = store
            .task(task_uid)
            .await?
            .ok_or(StoreError::TaskNotFound)?;
        for generation in [3, 4] {
            store
                .decide_task_runtime_authority(
                    task_uid,
                    generation,
                    &steward_connections_v1::envelope(),
                    task.service_envelope_digest
                        .as_deref()
                        .ok_or(StoreError::InvalidTaskTransition)?,
                    "controller-a",
                )
                .await?;
        }
        let operation = store
            .task_runtime_operation(task_uid)
            .await?
            .ok_or(StoreError::TaskNotFound)?;
        store
            .record_task_activation_observed(
                task_uid,
                5,
                &TaskActivationObservation {
                    runtime_uid: &runtime_uid,
                    resource_version: "2",
                    active_manifest_digest: &operation.active_manifest_digest,
                    provider_set_ready: true,
                },
                "controller-a",
            )
            .await?;
        let digest = format!("sha256:{}", "a".repeat(64));
        let attempt = match store
            .claim_task_execution_attempt(task_uid, &digest, &digest, "controller-a")
            .await?
        {
            TaskExecutionTransition::Created(attempt) => attempt,
            other => {
                return Err(
                    io::Error::other(format!("unexpected execution claim: {other:?}")).into(),
                );
            }
        };
        store
            .authorize_task_execution_start(attempt.attempt_id, 1, "controller-a")
            .await?;
        store
            .record_task_execution_observation(
                attempt.attempt_id,
                2,
                TaskExecutionObservation::Succeeded {
                    adapter_observation_id: "adapter-result",
                    result_digest: &digest,
                    result_reference: "adapter:result",
                    output_archive: b"neutral transient connection output",
                    execution_stdout: None,
                    execution_stderr: None,
                },
                "controller-a",
            )
            .await?;
        if upgrade {
            // Reproduce the pre-migration failure with a populated successful
            // Task, then run the normal checksum-checked migration path.
            assert!(matches!(
                store
                    .complete_connection_operation(
                        task_uid,
                        &serde_json::json!({"connected": false}),
                        None,
                        None,
                        connection_operation_retention(),
                    )
                    .await,
                Err(StoreError::Database(_))
            ));
            let before = store
                .task(task_uid)
                .await?
                .ok_or(StoreError::TaskNotFound)?;
            let history_before = store.task_execution_attempt(task_uid).await?;
            store.migrate().await?;
            // The staged rollout replaces the old processes. Reconnect after
            // DDL rather than reusing pre-upgrade SELECT * statement caches.
            pool.close().await;
            pool = PgPoolOptions::new()
                .max_connections(8)
                .connect_with(options)
                .await?;
            store = PgStore::new(pool.clone());
            assert_eq!(store.task(task_uid).await?, Some(before));
            assert_eq!(
                store.task_execution_attempt(task_uid).await?,
                history_before
            );
        }
        // Before the connection result is consumed, even an internal Task's
        // output cannot be silently erased or replaced.
        for replacement in [None, Some(b"different output".as_slice())] {
            assert!(
                sqlx::query("UPDATE task_submissions SET output_archive = $2 WHERE task_uid = $1")
                    .bind(task_uid)
                    .bind(replacement)
                    .execute(&pool)
                    .await
                    .is_err()
            );
        }
        // A terminal connection result alone is insufficient: retiring its
        // output must request Task cleanup in the same transaction.
        let mut premature = pool.begin().await?;
        sqlx::query("UPDATE connection_operations SET operation_state = 'failed', failure_category = 'test_rejection', finalization_state = 'requested', cleanup_state = 'tearing_down' WHERE task_uid = $1")
            .bind(task_uid).execute(&mut *premature).await?;
        assert!(
            sqlx::query("UPDATE task_submissions SET output_archive = NULL WHERE task_uid = $1")
                .bind(task_uid)
                .execute(&mut *premature)
                .await
                .is_err()
        );
        premature.rollback().await?;
        if reject_result {
            store
                .fail_connection_operation(task_uid, "invalid_bridge_result")
                .await?;
        } else {
            store
                .complete_connection_operation(
                    task_uid,
                    &serde_json::json!({"connected": false}),
                    None,
                    None,
                    connection_operation_retention(),
                )
                .await?;
        }
        let retired = store
            .task(task_uid)
            .await?
            .ok_or(StoreError::TaskNotFound)?;
        assert_eq!(retired.phase, TaskPhase::Succeeded);
        assert!(retired.finalize_requested);
        assert!(
            retired.output_archive.is_none(),
            "transient connection output must not be retained after consumption"
        );
        let history = store
            .task_execution_attempt(task_uid)
            .await?
            .ok_or(StoreError::TaskNotFound)?;
        assert_eq!(history.attempt_id, attempt.attempt_id);
        assert_eq!(history.runtime_uid, runtime_uid);
        assert_eq!(history.state, TaskExecutionAttemptState::Succeeded);
        assert_eq!(history.result_digest.as_deref(), Some(digest.as_str()));
        let retired_at: Option<String> = sqlx::query_scalar(
            "SELECT internal_output_retired_at::text FROM task_submissions WHERE task_uid = $1",
        )
        .bind(task_uid)
        .fetch_one(&pool)
        .await?;
        assert!(retired_at.is_some());
        assert!(
            sqlx::query(
                "UPDATE task_submissions SET internal_output_retired_at = NULL WHERE task_uid = $1"
            )
            .bind(task_uid)
            .execute(&pool)
            .await
            .is_err(),
            "payload retirement evidence is immutable"
        );
        assert!(
            sqlx::query("UPDATE task_submissions SET output_archive = $2 WHERE task_uid = $1")
                .bind(task_uid)
                .bind(b"restored output".as_slice())
                .execute(&pool)
                .await
                .is_err(),
            "retirement must not reopen the initial output write"
        );
        assert!(
            sqlx::query(
                "UPDATE task_execution_attempts SET result_digest = $2 WHERE attempt_id = $1"
            )
            .bind(attempt.attempt_id)
            .bind(format!("sha256:{}", "b".repeat(64)))
            .execute(&pool)
            .await
            .is_err(),
            "retiring a payload does not permit rewriting its immutable result identity"
        );
    }
    Ok(())
}

#[tokio::test]
async fn governed_connection_operations_are_serialized_restart_safe_and_hidden_from_agent_runs()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other(
            "STEWARD_TEST_DATABASE_URL is required for the connection-operation Postgres test",
        )
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool.clone());
    store.migrate().await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let email = Email::parse(format!("connections-{suffix}@example.com"))?;
    let identity = google_identity(format!("connections-subject-{suffix}"), email.as_str())?;
    let principal = store
        .register_canonical_identity(&identity, "identity-admin")
        .await?;

    let first_status_key = format!("status-a-{suffix}");
    let second_status_key = format!("status-b-{suffix}");
    let (first_status, second_status) = tokio::join!(
        reserve_governed_connection(
            &store,
            &principal.user_id,
            &email,
            ConnectionOperationKind::Status,
            true,
            &first_status_key,
        ),
        reserve_governed_connection(
            &store,
            &principal.user_id,
            &email,
            ConnectionOperationKind::Status,
            true,
            &second_status_key,
        )
    );
    let first_status = first_status?;
    let second_status = second_status?;
    assert_ne!(
        first_status.inserted, second_status.inserted,
        "concurrent status calls must create exactly one bridge operation"
    );
    let (status, joined_status) = if first_status.inserted {
        (first_status, second_status)
    } else {
        (second_status, first_status)
    };
    assert_eq!(
        joined_status.record.operation_id,
        status.record.operation_id
    );
    let task_authority: (Option<String>, Option<i64>, Option<String>) = sqlx::query_as(
        "SELECT internal_authority_id, internal_authority_version, internal_authority_digest \
         FROM task_submissions WHERE task_uid = $1",
    )
    .bind(status.record.task_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        task_authority,
        (
            Some("steward-connections".to_owned()),
            Some(1),
            Some(
                steward_admission::internal_authorities::steward_connections_v1::AUTHORITY_DIGEST
                    .to_owned()
            ),
        ),
        "the internal task must persist the same exact authority pins as its operation projection"
    );
    let divergent_authority = sqlx::query(
        "UPDATE task_submissions SET internal_authority_digest = $2 WHERE task_uid = $1",
    )
    .bind(status.record.task_uid)
    .bind(format!("sha256:{}", "b".repeat(64)))
    .execute(&pool)
    .await;
    assert!(
        divergent_authority.is_err(),
        "the task authority pins must not diverge from the connection-operation projection"
    );
    let partial_authority = sqlx::query(
        "INSERT INTO task_submissions \
         (task_uid, idempotency_key, submitter_service, acting_user, acting_user_id, owner, \
          owner_user_id, identity_binding_state, workflow, coding_agent_runtime, \
          runtime_namespace, runtime_name, runtime_ownership, phase, runtime_spec, \
          agent_command, input_archive, execute_requested, envelope_revision, \
          internal_authority_id) \
         SELECT gen_random_uuid(), idempotency_key || '-partial-authority', \
                submitter_service, acting_user, acting_user_id, owner, owner_user_id, \
                identity_binding_state, workflow, coding_agent_runtime, runtime_namespace, \
                runtime_name, runtime_ownership, phase, runtime_spec, agent_command, \
                input_archive, execute_requested, envelope_revision, 'partial-authority' \
         FROM task_submissions WHERE task_uid = $1",
    )
    .bind(status.record.task_uid)
    .execute(&pool)
    .await;
    assert!(
        partial_authority.is_err(),
        "internal task authority pins must be either all absent or all present"
    );
    let bridge_runtime_name = format!("conn-{}", status.record.operation_id.simple());
    assert!(
        store
            .connection_runtime_admission(&status.record.bindings.namespace, &bridge_runtime_name,)
            .await?
            .is_none(),
        "recorded intent must not authorize a Kubernetes CREATE before the durable effect fence"
    );
    assert!(
        store
            .connection_runtime_admission(
                &status.record.bindings.namespace,
                &format!("unrelated-{suffix}"),
            )
            .await?
            .is_none(),
        "an unrelated runtime name must not resolve internal connection authority"
    );
    assert!(matches!(
        store
            .authorize_task_runtime_creation(status.record.task_uid, 1, "controller-a")
            .await?,
        TaskOperationTransition::Applied(_)
    ));
    let admission = store
        .connection_runtime_admission(&status.record.bindings.namespace, &bridge_runtime_name)
        .await?
        .ok_or_else(|| {
            io::Error::other(
                "authorized server-authored runtime name did not resolve its admission state",
            )
        })?;
    assert_eq!(
        admission.connection.operation_id,
        status.record.operation_id
    );
    assert_eq!(
        admission.orchestration_state,
        TaskOrchestrationState::RuntimeCreatePending
    );
    assert!(admission.runtime_create_authorized);
    sqlx::query(
        "UPDATE connection_operations SET response_deadline_at = now() - interval '1 second' \
         WHERE operation_id = $1",
    )
    .bind(status.record.operation_id)
    .execute(&pool)
    .await?;
    assert!(
        store
            .connection_runtime_admission(&status.record.bindings.namespace, &bridge_runtime_name,)
            .await?
            .is_none(),
        "an elapsed response deadline must revoke prospective CREATE authority"
    );
    sqlx::query(
        "UPDATE connection_operations SET response_deadline_at = now() + interval '40 seconds' \
         WHERE operation_id = $1",
    )
    .bind(status.record.operation_id)
    .execute(&pool)
    .await?;
    let bridge_runtime_uid = format!("bridge-runtime-{suffix}");
    assert!(matches!(
        store
            .record_task_runtime_observed(
                status.record.task_uid,
                2,
                &bridge_runtime_uid,
                "resource-version-bridge",
                "controller-a",
            )
            .await?,
        TaskOperationTransition::Applied(_)
    ));
    assert!(
        store
            .connection_runtime_admission(&status.record.bindings.namespace, &bridge_runtime_name,)
            .await?
            .is_none(),
        "an observed runtime must not replay its initial CREATE authority"
    );
    let task = store
        .task(status.record.task_uid)
        .await?
        .ok_or(StoreError::TaskNotFound)?;
    let operation = store
        .task_runtime_operation(status.record.task_uid)
        .await?
        .ok_or(StoreError::TaskNotFound)?;
    assert!(matches!(
        store
            .decide_task_runtime_authority(
                status.record.task_uid,
                3,
                &steward_connections_v1::envelope(),
                task.service_envelope_digest
                    .as_deref()
                    .ok_or(StoreError::InvalidTaskTransition)?,
                "controller-a",
            )
            .await?,
        TaskOperationTransition::Applied(_)
    ));
    assert!(matches!(
        store
            .decide_task_runtime_authority(
                status.record.task_uid,
                4,
                &steward_connections_v1::envelope(),
                task.service_envelope_digest
                    .as_deref()
                    .ok_or(StoreError::InvalidTaskTransition)?,
                "controller-a",
            )
            .await?,
        TaskOperationTransition::Applied(_)
    ));
    let activation_admission = store
        .connection_runtime_admission(&status.record.bindings.namespace, &bridge_runtime_name)
        .await?
        .ok_or_else(|| {
            io::Error::other("authorized connection activation did not resolve its admission state")
        })?;
    assert_eq!(
        activation_admission.orchestration_state,
        TaskOrchestrationState::ActivationPending
    );
    assert!(activation_admission.activation_effect_authorized);
    assert_eq!(
        activation_admission.connection.runtime_uid.as_deref(),
        Some(bridge_runtime_uid.as_str())
    );
    assert!(matches!(
        store
            .record_task_activation_observed(
                status.record.task_uid,
                5,
                &TaskActivationObservation {
                    runtime_uid: &bridge_runtime_uid,
                    resource_version: "resource-version-bridge-active",
                    active_manifest_digest: &operation.active_manifest_digest,
                    provider_set_ready: true,
                },
                "controller-a",
            )
            .await?,
        TaskOperationTransition::Applied(_)
    ));
    assert!(
        store
            .connection_runtime_admission(&status.record.bindings.namespace, &bridge_runtime_name,)
            .await?
            .is_none(),
        "an active runtime must not replay its activation admission authority"
    );
    let by_runtime = store
        .connection_operation_for_runtime(&bridge_runtime_uid)
        .await?
        .ok_or_else(|| {
            io::Error::other("exact bridge runtime UID did not resolve its internal authority")
        })?;
    assert_eq!(by_runtime.operation_id, status.record.operation_id);
    assert_eq!(
        by_runtime.runtime_uid.as_deref(),
        Some(bridge_runtime_uid.as_str())
    );
    assert!(
        store
            .connection_operation_for_runtime(&format!("unrelated-{suffix}"))
            .await?
            .is_none(),
        "an unrelated or long-running runtime UID must not resolve bridge authority"
    );
    let disconnect = reserve_governed_connection(
        &store,
        &principal.user_id,
        &email,
        ConnectionOperationKind::Disconnect,
        true,
        &format!("disconnect-{suffix}"),
    )
    .await?;
    assert!(
        disconnect.inserted,
        "a mutation must preempt an in-flight status read"
    );
    assert_eq!(
        store
            .connection_operation(status.record.operation_id, &principal.user_id)
            .await?
            .ok_or_else(|| io::Error::other("preempted status operation disappeared"))?
            .operation_state,
        ConnectionOperationState::Failed,
        "mutation precedence must durably stop the in-flight status operation"
    );
    assert_eq!(
        reserve_governed_connection(
            &store,
            &principal.user_id,
            &email,
            ConnectionOperationKind::Status,
            false,
            &format!("status-during-disconnect-{suffix}"),
        )
        .await,
        Err(StoreError::ConnectionOperationConflict),
        "polling must not create a second runtime while a mutation is active"
    );
    store
        .complete_connection_operation(
            disconnect.record.operation_id,
            &serde_json::json!({"disconnected": true}),
            None,
            None,
            connection_operation_retention(),
        )
        .await?;
    assert_eq!(
        store
            .fail_connection_operation(disconnect.record.operation_id, "late_failure")
            .await,
        Err(StoreError::InvalidConnectionOperation),
        "a stale reconciler must never overwrite an atomically persisted success"
    );
    assert_eq!(
        store
            .connection_operation(disconnect.record.operation_id, &principal.user_id)
            .await?
            .ok_or_else(|| io::Error::other("completed disconnect disappeared"))?
            .operation_state,
        ConnectionOperationState::Succeeded
    );
    sqlx::query(
        "UPDATE connection_operations SET updated_at = now() - interval '151 seconds' \
         WHERE operation_id = $1",
    )
    .bind(disconnect.record.operation_id)
    .execute(&pool)
    .await?;
    assert!(
        store
            .mark_stalled_connection_cleanup(disconnect.record.operation_id, 150)
            .await?,
        "teardown beyond the fixed grace period must create a durable finding"
    );
    let stalled = store
        .connection_operation(disconnect.record.operation_id, &principal.user_id)
        .await?
        .ok_or_else(|| io::Error::other("stalled cleanup audit record disappeared"))?;
    assert_eq!(stalled.cleanup_state, "stalled");
    assert_eq!(stalled.cleanup_finding.as_deref(), Some("teardown_stalled"));

    assert!(
        store.agent_run(disconnect.record.task_uid).await?.is_none(),
        "connection operations must not appear in agent-run detail"
    );
    assert!(
        store
            .agent_run_timeline(disconnect.record.task_uid)
            .await?
            .is_none(),
        "connection operations must not appear in agent-run timelines"
    );
    assert!(
        store
            .task_for_submitter(
                disconnect.record.task_uid,
                CONNECTIONS_SERVICE,
                principal.user_id.as_str(),
            )
            .await?
            .is_none(),
        "connection operations must not appear in generic task APIs"
    );

    let first_start_key = format!("start-a-{suffix}");
    let second_start_key = format!("start-b-{suffix}");
    let (first_start, second_start) = tokio::join!(
        reserve_governed_connection(
            &store,
            &principal.user_id,
            &email,
            ConnectionOperationKind::Start,
            true,
            &first_start_key,
        ),
        reserve_governed_connection(
            &store,
            &principal.user_id,
            &email,
            ConnectionOperationKind::Start,
            true,
            &second_start_key,
        )
    );
    let first_start = first_start?;
    let second_start = second_start?;
    assert_ne!(
        first_start.inserted, second_start.inserted,
        "concurrent starts must create exactly one OAuth bridge operation"
    );
    let (start, joined_start) = if first_start.inserted {
        (first_start, second_start)
    } else {
        (second_start, first_start)
    };
    assert_eq!(joined_start.record.operation_id, start.record.operation_id);
    let authorization_url =
        format!("https://github.example.test/login/oauth/authorize?state={suffix}");
    store
        .complete_connection_operation(
            start.record.operation_id,
            &serde_json::json!({"authorizationUrl": authorization_url}),
            Some(&authorization_url),
            Some(&format!("sha256:{}", "a".repeat(64))),
            connection_operation_retention(),
        )
        .await?;
    let reused_start = reserve_governed_connection(
        &store,
        &principal.user_id,
        &email,
        ConnectionOperationKind::Start,
        true,
        &format!("start-duplicate-{suffix}"),
    )
    .await?;
    assert!(!reused_start.inserted);
    assert_eq!(reused_start.record.operation_id, start.record.operation_id);
    assert_eq!(
        reused_start.record.oauth_phase,
        ConnectionOAuthPhase::Pending
    );
    assert_eq!(
        reserve_governed_connection(
            &store,
            &principal.user_id,
            &email,
            ConnectionOperationKind::Disconnect,
            true,
            &format!("disconnect-pending-{suffix}"),
        )
        .await,
        Err(StoreError::ConnectionOAuthFlowPending)
    );

    let pending_disconnected_status = reserve_governed_connection(
        &store,
        &principal.user_id,
        &email,
        ConnectionOperationKind::Status,
        false,
        &format!("pending-disconnected-status-{suffix}"),
    )
    .await?;
    assert!(pending_disconnected_status.record.uncached_status);
    store
        .complete_connection_operation(
            pending_disconnected_status.record.operation_id,
            &serde_json::json!({"connected": false}),
            None,
            None,
            connection_operation_retention(),
        )
        .await?;
    let post_callback_status = reserve_governed_connection(
        &store,
        &principal.user_id,
        &email,
        ConnectionOperationKind::Status,
        true,
        &format!("post-callback-status-{suffix}"),
    )
    .await?;
    assert!(
        post_callback_status.inserted,
        "a disconnected cache cannot hide an OAuth callback while its flow remains pending"
    );
    assert_ne!(
        post_callback_status.record.operation_id,
        pending_disconnected_status.record.operation_id
    );
    store
        .complete_connection_operation(
            post_callback_status.record.operation_id,
            &serde_json::json!({
                "connected": true,
                "email": email.as_str(),
                "scopesRequired": [],
                "scopesGranted": [],
                "missingScopes": []
            }),
            None,
            None,
            connection_operation_retention(),
        )
        .await?;
    store
        .complete_pending_connection_oauth_flow(&principal.user_id)
        .await?;
    let post_callback_disconnect = reserve_governed_connection(
        &store,
        &principal.user_id,
        &email,
        ConnectionOperationKind::Disconnect,
        true,
        &format!("disconnect-after-callback-{suffix}"),
    )
    .await?;
    assert!(post_callback_disconnect.inserted);
    let redacted_start = store
        .connection_operation(start.record.operation_id, &principal.user_id)
        .await?
        .ok_or_else(|| io::Error::other("start audit record disappeared"))?;
    assert_eq!(redacted_start.oauth_phase, ConnectionOAuthPhase::Completed);
    assert_eq!(redacted_start.authorization_url, None);
    store
        .complete_connection_operation(
            post_callback_disconnect.record.operation_id,
            &serde_json::json!({"disconnected": true}),
            None,
            None,
            connection_operation_retention(),
        )
        .await?;

    let expiring_start = reserve_governed_connection(
        &store,
        &principal.user_id,
        &email,
        ConnectionOperationKind::Start,
        true,
        &format!("expiring-start-{suffix}"),
    )
    .await?;
    store
        .complete_connection_operation(
            expiring_start.record.operation_id,
            &serde_json::json!({"authorizationUrl": authorization_url}),
            Some(&authorization_url),
            Some(&format!("sha256:{}", "b".repeat(64))),
            connection_operation_retention(),
        )
        .await?;
    let lifetime_seconds: f64 = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM (flow_expires_at - flow_created_at))::float8 \
         FROM connection_operations WHERE operation_id = $1",
    )
    .bind(expiring_start.record.operation_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        lifetime_seconds, 630.0,
        "the 600-second upstream lifetime must retain the 30-second conservative skew buffer"
    );
    sqlx::query(
        "UPDATE connection_operations SET flow_expires_at = now() - interval '1 second' \
         WHERE operation_id = $1",
    )
    .bind(expiring_start.record.operation_id)
    .execute(&pool)
    .await?;
    let replacement_start = reserve_governed_connection(
        &store,
        &principal.user_id,
        &email,
        ConnectionOperationKind::Start,
        true,
        &format!("replacement-start-{suffix}"),
    )
    .await?;
    assert!(replacement_start.inserted);
    assert_ne!(
        replacement_start.record.operation_id,
        expiring_start.record.operation_id
    );
    let expired_start = store
        .connection_operation(expiring_start.record.operation_id, &principal.user_id)
        .await?
        .ok_or_else(|| io::Error::other("expired start audit record disappeared"))?;
    assert_eq!(expired_start.oauth_phase, ConnectionOAuthPhase::Expired);
    assert_eq!(expired_start.authorization_url, None);
    Ok(())
}

#[tokio::test]
async fn governed_connection_reuse_is_bound_to_artifact_trust_and_full_execution_snapshot()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other(
            "STEWARD_TEST_DATABASE_URL is required for the connection binding Postgres test",
        )
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool.clone());
    store.migrate().await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let email = Email::parse(format!("connection-binding-{suffix}@example.com"))?;
    let identity = google_identity(
        format!("connection-binding-subject-{suffix}"),
        email.as_str(),
    )?;
    let principal = store
        .register_canonical_identity(&identity, "identity-admin")
        .await?;
    let github_bindings = governed_connection_bindings();
    let operator_bindings = ConnectionExecutionBindings {
        artifact_trust_mode: "operator-pinned".to_owned(),
        ..github_bindings.clone()
    };

    let github_status = reserve_governed_connection_with_bindings(
        &store,
        &principal.user_id,
        &email,
        ConnectionOperationKind::Status,
        true,
        &format!("github-active-{suffix}"),
        github_bindings.clone(),
    )
    .await?;
    assert!(
        reserve_governed_connection_with_bindings(
            &store,
            &principal.user_id,
            &email,
            ConnectionOperationKind::Status,
            true,
            &format!("github-active-{suffix}"),
            operator_bindings.clone(),
        )
        .await
        .is_err(),
        "an identical retry must not reinterpret its persisted operation under a new binding"
    );
    let original_retry = store
        .connection_operation(github_status.record.operation_id, &principal.user_id)
        .await?
        .ok_or_else(|| io::Error::other("original retry operation disappeared"))?;
    assert_eq!(
        original_retry.bindings.artifact_trust_mode,
        "github-attestation"
    );
    assert_eq!(
        original_retry.operation_state,
        ConnectionOperationState::Queued
    );
    let operator_status = reserve_governed_connection_with_bindings(
        &store,
        &principal.user_id,
        &email,
        ConnectionOperationKind::Status,
        true,
        &format!("operator-replacement-{suffix}"),
        operator_bindings.clone(),
    )
    .await?;
    assert!(operator_status.inserted);
    assert_ne!(
        operator_status.record.operation_id, github_status.record.operation_id,
        "an active operation must never be reused under a different trust mode"
    );
    let drifted = store
        .connection_operation(github_status.record.operation_id, &principal.user_id)
        .await?
        .ok_or_else(|| io::Error::other("drifted operation disappeared"))?;
    assert_eq!(drifted.operation_state, ConnectionOperationState::Failed);
    assert_eq!(
        drifted.failure_category.as_deref(),
        Some("binding_mismatch")
    );
    assert_eq!(drifted.finalization_state, "requested");
    assert!(drifted.finalize_requested);
    assert_eq!(
        operator_status.record.bindings.artifact_trust_mode,
        "operator-pinned"
    );

    store
        .complete_connection_operation(
            operator_status.record.operation_id,
            &serde_json::json!({"connected": true}),
            None,
            None,
            connection_operation_retention(),
        )
        .await?;
    let replacement_github_status = reserve_governed_connection_with_bindings(
        &store,
        &principal.user_id,
        &email,
        ConnectionOperationKind::Status,
        true,
        &format!("github-after-cache-{suffix}"),
        github_bindings.clone(),
    )
    .await?;
    assert!(
        replacement_github_status.inserted,
        "a cached status must not survive an execution-binding change"
    );
    let invalidated_cache: bool = sqlx::query_scalar(
        "SELECT cache_expires_at IS NULL FROM connection_operations WHERE operation_id = $1",
    )
    .bind(operator_status.record.operation_id)
    .fetch_one(&pool)
    .await?;
    assert!(invalidated_cache);
    store
        .complete_connection_operation(
            replacement_github_status.record.operation_id,
            &serde_json::json!({"connected": false}),
            None,
            None,
            connection_operation_retention(),
        )
        .await?;

    let github_start = reserve_governed_connection_with_bindings(
        &store,
        &principal.user_id,
        &email,
        ConnectionOperationKind::Start,
        true,
        &format!("github-start-{suffix}"),
        github_bindings.clone(),
    )
    .await?;
    let authorization_url =
        format!("https://github.example.test/login/oauth/authorize?state={suffix}");
    store
        .complete_connection_operation(
            github_start.record.operation_id,
            &serde_json::json!({"authorizationUrl": authorization_url}),
            Some(&authorization_url),
            Some(&format!("sha256:{}", "c".repeat(64))),
            connection_operation_retention(),
        )
        .await?;
    assert_eq!(
        reserve_governed_connection_with_bindings(
            &store,
            &principal.user_id,
            &email,
            ConnectionOperationKind::Start,
            true,
            &format!("operator-start-{suffix}"),
            operator_bindings,
        )
        .await,
        Err(StoreError::ConnectionOAuthFlowPending),
        "an outstanding OAuth URL must never be returned under changed execution bindings"
    );
    let expired_flow = store
        .connection_operation(github_start.record.operation_id, &principal.user_id)
        .await?
        .ok_or_else(|| io::Error::other("mismatched OAuth operation disappeared"))?;
    assert_eq!(expired_flow.oauth_phase, ConnectionOAuthPhase::Pending);
    assert_eq!(
        expired_flow.authorization_url.as_deref(),
        Some(authorization_url.as_str()),
        "binding drift must not falsely claim that MCP-GW's outstanding OAuth state expired"
    );
    assert_eq!(
        reserve_governed_connection_with_bindings(
            &store,
            &principal.user_id,
            &email,
            ConnectionOperationKind::Start,
            true,
            &format!("operator-start-still-conflicting-{suffix}"),
            ConnectionExecutionBindings {
                artifact_trust_mode: "operator-pinned".to_owned(),
                ..github_bindings.clone()
            },
        )
        .await,
        Err(StoreError::ConnectionOAuthFlowPending),
        "retries must remain blocked until the external OAuth flow actually expires"
    );
    sqlx::query(
        "UPDATE connection_operations SET flow_expires_at = now() - interval '1 second' \
         WHERE operation_id = $1",
    )
    .bind(github_start.record.operation_id)
    .execute(&pool)
    .await?;
    let replacement_operator_start = reserve_governed_connection_with_bindings(
        &store,
        &principal.user_id,
        &email,
        ConnectionOperationKind::Start,
        true,
        &format!("operator-start-retry-{suffix}"),
        ConnectionExecutionBindings {
            artifact_trust_mode: "operator-pinned".to_owned(),
            ..github_bindings
        },
    )
    .await?;
    assert!(replacement_operator_start.inserted);
    let expired_flow = store
        .connection_operation(github_start.record.operation_id, &principal.user_id)
        .await?
        .ok_or_else(|| io::Error::other("expired OAuth operation disappeared"))?;
    assert_eq!(expired_flow.oauth_phase, ConnectionOAuthPhase::Expired);
    assert_eq!(expired_flow.authorization_url, None);

    let digest_email = Email::parse(format!("connection-digest-{suffix}@example.org"))?;
    let digest_identity = google_identity_for(
        "example.org",
        OrganizationId::parse("org_example")?,
        format!("connection-digest-subject-{suffix}"),
        digest_email.as_str(),
    )?;
    let digest_principal = store
        .register_canonical_identity(&digest_identity, "identity-admin")
        .await?;
    let first_operator_bindings = ConnectionExecutionBindings {
        artifact_trust_mode: "operator-pinned".to_owned(),
        ..governed_connection_bindings()
    };
    let first_disconnect = reserve_governed_connection_with_bindings(
        &store,
        &digest_principal.user_id,
        &digest_email,
        ConnectionOperationKind::Disconnect,
        true,
        &format!("operator-disconnect-a-{suffix}"),
        first_operator_bindings.clone(),
    )
    .await?;
    store
        .complete_connection_operation(
            first_disconnect.record.operation_id,
            &serde_json::json!({"disconnected": true}),
            None,
            None,
            connection_operation_retention(),
        )
        .await?;
    let changed_digest_bindings = ConnectionExecutionBindings {
        bridge_image_digest:
            "ghcr.io/example-org/steward-connections-bridge@sha256:1123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .to_owned(),
        ..first_operator_bindings
    };
    let replacement_disconnect = reserve_governed_connection_with_bindings(
        &store,
        &digest_principal.user_id,
        &digest_email,
        ConnectionOperationKind::Disconnect,
        true,
        &format!("operator-disconnect-b-{suffix}"),
        changed_digest_bindings,
    )
    .await?;
    assert!(
        replacement_disconnect.inserted,
        "a completed mutation result must not be reused after only the image digest changes"
    );
    let invalidated_result: bool = sqlx::query_scalar(
        "SELECT result_expires_at IS NULL FROM connection_operations WHERE operation_id = $1",
    )
    .bind(first_disconnect.record.operation_id)
    .fetch_one(&pool)
    .await?;
    assert!(invalidated_result);

    let default_expression: Option<String> = sqlx::query_scalar(
        "SELECT column_default FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name = 'connection_operations' \
           AND column_name = 'artifact_trust_mode'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        default_expression.as_deref(),
        Some("'github-attestation'::text"),
        "the forward migration must preserve old writers during rollout"
    );
    Ok(())
}

#[tokio::test]
async fn connection_artifact_trust_migration_backfills_populated_state_and_keeps_old_writers()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other(
            "STEWARD_TEST_DATABASE_URL is required for the connection migration Postgres test",
        )
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool.clone());
    store.migrate().await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let email = Email::parse(format!("migration-{suffix}@example.com"))?;
    let identity = google_identity(format!("migration-subject-{suffix}"), email.as_str())?;
    let principal = store
        .register_canonical_identity(&identity, "identity-admin")
        .await?;
    let source = reserve_governed_connection(
        &store,
        &principal.user_id,
        &email,
        ConnectionOperationKind::Status,
        true,
        &format!("migration-source-{suffix}"),
    )
    .await?;

    let schema = format!("connection_migration_{suffix}");
    let mut transaction = pool.begin().await?;
    sqlx::query(&format!("CREATE SCHEMA \"{schema}\""))
        .execute(&mut *transaction)
        .await?;
    sqlx::query(&format!("SET LOCAL search_path TO \"{schema}\""))
        .execute(&mut *transaction)
        .await?;
    let migration_directory =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../migrations");
    let mut migrations = fs::read_dir(&migration_directory)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("sql"))
        .collect::<Vec<_>>();
    migrations.sort();
    for migration in migrations.iter().filter(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name < "0024_")
    }) {
        let sql = fs::read_to_string(migration)?;
        sqlx::raw_sql(&sql).execute(&mut *transaction).await?;
    }
    sqlx::query(
        "INSERT INTO canonical_users SELECT * FROM public.canonical_users WHERE user_id = $1",
    )
    .bind(principal.user_id.as_str())
    .execute(&mut *transaction)
    .await?;
    let prechange_task_columns = sqlx::query_scalar::<_, String>(
        "SELECT column_name FROM information_schema.columns \
         WHERE table_schema = $1 AND table_name = 'task_submissions' \
         ORDER BY ordinal_position",
    )
    .bind(&schema)
    .fetch_all(&mut *transaction)
    .await?;
    let task_columns = prechange_task_columns
        .iter()
        .map(|column| format!("\"{column}\""))
        .collect::<Vec<_>>()
        .join(", ");
    sqlx::query(&format!(
        "INSERT INTO task_submissions ({task_columns}) \
         SELECT {task_columns} FROM public.task_submissions WHERE task_uid = $1"
    ))
    .bind(source.record.task_uid)
    .execute(&mut *transaction)
    .await?;
    let prechange_columns = sqlx::query_scalar::<_, String>(
        "SELECT column_name FROM information_schema.columns \
         WHERE table_schema = $1 AND table_name = 'connection_operations' \
         ORDER BY ordinal_position",
    )
    .bind(&schema)
    .fetch_all(&mut *transaction)
    .await?;
    let columns = prechange_columns
        .iter()
        .map(|column| format!("\"{column}\""))
        .collect::<Vec<_>>()
        .join(", ");
    sqlx::query(&format!(
        "INSERT INTO connection_operations ({columns}) \
         SELECT {columns} FROM public.connection_operations WHERE operation_id = $1"
    ))
    .bind(source.record.operation_id)
    .execute(&mut *transaction)
    .await?;
    let forward_migration = fs::read_to_string(
        migration_directory.join("0024_connection_operation_artifact_trust.sql"),
    )?;
    sqlx::raw_sql(&forward_migration)
        .execute(&mut *transaction)
        .await?;
    let backfilled: String = sqlx::query_scalar(
        "SELECT artifact_trust_mode FROM connection_operations WHERE operation_id = $1",
    )
    .bind(source.record.operation_id)
    .fetch_one(&mut *transaction)
    .await?;
    assert_eq!(backfilled, "github-attestation");

    sqlx::query("DELETE FROM connection_operations WHERE operation_id = $1")
        .bind(source.record.operation_id)
        .execute(&mut *transaction)
        .await?;
    sqlx::query(&format!(
        "INSERT INTO connection_operations ({columns}) \
         SELECT {columns} FROM public.connection_operations WHERE operation_id = $1"
    ))
    .bind(source.record.operation_id)
    .execute(&mut *transaction)
    .await?;
    let old_writer_default: String = sqlx::query_scalar(
        "SELECT artifact_trust_mode FROM connection_operations WHERE operation_id = $1",
    )
    .bind(source.record.operation_id)
    .fetch_one(&mut *transaction)
    .await?;
    assert_eq!(old_writer_default, "github-attestation");
    assert!(
        sqlx::query(
            "UPDATE connection_operations SET artifact_trust_mode = 'unknown' \
             WHERE operation_id = $1",
        )
        .bind(source.record.operation_id)
        .execute(&mut *transaction)
        .await
        .is_err(),
        "the forward constraint must reject unknown persisted trust modes"
    );
    drop(transaction);
    Ok(())
}
