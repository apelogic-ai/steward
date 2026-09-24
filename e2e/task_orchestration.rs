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
use kube::api::PostParams;
use kube::core::Request as KubeRequest;
use kube::{Client, ResourceExt};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{postgres::PgPoolOptions, types::Uuid};
use steward_admission::internal_authorities::steward_connections_v1;
use steward_admission::{AdmissionDecision, Envelope, EnvelopeSpec};
use steward_apiserver::connections::{
    ConnectionSession, ConnectionSubject, ProviderConnectionBroker,
};
use steward_apiserver::governed_connections::{
    ConnectionExecutionBindings, GovernedConnectionsBroker, GovernedConnectionsConfig,
};
use steward_controller::{
    TaskControllerError, reconcile_task_orchestration_work_item, webhook_router_for_controller,
};
use steward_ports::{
    PortError, SandboxTaskObservation, SandboxTaskRequest, SandboxTaskRuntime, TaskAttemptId,
};
use steward_store::{
    EnvelopeRequestReservationRequest, EnvelopeRequestStatus, EnvelopeRequestStatusUpdate, PgStore,
    StoreError, TaskOrchestrationMode, TaskOrchestrationState, TaskReservationRequest,
    WorkflowPublication,
};
use steward_types::{
    AgentRuntime, AgentRuntimeSpec, AgentRuntimeStatus, AgentType, Budget,
    CanonicalAuthorityBinding, DisposableExecutionBinding, Duration, Email,
    ExecutionProviderProfiles, ExecutionVersionProbe, ModelRef, OrganizationId,
    OrganizationIdentityPolicy, Phase, Principal, RunnerRequirements, RuntimeOwnership,
    RuntimeRefs, TASK_EXECUTION_BINDING_SCHEMA_VERSION, TaskExecutionBinding, TaskPhase, ToolGrant,
};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

const RUNTIME_PATH_PREFIX: &str =
    "/apis/agents.apelogic.ai/v1alpha1/namespaces/steward-test/agentruntimes";
const CONTROLLER_USERNAME: &str = "system:serviceaccount:steward-system:steward-controller";

#[derive(Clone)]
struct WebhookAdmissionHarness {
    client: Client,
    verify_boundaries: Arc<AtomicBool>,
    create_checks: Arc<AtomicUsize>,
    update_checks: Arc<AtomicUsize>,
    delete_checks: Arc<AtomicUsize>,
}

#[derive(Clone, Default)]
struct AmbiguousKubernetes {
    runtime: Arc<Mutex<Option<AgentRuntime>>>,
    created: Arc<Mutex<Vec<AgentRuntime>>>,
    create_calls: Arc<AtomicUsize>,
    replace_calls: Arc<AtomicUsize>,
    fail_first_create_response: Arc<AtomicBool>,
    reject_create_with_unprocessable_entity: Arc<AtomicBool>,
    delete_preconditions: Arc<Mutex<Vec<String>>>,
    replace_name_after_delete: Arc<AtomicBool>,
    admission: Arc<Mutex<Option<WebhookAdmissionHarness>>>,
}

struct ServerGuard(JoinHandle<Result<(), io::Error>>);

impl Drop for ServerGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn disposable_execution_binding() -> Result<TaskExecutionBinding, io::Error> {
    let mut binding = DisposableExecutionBinding {
        schema_version: TASK_EXECUTION_BINDING_SCHEMA_VERSION.to_owned(),
        binding_id: format!("sha256:{}", "0".repeat(64)),
        binding_digest: format!("sha256:{}", "0".repeat(64)),
        agent_ref: "example-agent@1".to_owned(),
        display_name: None,
        adapter: "example-v1".to_owned(),
        image: format!(
            "registry.example.test/agents/example@sha256:{}",
            "a".repeat(64)
        ),
        executable: "/opt/example/bin/example-agent".to_owned(),
        version_probe: ExecutionVersionProbe {
            arguments: vec!["--version".to_owned()],
            expected_stdout: "example-agent 1".to_owned(),
        },
        provider_profiles: ExecutionProviderProfiles::default(),
    };
    let digest = format!(
        "sha256:{:x}",
        Sha256::digest(binding.canonical_content().map_err(io::Error::other)?)
    );
    binding.binding_id.clone_from(&digest);
    binding.binding_digest = digest;
    binding.validate().map_err(io::Error::other)?;
    Ok(TaskExecutionBinding::Disposable(binding))
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
async fn internal_authority_provisions_and_recovers_cleanup_without_a_service_envelope()
-> Result<(), Box<dyn Error>> {
    install_rustls_crypto_provider()?;
    let database_url = env::var("STEWARD_TEST_DATABASE_URL")?;
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool.clone());
    store.migrate().await?;
    for finalize_before_observation in [false, true] {
        let suffix = Uuid::new_v4().simple().to_string();
        let email = Email(format!("alice-{suffix}@example.com"));
        let identity = store
            .register_canonical_identity(
                &OrganizationIdentityPolicy::new(
                    "https://accounts.google.com",
                    "example.com",
                    OrganizationId::parse("org_example")?,
                )?
                .validate(
                    "https://accounts.google.com",
                    &suffix,
                    "example.com",
                    email.as_str(),
                    true,
                )?,
                "test-bootstrap",
            )
            .await?;
        let config = GovernedConnectionsConfig::new(
            ConnectionExecutionBindings {
                artifact_trust_mode: "github-attestation".to_owned(),
                bridge_image_digest: format!(
                    "ghcr.io/example-org/bridge@sha256:{}",
                    "a".repeat(64)
                ),
                mcp_gw_origin: "https://mcp-gw.example.test".to_owned(),
                mcp_gw_version: "0.3.2".to_owned(),
                namespace: "steward-test".to_owned(),
                runtime_class: "sandbox-vm".to_owned(),
            },
            "https://steward.example.test",
        )
        .map_err(|error| io::Error::other(format!("internal Task config: {error:?}")))?;
        let broker =
            GovernedConnectionsBroker::new(store.clone(), config, TaskOrchestrationMode::Active);
        let session = ConnectionSession {
            subject: ConnectionSubject {
                canonical_user_id: identity.user_id.clone(),
                display_email: email.as_str().to_owned(),
            },
            binding: (),
        };
        // Drive the actual broker through reservation, then drop its HTTP wait:
        // the controller must recover entirely from the committed intent.
        let reserved = tokio::select! {
            result = broker.status(&session) => {
                return Err(io::Error::other(format!("broker returned before reconciliation: {result:?}")).into());
            }
            result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if let Some(work) = store.task_orchestration_work_items().await?
                        .into_iter()
                        .find(|work| work.task.owner_user_id.as_deref() == Some(identity.user_id.as_str()))
                    {
                        return Ok::<_, StoreError>(work);
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }) => result??,
        };
        let task_uid = reserved.task.task_uid;
        assert_eq!(task_uid, reserved.operation.operation_id);
        assert_eq!(
            reserved.operation.inert_manifest_digest,
            manifest_digest(
                task_uid,
                reserved.operation.operation_id,
                &reserved.operation.runtime_name,
                &inert_spec(
                    &reserved.task.runtime_spec,
                    &steward_connections_v1::envelope()
                ),
                "inert",
            )?,
            "the broker must hash the identity actually persisted by the connection store"
        );
        let kubernetes = AmbiguousKubernetes::default();
        let (client, _server) = kubernetes_client(kubernetes.clone()).await?;
        let runtime = AmbiguousTaskRuntime::default();
        reconcile_current(&client, &runtime, &store, task_uid).await?;
        assert_eq!(
            operation(&store, task_uid).await?.state,
            TaskOrchestrationState::RuntimeCreatePending
        );
        if finalize_before_observation {
            store
                .request_task_finalization(
                    task_uid,
                    steward_connections_v1::SERVICE,
                    identity.user_id.as_str(),
                )
                .await?;
            reconcile_current(&client, &runtime, &store, task_uid).await?;
            assert_eq!(
                operation(&store, task_uid).await?.state,
                TaskOrchestrationState::CleanupPending
            );
        }

        // Corrupted internal authority must never fall back to another authority
        // or produce even an inert external object, including during recovery.
        for field in 0..8 {
            let mut work = current_work(&store, task_uid).await?;
            match field {
                0 => work.task.internal_authority_id = Some("unknown-authority".to_owned()),
                1 => work.task.internal_authority_version = Some(999),
                2 => {
                    work.task.internal_authority_digest = Some(format!("sha256:{}", "0".repeat(64)))
                }
                3 => work.task.service_envelope_digest = Some(format!("sha256:{}", "0".repeat(64))),
                4 => work.task.envelope_revision = Some(999),
                5 => work.task.submitter_service = "other-service".to_owned(),
                6 => work.task.internal_authority_version = None,
                _ => {
                    work.task.internal_authority_id = None;
                    work.task.internal_authority_version = None;
                    work.task.internal_authority_digest = None;
                }
            }
            assert!(
                reconcile_task_orchestration_work_item(&client, &runtime, &store, &work)
                    .await
                    .is_err()
            );
            assert_eq!(kubernetes.create_calls.load(Ordering::SeqCst), 0);
        }
        reconcile_current(&client, &runtime, &store, task_uid).await?;
        let observed = operation(&store, task_uid).await?;
        assert_eq!(observed.runtime_uid.as_deref(), Some("runtime-uid-a"));
        let created = kubernetes
            .created
            .lock()
            .map_err(|_| io::Error::other("fixture poisoned"))?
            .clone();
        assert_eq!(created.len(), 1);
        assert!(created[0].spec.llms.is_empty() && created[0].spec.tools.is_empty());
        if !finalize_before_observation {
            assert_eq!(observed.state, TaskOrchestrationState::RuntimeObserved);
            reconcile_current(&client, &runtime, &store, task_uid).await?;
            assert_eq!(
                operation(&store, task_uid).await?.state,
                TaskOrchestrationState::ActivationPending
            );
            store
                .request_task_finalization(
                    task_uid,
                    steward_connections_v1::SERVICE,
                    identity.user_id.as_str(),
                )
                .await?;
        }
        for _ in 0..4 {
            if operation(&store, task_uid).await?.state == TaskOrchestrationState::Finalized {
                break;
            }
            reconcile_current(&client, &runtime, &store, task_uid).await?;
        }
        assert_eq!(
            operation(&store, task_uid).await?.state,
            TaskOrchestrationState::Finalized
        );
        assert!(
            kubernetes
                .runtime
                .lock()
                .map_err(|_| io::Error::other("fixture poisoned"))?
                .is_none()
        );
        assert_eq!(
            *kubernetes
                .delete_preconditions
                .lock()
                .map_err(|_| io::Error::other("fixture poisoned"))?,
            vec!["runtime-uid-a".to_owned()]
        );
    }
    Ok(())
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
    let store = PgStore::new(pool.clone());
    store.migrate().await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let service = format!("orchestrator-fault-{suffix}");
    let member_role = format!("engineer-{suffix}");
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
        .insert_envelope(&member_role, &envelope, "admin@example.com")
        .await?;
    let envelope_request = store
        .reserve_envelope_request(EnvelopeRequestReservationRequest {
            owner_user_id: &identity.user_id,
            template_id: &member_role,
            template_revision: envelope.revision,
            requested_envelope: &envelope,
            idempotency_key: &format!("envelope-{suffix}"),
            actor: "admin@example.com",
        })
        .await?
        .record;
    let approval_id = Uuid::new_v4();
    store
        .append_envelope_request_status(
            envelope_request.id,
            EnvelopeRequestStatusUpdate {
                from: EnvelopeRequestStatus::Pending,
                to: EnvelopeRequestStatus::Approved,
                approval_id: Some(approval_id),
                envelope_instance_id: None,
                envelope_digest: None,
                reason: None,
                approved_envelope: Some(&envelope),
                actor: "admin@example.com",
            },
        )
        .await?;
    let envelope_instance_id = format!("env_{}", envelope_request.id.simple());
    let envelope_digest = format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&envelope)?)
    );
    let workflow_digest = format!(
        "sha256:{:x}",
        Sha256::digest(b"task-orchestration-workflow-v1")
    );
    store
        .publish_initial_workflow(WorkflowPublication {
            name: "fault-injection",
            display_name: "Fault injection",
            agent: "example-agent@1",
            prompt: "Exercise durable orchestration recovery.",
            content_digest: &workflow_digest,
            published_by: "admin@example.com",
        })
        .await?;
    store
        .append_envelope_request_status(
            envelope_request.id,
            EnvelopeRequestStatusUpdate {
                from: EnvelopeRequestStatus::Approved,
                to: EnvelopeRequestStatus::Provisioned,
                approval_id: Some(approval_id),
                envelope_instance_id: Some(&envelope_instance_id),
                envelope_digest: Some(&envelope_digest),
                reason: None,
                approved_envelope: Some(&envelope),
                actor: "steward-test",
            },
        )
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
            name: "example-agent@1".to_owned(),
        },
        llms: envelope.spec.llms.clone(),
        tools: envelope.spec.tools.clone(),
        budget: envelope.spec.budget.clone(),
        ttl: envelope.spec.ttl.clone(),
        runner: envelope.spec.runner.clone(),
        bindings: None,
    };
    let execution_binding = disposable_execution_binding()?;
    let task_uid = Uuid::new_v4();
    let operation_id = Uuid::new_v4();
    let runtime_name = format!("task-{}", operation_id.simple());
    let candidate_digest = digest(serde_json::to_value(&spec))?;
    let inert_digest = manifest_digest_with_binding(
        task_uid,
        operation_id,
        &runtime_name,
        &inert_spec(&spec, &envelope),
        "inert",
        Some(&execution_binding),
    )?;
    let active_digest = manifest_digest_with_binding(
        task_uid,
        operation_id,
        &runtime_name,
        &spec,
        "active",
        Some(&execution_binding),
    )?;
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
            workflow_name: Some("fault-injection"),
            workflow_version: Some(1),
            workflow_digest: Some(&workflow_digest),
            user_envelope_instance_id: Some(&envelope_instance_id),
            user_envelope_revision: Some(envelope.revision),
            user_envelope_digest: Some(&envelope_digest),
            coding_agent_runtime: "example-agent@1",
            runtime_uid: None,
            runtime_namespace: "steward-test",
            runtime_name: &runtime_name,
            runtime_ownership: RuntimeOwnership::Provisioned,
            runtime_spec: &spec,
            agent_command: &agent_command,
            execution_binding: Some(&execution_binding),
            direct_task_evidence: None,
            user_envelope_snapshot: Some(&envelope),
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
    let (admission_client, _admission_server) = router_client(
        webhook_router_for_controller(store.clone(), CONTROLLER_USERNAME.to_owned()),
        "steward-test",
    )
    .await?;
    let admission_boundaries_pending = Arc::new(AtomicBool::new(true));
    let admission_create_checks = Arc::new(AtomicUsize::new(0));
    let admission_update_checks = Arc::new(AtomicUsize::new(0));
    let admission_delete_checks = Arc::new(AtomicUsize::new(0));
    *kubernetes
        .admission
        .lock()
        .map_err(|_| io::Error::other("admission fixture was poisoned"))? =
        Some(WebhookAdmissionHarness {
            client: admission_client,
            verify_boundaries: admission_boundaries_pending.clone(),
            create_checks: admission_create_checks.clone(),
            update_checks: admission_update_checks.clone(),
            delete_checks: admission_delete_checks.clone(),
        });
    let (client, _server) = kubernetes_client(kubernetes.clone()).await?;
    let task_runtime = AmbiguousTaskRuntime::default();

    reconcile_current(&client, &task_runtime, &store, task_uid).await?;
    assert_eq!(
        operation(&store, task_uid).await?.state,
        TaskOrchestrationState::RuntimeCreatePending
    );

    let create_work = current_work(&store, task_uid).await?;
    let left = {
        let client = client.clone();
        let task_runtime = task_runtime.clone();
        let store = store.clone();
        let create_work = create_work.clone();
        tokio::spawn(async move {
            reconcile_task_orchestration_work_item(&client, &task_runtime, &store, &create_work)
                .await
        })
    };
    let right = {
        let client = client.clone();
        let task_runtime = task_runtime.clone();
        let store = store.clone();
        tokio::spawn(async move {
            reconcile_task_orchestration_work_item(&client, &task_runtime, &store, &create_work)
                .await
        })
    };
    let (left, right) = tokio::join!(left, right);
    let outcomes = [left?, right?];
    assert!(
        outcomes.iter().any(Result::is_ok),
        "one competing reconciler must complete the durable lifecycle step: {outcomes:?}"
    );
    for outcome in outcomes.into_iter().filter_map(Result::err) {
        assert!(
            matches!(
                outcome,
                TaskControllerError::Store(StoreError::Database(ref reason))
                    if reason.contains("deadlock detected")
            ),
            "only PostgreSQL's retryable deadlock arbitration may reject a competing reconcile: {outcome}"
        );
    }
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
        activated_runtime
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
    assert!(
        !admission_boundaries_pending.load(Ordering::SeqCst),
        "the production webhook must reject mutated and unpersisted CREATE requests"
    );
    assert!(
        admission_create_checks.load(Ordering::SeqCst) >= 1,
        "the controller-authored inert CREATE must cross the real webhook router"
    );
    assert!(
        admission_update_checks.load(Ordering::SeqCst) >= 1,
        "the inert-to-active UPDATE must cross the real webhook router"
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
    assert!(
        admission_delete_checks.load(Ordering::SeqCst) >= 1,
        "cleanup must cross the real webhook router with the exact persisted runtime UID"
    );
    *kubernetes
        .admission
        .lock()
        .map_err(|_| io::Error::other("admission fixture was poisoned"))? = None;

    *kubernetes
        .runtime
        .lock()
        .map_err(|_| io::Error::other("runtime fixture was poisoned"))? = None;
    let cleanup_task_uid = Uuid::new_v4();
    let cleanup_operation_id = Uuid::new_v4();
    let cleanup_runtime_name = format!("task-{}", cleanup_operation_id.simple());
    let cleanup_candidate_digest = digest(serde_json::to_value(&spec))?;
    let cleanup_inert_digest = manifest_digest_with_binding(
        cleanup_task_uid,
        cleanup_operation_id,
        &cleanup_runtime_name,
        &inert_spec(&spec, &envelope),
        "inert",
        Some(&execution_binding),
    )?;
    let cleanup_active_digest = manifest_digest_with_binding(
        cleanup_task_uid,
        cleanup_operation_id,
        &cleanup_runtime_name,
        &spec,
        "active",
        Some(&execution_binding),
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
            workflow: "fault-injection",
            workflow_name: Some("fault-injection"),
            workflow_version: Some(1),
            workflow_digest: Some(&workflow_digest),
            user_envelope_instance_id: Some(&envelope_instance_id),
            user_envelope_revision: Some(envelope.revision),
            user_envelope_digest: Some(&envelope_digest),
            coding_agent_runtime: "example-agent@1",
            runtime_uid: None,
            runtime_namespace: "steward-test",
            runtime_name: &cleanup_runtime_name,
            runtime_ownership: RuntimeOwnership::Provisioned,
            runtime_spec: &spec,
            agent_command: &agent_command,
            execution_binding: Some(&execution_binding),
            direct_task_evidence: None,
            user_envelope_snapshot: Some(&envelope),
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

    let rejected_task_uid = Uuid::new_v4();
    let rejected_operation_id = Uuid::new_v4();
    let rejected_runtime_name = format!("task-{}", rejected_operation_id.simple());
    let rejected_candidate_digest = digest(serde_json::to_value(&spec))?;
    let rejected_inert_digest = manifest_digest_with_binding(
        rejected_task_uid,
        rejected_operation_id,
        &rejected_runtime_name,
        &inert_spec(&spec, &envelope),
        "inert",
        Some(&execution_binding),
    )?;
    let rejected_active_digest = manifest_digest_with_binding(
        rejected_task_uid,
        rejected_operation_id,
        &rejected_runtime_name,
        &spec,
        "active",
        Some(&execution_binding),
    )?;
    store
        .reserve_task(&TaskReservationRequest {
            task_uid: rejected_task_uid,
            operation_id: rejected_operation_id,
            idempotency_key: &format!("deterministic-create-rejection-{suffix}"),
            submitter_service: &service,
            acting_user: Some("alice@example.com"),
            acting_user_id: Some(identity.user_id.as_str()),
            owner: "alice@example.com",
            owner_user_id: identity.user_id.as_str(),
            workflow: "fault-injection",
            workflow_name: Some("fault-injection"),
            workflow_version: Some(1),
            workflow_digest: Some(&workflow_digest),
            user_envelope_instance_id: Some(&envelope_instance_id),
            user_envelope_revision: Some(envelope.revision),
            user_envelope_digest: Some(&envelope_digest),
            coding_agent_runtime: "example-agent@1",
            runtime_uid: None,
            runtime_namespace: "steward-test",
            runtime_name: &rejected_runtime_name,
            runtime_ownership: RuntimeOwnership::Provisioned,
            runtime_spec: &spec,
            agent_command: &agent_command,
            execution_binding: Some(&execution_binding),
            direct_task_evidence: None,
            user_envelope_snapshot: Some(&envelope),
            candidate_digest: &rejected_candidate_digest,
            admission_decision: &admission_decision,
            inert_manifest_digest: &rejected_inert_digest,
            active_manifest_digest: &rejected_active_digest,
        })
        .await?;
    reconcile_current(&client, &task_runtime, &store, rejected_task_uid).await?;
    assert_eq!(
        operation(&store, rejected_task_uid).await?.state,
        TaskOrchestrationState::RuntimeCreatePending
    );
    kubernetes
        .reject_create_with_unprocessable_entity
        .store(true, Ordering::SeqCst);
    let create_calls_before_rejection = kubernetes.create_calls.load(Ordering::SeqCst);
    reconcile_current(&client, &task_runtime, &store, rejected_task_uid).await?;
    let rejected_operation = operation(&store, rejected_task_uid).await?;
    assert_eq!(
        rejected_operation.state,
        TaskOrchestrationState::CleanupPending,
        "a deterministic Kubernetes rejection must leave runtime-create-pending by entering cleanup"
    );
    assert_eq!(
        rejected_operation.last_error_code.as_deref(),
        Some("runtime_create_admission_rejected")
    );
    assert!(rejected_operation.runtime_absent_observed_at.is_some());
    let rejected_task = store
        .task(rejected_task_uid)
        .await?
        .ok_or(StoreError::TaskNotFound)?;
    assert_eq!(rejected_task.phase, TaskPhase::Failed);
    assert!(rejected_task.finalize_requested);

    reconcile_current(&client, &task_runtime, &store, rejected_task_uid).await?;
    assert_eq!(
        operation(&store, rejected_task_uid).await?.state,
        TaskOrchestrationState::Finalized
    );
    assert_eq!(
        kubernetes.create_calls.load(Ordering::SeqCst),
        create_calls_before_rejection + 1,
        "cleanup must not repeat a create after deterministic non-creation was proven"
    );
    assert!(
        store
            .task_runtime_admission("steward-test", "task-unpersisted")
            .await?
            .is_none(),
        "a missing Task runtime projection must remain absent"
    );

    let duplicate_task_uid = Uuid::new_v4();
    let duplicate_operation_id = Uuid::new_v4();
    let duplicate_inert_digest = manifest_digest_with_binding(
        duplicate_task_uid,
        duplicate_operation_id,
        &runtime_name,
        &inert_spec(&spec, &envelope),
        "inert",
        Some(&execution_binding),
    )?;
    let duplicate_active_digest = manifest_digest_with_binding(
        duplicate_task_uid,
        duplicate_operation_id,
        &runtime_name,
        &spec,
        "active",
        Some(&execution_binding),
    )?;
    insert_task_projection_fixture(
        &pool,
        TaskProjectionFixture {
            source_task_uid: task_uid,
            task_uid: duplicate_task_uid,
            operation_id: duplicate_operation_id,
            idempotency_key: &format!("duplicate-runtime-admission-{suffix}"),
            task_runtime_name: &runtime_name,
            operation_runtime_name: &runtime_name,
            inert_manifest_digest: &duplicate_inert_digest,
            active_manifest_digest: &duplicate_active_digest,
        },
    )
    .await?;
    assert!(
        matches!(
            store
                .task_runtime_admission("steward-test", &runtime_name)
                .await,
            Err(StoreError::InvalidTaskTransition)
        ),
        "duplicate runtime coordinates must fail the admission projection closed"
    );

    assert!(
        store
            .task_runtime_admission("steward-test", &rejected_runtime_name)
            .await?
            .is_some(),
        "the valid persisted projection must be readable before corruption"
    );
    let invalid_task_uid = Uuid::new_v4();
    let invalid_operation_id = Uuid::new_v4();
    let invalid_task_runtime_name = format!("task-{}", invalid_operation_id.simple());
    insert_task_projection_fixture(
        &pool,
        TaskProjectionFixture {
            source_task_uid: rejected_task_uid,
            task_uid: invalid_task_uid,
            operation_id: invalid_operation_id,
            idempotency_key: &format!("invalid-runtime-admission-{suffix}"),
            task_runtime_name: &invalid_task_runtime_name,
            operation_runtime_name: "task-corrupted-projection",
            inert_manifest_digest: &rejected_inert_digest,
            active_manifest_digest: &rejected_active_digest,
        },
    )
    .await?;
    assert!(
        matches!(
            store
                .task_runtime_admission("steward-test", "task-corrupted-projection")
                .await,
            Err(StoreError::InvalidTaskTransition)
        ),
        "an inconsistent persisted Task/runtime projection must fail closed"
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

struct TaskProjectionFixture<'a> {
    source_task_uid: Uuid,
    task_uid: Uuid,
    operation_id: Uuid,
    idempotency_key: &'a str,
    task_runtime_name: &'a str,
    operation_runtime_name: &'a str,
    inert_manifest_digest: &'a str,
    active_manifest_digest: &'a str,
}

async fn insert_task_projection_fixture(
    pool: &sqlx::PgPool,
    fixture: TaskProjectionFixture<'_>,
) -> Result<(), Box<dyn Error>> {
    let mut transaction = pool.begin().await?;
    let inserted = sqlx::query(
        "INSERT INTO task_submissions \
         (task_uid, idempotency_key, submitter_service, acting_user, acting_user_id, \
          owner, owner_user_id, identity_binding_state, workflow, workflow_name, workflow_version, \
          workflow_digest, user_envelope_instance_id, user_envelope_revision, \
          user_envelope_digest, authority_kind, user_envelope_snapshot, coding_agent_runtime, \
          runtime_uid, runtime_namespace, runtime_name, runtime_ownership, phase, runtime_spec, \
          agent_command, execution_binding, direct_task_evidence, envelope_revision, \
          orchestration_version, orchestration_operation_id, candidate_digest, \
          service_envelope_digest, original_admission_decision, original_admission_deltas) \
         SELECT $1, $2, submitter_service, acting_user, acting_user_id, owner, owner_user_id, \
                identity_binding_state, workflow, workflow_name, workflow_version, workflow_digest, \
                user_envelope_instance_id, user_envelope_revision, user_envelope_digest, \
                authority_kind, user_envelope_snapshot, coding_agent_runtime, NULL, \
                runtime_namespace, $3, runtime_ownership, 'submitted', runtime_spec, agent_command, \
                execution_binding, direct_task_evidence, envelope_revision, orchestration_version, \
                $4, candidate_digest, service_envelope_digest, original_admission_decision, \
                original_admission_deltas \
         FROM task_submissions WHERE task_uid = $5",
    )
    .bind(fixture.task_uid)
    .bind(fixture.idempotency_key)
    .bind(fixture.task_runtime_name)
    .bind(fixture.operation_id)
    .bind(fixture.source_task_uid)
    .execute(&mut *transaction)
    .await?
    .rows_affected();
    if inserted != 1 {
        return Err(io::Error::other("projection fixture source Task is absent").into());
    }
    sqlx::query(
        "INSERT INTO task_runtime_operations \
         (task_uid, operation_id, state, generation, runtime_ownership, runtime_namespace, \
          runtime_name, inert_manifest_digest, active_manifest_digest) \
         VALUES ($1, $2, 'intent_recorded', 1, 'provisioned', 'steward-test', $3, $4, $5)",
    )
    .bind(fixture.task_uid)
    .bind(fixture.operation_id)
    .bind(fixture.operation_runtime_name)
    .bind(fixture.inert_manifest_digest)
    .bind(fixture.active_manifest_digest)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(())
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

async fn router_client(
    router: Router,
    default_namespace: &str,
) -> Result<(Client, ServerGuard), Box<dyn Error>> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .map_err(io::Error::other)
    });
    let mut config = kube::Config::new(format!("http://{address}").parse()?);
    config.default_namespace = default_namespace.to_owned();
    Ok((Client::try_from(config)?, ServerGuard(server)))
}

async fn webhook_admits_runtime(
    harness: &WebhookAdmissionHarness,
    operation: &str,
    runtime: &AgentRuntime,
    old_runtime: Option<&AgentRuntime>,
) -> Result<bool, Box<dyn Error>> {
    let name = runtime
        .metadata
        .name
        .as_deref()
        .ok_or_else(|| io::Error::other("admission runtime name is absent"))?;
    let namespace = runtime
        .metadata
        .namespace
        .as_deref()
        .ok_or_else(|| io::Error::other("admission runtime namespace is absent"))?;
    let object = if operation == "DELETE" {
        serde_json::Value::Null
    } else {
        serde_json::to_value(runtime)?
    };
    let old_object = if operation == "DELETE" {
        serde_json::to_value(runtime)?
    } else {
        serde_json::to_value(old_runtime)?
    };
    let review = json!({
        "apiVersion": "admission.k8s.io/v1",
        "kind": "AdmissionReview",
        "request": {
            "uid": format!("{operation}-{name}"),
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
            "name": name,
            "namespace": namespace,
            "operation": operation,
            "userInfo": {
                "username": CONTROLLER_USERNAME,
                "groups": ["system:serviceaccounts"]
            },
            "object": object,
            "oldObject": old_object,
            "dryRun": false,
            "options": null
        }
    });
    let request = KubeRequest::new("/validate-agent-runtime")
        .create(&PostParams::default(), serde_json::to_vec(&review)?)?;
    let response: serde_json::Value =
        serde_json::from_str(&harness.client.request_text(request).await?)?;
    response
        .pointer("/response/allowed")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| io::Error::other("webhook response has no allowed decision").into())
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
            if state
                .reject_create_with_unprocessable_entity
                .swap(false, Ordering::SeqCst)
            {
                return Ok(status_response(StatusCode::UNPROCESSABLE_ENTITY, "Invalid"));
            }
            let bytes = match to_bytes(request.into_body(), 1024 * 1024).await {
                Ok(bytes) => bytes,
                Err(_) => return Ok(status_response(StatusCode::BAD_REQUEST, "BadRequest")),
            };
            let mut runtime = match serde_json::from_slice::<AgentRuntime>(&bytes) {
                Ok(runtime) => runtime,
                Err(_) => return Ok(status_response(StatusCode::BAD_REQUEST, "BadRequest")),
            };
            let admission = match state.admission.lock() {
                Ok(admission) => admission.clone(),
                Err(_) => {
                    return Ok(status_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "InternalError",
                    ));
                }
            };
            if let Some(admission) = admission {
                if admission.verify_boundaries.swap(false, Ordering::SeqCst) {
                    let mut mutated = runtime.clone();
                    mutated.spec.budget.monthly_limit = "999".to_owned();
                    if !matches!(
                        webhook_admits_runtime(&admission, "CREATE", &mutated, None).await,
                        Ok(false)
                    ) {
                        return Ok(status_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "MutatedRuntimeWasNotDenied",
                        ));
                    }
                    let mut unpersisted = runtime.clone();
                    unpersisted.metadata.name = Some("task-unpersisted".to_owned());
                    if !matches!(
                        webhook_admits_runtime(&admission, "CREATE", &unpersisted, None).await,
                        Ok(false)
                    ) {
                        return Ok(status_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "UnpersistedRuntimeWasNotDenied",
                        ));
                    }
                }
                admission.create_checks.fetch_add(1, Ordering::SeqCst);
                if !matches!(
                    webhook_admits_runtime(&admission, "CREATE", &runtime, None).await,
                    Ok(true)
                ) {
                    return Ok(status_response(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "AdmissionDenied",
                    ));
                }
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
            let current = match state.runtime.lock() {
                Ok(stored) => stored.clone(),
                Err(_) => {
                    return Ok(status_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "InternalError",
                    ));
                }
            };
            let Some(current) = current else {
                return Ok(status_response(StatusCode::NOT_FOUND, "NotFound"));
            };
            if desired.metadata.resource_version != current.metadata.resource_version {
                status_response(StatusCode::CONFLICT, "Conflict")
            } else {
                let admission = match state.admission.lock() {
                    Ok(admission) => admission.clone(),
                    Err(_) => {
                        return Ok(status_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "InternalError",
                        ));
                    }
                };
                if let Some(admission) = admission {
                    admission.update_checks.fetch_add(1, Ordering::SeqCst);
                    if !matches!(
                        webhook_admits_runtime(&admission, "UPDATE", &desired, Some(&current),)
                            .await,
                        Ok(true)
                    ) {
                        return Ok(status_response(
                            StatusCode::UNPROCESSABLE_ENTITY,
                            "AdmissionDenied",
                        ));
                    }
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
                    .and_then(|runtime| runtime.metadata.resource_version.as_deref())
                    != current.metadata.resource_version.as_deref()
                {
                    return Ok(status_response(StatusCode::CONFLICT, "Conflict"));
                }
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
            let current = match state.runtime.lock() {
                Ok(stored) => stored.clone(),
                Err(_) => {
                    return Ok(status_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "InternalError",
                    ));
                }
            };
            let admission = match state.admission.lock() {
                Ok(admission) => admission.clone(),
                Err(_) => {
                    return Ok(status_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "InternalError",
                    ));
                }
            };
            if let (Some(admission), Some(current)) = (admission, current.as_ref()) {
                admission.delete_checks.fetch_add(1, Ordering::SeqCst);
                if !matches!(
                    webhook_admits_runtime(&admission, "DELETE", current, Some(current)).await,
                    Ok(true)
                ) {
                    return Ok(status_response(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "AdmissionDenied",
                    ));
                }
            }
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
    manifest_digest_with_binding(task_uid, operation_id, runtime_name, spec, mode, None)
}

fn manifest_digest_with_binding(
    task_uid: Uuid,
    operation_id: Uuid,
    runtime_name: &str,
    spec: &AgentRuntimeSpec,
    mode: &str,
    execution_binding: Option<&TaskExecutionBinding>,
) -> Result<String, serde_json::Error> {
    digest(Ok(json!({
        "schemaVersion": "steward-task-runtime-manifest/v1",
        "taskUid": task_uid,
        "operationId": operation_id,
        "runtimeNamespace": "steward-test",
        "runtimeName": runtime_name,
        "mode": mode,
        "spec": spec,
        "executionBinding": execution_binding,
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
