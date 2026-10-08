use std::collections::BTreeMap;
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
use axum::response::IntoResponse;
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
    ConnectionBrokerError, ConnectionSession, ConnectionStartOperation, ConnectionSubject,
    ProviderConnectionBroker,
};
use steward_apiserver::governed_connections::{
    ConnectionExecutionBindings, ConnectionOperationReconciler, GovernedConnectionsBroker,
    GovernedConnectionsConfig,
};
use steward_apiserver::{
    AuthenticatedCaller, AuthenticationError, BoxFuture, RequestAuthenticator, operator_admin,
};
use steward_controller::{
    TaskControllerError, reconcile_agent_runtime_work_item, reconcile_task_orchestration_work_item,
    webhook_router_for_controller,
};
use steward_ports::{
    InferenceCapabilities, InferenceCredential, InferenceObservation, InferencePlane,
    InferenceRequest, PortError, ProviderControlExecutionBindings, ProvisionedInference,
    SandboxObservation, SandboxRequest, SandboxRuntime, SandboxTaskObservation, SandboxTaskOutput,
    SandboxTaskRequest, SandboxTaskRuntime, SandboxTaskTranscript, TaskAttemptId,
};
use steward_store::{
    AgentRunLogStream, EnvelopeRequestReservationRequest, EnvelopeRequestStatus,
    EnvelopeRequestStatusUpdate, EnvelopeTemplatePublication, MAX_ACTIVE_BROWSER_TASKS_PER_USER,
    PgStore, StoreError, TaskActivationObservation, TaskExecutionObservation,
    TaskExecutionTransition, TaskOrchestrationMode, TaskOrchestrationState, TaskReservationRequest,
    WorkflowPublication,
};
use steward_types::direct_package::{
    BrowserTaskEvidence, ClosureEntry, ClosureEntryKind, ContentDigest, DiagnosticsRequest,
    ExecutionLogMode, PackageClosure, PromptSourceKind, RelativePath, TaskOrigin,
    canonical_json_bytes,
};
use steward_types::task_output_archive::{
    TASK_OUTPUT_ARCHIVE_CONTRACT, TaskOutputArchiveCompatibility, task_output_archive_entries,
    task_output_archive_with_execution_transcript,
};
use steward_types::{
    AgentRuntime, AgentRuntimeSpec, AgentRuntimeStatus, AgentType, Budget,
    CanonicalAuthorityBinding, DisposableExecutionBinding, Duration, Email,
    ExecutionProviderProfiles, ExecutionVersionProbe, ModelRef, OrganizationId,
    OrganizationIdentityPolicy, Phase, Principal, RunnerRequirements, RuntimeOwnership,
    RuntimeRefs, SpendSummary, TASK_EXECUTION_BINDING_SCHEMA_VERSION, TaskExecutionBinding,
    TaskPhase, ToolGrant,
};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tower::ServiceExt;

const RUNTIME_PATH_PREFIX: &str =
    "/apis/agents.apelogic.ai/v1alpha1/namespaces/steward-test/agentruntimes";
const SECRET_PATH_PREFIX: &str = "/api/v1/namespaces/steward-test/secrets";
const CONTROLLER_USERNAME: &str = "system:serviceaccount:steward-system:steward-controller";
const OPERATOR_USERNAME: &str = "system:serviceaccount:steward-test:operator";

#[derive(Clone)]
struct OperatorTestAuthenticator;

impl RequestAuthenticator for OperatorTestAuthenticator {
    fn authenticate<'a>(
        &'a self,
        bearer_token: &'a str,
    ) -> BoxFuture<'a, Result<AuthenticatedCaller, AuthenticationError>> {
        Box::pin(async move {
            if bearer_token != "operator-token" {
                return Err(AuthenticationError::InvalidCredentials);
            }
            Ok(AuthenticatedCaller {
                actor: OPERATOR_USERNAME.to_owned(),
                member_roles: Vec::new(),
                canonical_user_id: None,
                is_admin: true,
            })
        })
    }
}

async fn operator_api_request(
    app: &Router,
    method: Method,
    uri: &str,
    body: Option<serde_json::Value>,
) -> Result<Response<Body>, Box<dyn Error>> {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, "Bearer operator-token");
    let body = match body {
        Some(body) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(body.to_string())
        }
        None => Body::empty(),
    };
    Ok(app.clone().oneshot(request.body(body)?).await?)
}

async fn response_json(response: Response<Body>) -> Result<serde_json::Value, Box<dyn Error>> {
    let body = to_bytes(response.into_body(), 1024 * 1024).await?;
    Ok(serde_json::from_slice(&body)?)
}

fn task_output_archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut archive = Vec::new();
    for (path, content) in entries {
        let header_offset = archive.len();
        archive.resize(header_offset + 512, 0);
        archive[header_offset..header_offset + path.len()].copy_from_slice(path.as_bytes());
        archive[header_offset + 100..header_offset + 108].copy_from_slice(b"0000644\0");
        archive[header_offset + 108..header_offset + 116].copy_from_slice(b"0000000\0");
        archive[header_offset + 116..header_offset + 124].copy_from_slice(b"0000000\0");
        let size = format!("{:011o}\0", content.len());
        archive[header_offset + 124..header_offset + 136].copy_from_slice(size.as_bytes());
        archive[header_offset + 136..header_offset + 148].copy_from_slice(b"00000000000\0");
        archive[header_offset + 148..header_offset + 156].fill(b' ');
        archive[header_offset + 156] = b'0';
        archive[header_offset + 257..header_offset + 263].copy_from_slice(b"ustar\0");
        archive[header_offset + 263..header_offset + 265].copy_from_slice(b"00");
        let checksum = archive[header_offset..header_offset + 512]
            .iter()
            .map(|byte| usize::from(*byte))
            .sum::<usize>();
        let checksum = format!("{:06o}\0 ", checksum);
        archive[header_offset + 148..header_offset + 156].copy_from_slice(checksum.as_bytes());
        archive.extend_from_slice(content);
        archive.resize(header_offset + 512 + content.len().div_ceil(512) * 512, 0);
    }
    archive.resize(archive.len() + 1024, 0);
    archive
}

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
    replace_uid_after_update: Arc<AtomicBool>,
    replace_name_after_delete: Arc<AtomicBool>,
    admission: Arc<Mutex<Option<WebhookAdmissionHarness>>>,
    credential_secret: Arc<Mutex<Option<serde_json::Value>>>,
    status_patches: Arc<AtomicUsize>,
    next_runtime_uid: Arc<Mutex<Option<String>>>,
}

struct ServerGuard(JoinHandle<Result<(), io::Error>>);

impl Drop for ServerGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[tokio::test]
async fn concurrent_provisioning_enforces_active_digest_uniqueness_transactionally()
-> Result<(), Box<dyn Error>> {
    install_rustls_crypto_provider()?;
    let database_url = env::var("STEWARD_TEST_DATABASE_URL")?;
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let suffix = Uuid::new_v4().simple().to_string();
    let identity = store
        .register_canonical_identity(
            &OrganizationIdentityPolicy::new(
                "https://accounts.google.com",
                "example.com",
                OrganizationId::parse("org_example")?,
            )?
            .validate(
                "https://accounts.google.com",
                &format!("concurrent-envelope-{suffix}"),
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
            tools: Vec::new(),
            budget: Budget {
                monthly_limit: "1.00".to_owned(),
                single_run_limit: Some("0.50".to_owned()),
                currency: "USD".to_owned(),
            },
            runtime_minutes_limit: Some("10".to_owned()),
            ttl: Duration("1h".to_owned()),
            runner: RunnerRequirements::default(),
        },
    };
    let template_a = format!("template-a-{suffix}");
    let template_b = format!("template-b-{suffix}");
    for template_id in [&template_a, &template_b] {
        store
            .insert_envelope_template_revision(EnvelopeTemplatePublication {
                template_id,
                display_name: template_id,
                member_roles: std::slice::from_ref(template_id),
                ceiling: &envelope,
                auto_provision_threshold: Some(&envelope),
                allow_inline_browser_tasks: true,
                authored_by: "test-bootstrap",
            })
            .await?;
    }
    let request_a = store
        .reserve_envelope_request(EnvelopeRequestReservationRequest {
            owner_user_id: &identity.user_id,
            template_id: Some(&template_a),
            template_revision: Some(1),
            requested_envelope: &envelope,
            idempotency_key: &format!("request-a-{suffix}"),
            actor: "test-bootstrap",
        })
        .await?
        .record;
    let request_b = store
        .reserve_envelope_request(EnvelopeRequestReservationRequest {
            owner_user_id: &identity.user_id,
            template_id: Some(&template_b),
            template_revision: Some(1),
            requested_envelope: &envelope,
            idempotency_key: &format!("request-b-{suffix}"),
            actor: "test-bootstrap",
        })
        .await?
        .record;
    let digest = format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&envelope)?)
    );
    let instance_a = format!("env_{}", request_a.id.simple());
    let instance_b = format!("env_{}", request_b.id.simple());
    let update_a = EnvelopeRequestStatusUpdate {
        from: EnvelopeRequestStatus::Pending,
        to: EnvelopeRequestStatus::Provisioned,
        approval_id: None,
        envelope_instance_id: Some(&instance_a),
        envelope_digest: Some(&digest),
        reason: None,
        rationale: Some("concurrency test"),
        evidence_url: None,
        expires_at: None,
        approved_envelope: Some(&envelope),
        actor: "test-bootstrap",
    };
    let update_b = EnvelopeRequestStatusUpdate {
        from: EnvelopeRequestStatus::Pending,
        to: EnvelopeRequestStatus::Provisioned,
        approval_id: None,
        envelope_instance_id: Some(&instance_b),
        envelope_digest: Some(&digest),
        reason: None,
        rationale: Some("concurrency test"),
        evidence_url: None,
        expires_at: None,
        approved_envelope: Some(&envelope),
        actor: "test-bootstrap",
    };
    let (result_a, result_b) = tokio::join!(
        store.append_envelope_request_status(request_a.id, update_a),
        store.append_envelope_request_status(request_b.id, update_b),
    );
    assert!(
        matches!(
            (&result_a, &result_b),
            (Ok(_), Err(StoreError::EnvelopeRequestDigestConflict))
                | (Err(StoreError::EnvelopeRequestDigestConflict), Ok(_))
        ),
        "exactly one conflicting template may win concurrent digest provisioning: {result_a:?} / {result_b:?}"
    );
    let active = store
        .envelope_requests(&identity.user_id)
        .await?
        .into_iter()
        .filter(|request| request.status == EnvelopeRequestStatus::Provisioned)
        .count();
    assert_eq!(active, 1);
    Ok(())
}

#[tokio::test]
async fn operator_rbac_and_provisioning_preserve_distinct_active_authority()
-> Result<(), Box<dyn Error>> {
    install_rustls_crypto_provider()?;
    let database_url = env::var("STEWARD_TEST_DATABASE_URL")?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool.clone());
    store.migrate().await?;
    let suffix = Uuid::new_v4().simple().to_string();
    let role = format!("engineer-{suffix}");
    let identity = store
        .register_canonical_identity(
            &OrganizationIdentityPolicy::new(
                "https://accounts.google.com",
                "example.com",
                OrganizationId::parse("org_example")?,
            )?
            .validate(
                "https://accounts.google.com",
                &format!("operator-provision-{suffix}"),
                "example.com",
                &format!("alice-{suffix}@example.com"),
                true,
            )?,
            "test-bootstrap",
        )
        .await?;
    let bob_identity = store
        .register_canonical_identity(
            &OrganizationIdentityPolicy::new(
                "https://accounts.google.com",
                "example.org",
                OrganizationId::parse("org_example")?,
            )?
            .validate(
                "https://accounts.google.com",
                &format!("operator-provision-bob-{suffix}"),
                "example.org",
                &format!("bob-{suffix}@example.org"),
                true,
            )?,
            "test-bootstrap",
        )
        .await?;
    let envelope = |revision, monthly_limit: &str| Envelope {
        revision,
        spec: EnvelopeSpec {
            llms: vec![ModelRef {
                provider: "example".to_owned(),
                model: "model-a".to_owned(),
            }],
            tools: Vec::new(),
            budget: Budget {
                monthly_limit: monthly_limit.to_owned(),
                single_run_limit: Some("0.50".to_owned()),
                currency: "USD".to_owned(),
            },
            runtime_minutes_limit: Some("10".to_owned()),
            ttl: Duration("1h".to_owned()),
            runner: RunnerRequirements::default(),
        },
    };
    let template_a = format!("develop-{suffix}");
    let template_b = format!("review-{suffix}");
    let envelope_a1 = envelope(1, "1.00");
    let envelope_b1 = envelope(1, "2.00");
    for (template_id, ceiling) in [(&template_a, &envelope_a1), (&template_b, &envelope_b1)] {
        store
            .insert_envelope_template_revision(EnvelopeTemplatePublication {
                template_id,
                display_name: template_id,
                member_roles: std::slice::from_ref(&role),
                ceiling,
                auto_provision_threshold: Some(ceiling),
                allow_inline_browser_tasks: true,
                authored_by: "test-bootstrap",
            })
            .await?;
    }
    let operator_app = operator_admin::router(store.clone(), OperatorTestAuthenticator);
    let users =
        operator_api_request(&operator_app, Method::GET, "/admin/operator/v1/users", None).await?;
    assert_eq!(users.status(), StatusCode::OK);
    let users = response_json(users).await?;
    assert!(users["users"].as_array().is_some_and(|users| {
        users
            .iter()
            .any(|user| user["userId"] == identity.user_id.as_str())
    }));
    let roles =
        operator_api_request(&operator_app, Method::GET, "/admin/operator/v1/roles", None).await?;
    assert_eq!(roles.status(), StatusCode::OK);
    assert!(
        response_json(roles).await?["memberRoles"]
            .as_array()
            .is_some_and(|roles| roles.iter().any(|candidate| candidate == &role))
    );

    let admin_grant = operator_api_request(
        &operator_app,
        Method::POST,
        "/admin/operator/v1/rbac",
        Some(json!({
            "userId": identity.user_id.as_str(),
            "kind": "administrator",
            "action": "grant",
        })),
    )
    .await?;
    assert_eq!(admin_grant.status(), StatusCode::OK);
    let pure_admin = operator_api_request(
        &operator_app,
        Method::POST,
        "/admin/operator/v1/envelopes/provision",
        Some(json!({
            "ownerUserId": identity.user_id.as_str(),
            "templateId": template_a,
            "templateRevision": 1,
            "requestedEnvelope": envelope_a1,
            "idempotencyKey": format!("pure-admin-{suffix}"),
        })),
    )
    .await?;
    assert_eq!(pure_admin.status(), StatusCode::UNPROCESSABLE_ENTITY);

    for action in ["grant", "grant", "revoke", "revoke", "grant"] {
        let response = operator_api_request(
            &operator_app,
            Method::POST,
            "/admin/operator/v1/rbac",
            Some(json!({
                "userId": identity.user_id.as_str(),
                "kind": "member_role",
                "memberRole": role,
                "action": action,
            })),
        )
        .await?;
        assert_eq!(response.status(), StatusCode::OK);
    }
    let bob_role_grant = operator_api_request(
        &operator_app,
        Method::POST,
        "/admin/operator/v1/rbac",
        Some(json!({
            "userId": bob_identity.user_id.as_str(),
            "kind": "member_role",
            "memberRole": role.as_str(),
            "action": "grant",
        })),
    )
    .await?;
    assert_eq!(bob_role_grant.status(), StatusCode::OK);
    let role_events = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM browser_rbac_assignment_events \
         WHERE user_id = $1 AND assignment_kind = 'member_role' AND member_role = $2",
    )
    .bind(identity.user_id.as_str())
    .bind(&role)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        role_events, 3,
        "exact grant and revoke retries must not duplicate audit events"
    );
    let effective = operator_api_request(
        &operator_app,
        Method::GET,
        &format!(
            "/admin/operator/v1/users/{}/effective-access",
            identity.user_id.as_str()
        ),
        None,
    )
    .await?;
    assert_eq!(effective.status(), StatusCode::OK);
    let effective = response_json(effective).await?;
    assert_eq!(effective["administrator"], true);
    assert_eq!(effective["memberRoles"], json!([role.as_str()]));
    assert_eq!(
        effective["eligibleTemplates"].as_array().map(Vec::len),
        Some(2)
    );

    for (template_id, requested_envelope, idempotency_key) in [
        (&template_a, &envelope_a1, format!("provision-a1-{suffix}")),
        (&template_b, &envelope_b1, format!("provision-b1-{suffix}")),
    ] {
        let response = operator_api_request(
            &operator_app,
            Method::POST,
            "/admin/operator/v1/envelopes/provision",
            Some(json!({
                "ownerUserId": identity.user_id.as_str(),
                "templateId": template_id,
                "templateRevision": 1,
                "requestedEnvelope": requested_envelope,
                "idempotencyKey": idempotency_key,
            })),
        )
        .await?;
        assert_eq!(response.status(), StatusCode::OK);
    }
    let initial_requests = store.envelope_requests(&identity.user_id).await?;
    let a1 = initial_requests
        .iter()
        .find(|request| {
            request.template_id.as_deref() == Some(template_a.as_str())
                && request.status == EnvelopeRequestStatus::Provisioned
        })
        .cloned()
        .ok_or("operator API did not provision template A")?;
    let b1 = initial_requests
        .iter()
        .find(|request| {
            request.template_id.as_deref() == Some(template_b.as_str())
                && request.status == EnvelopeRequestStatus::Provisioned
        })
        .cloned()
        .ok_or("operator API did not provision template B")?;
    assert_eq!(a1.owner_user_id, identity.user_id);
    assert_eq!(a1.status_actor, OPERATOR_USERNAME);
    assert_eq!(b1.status_actor, OPERATOR_USERNAME);

    let envelope_a2 = envelope(2, "3.00");
    store
        .insert_envelope_template_revision(EnvelopeTemplatePublication {
            template_id: &template_a,
            display_name: &template_a,
            member_roles: std::slice::from_ref(&role),
            ceiling: &envelope_a2,
            auto_provision_threshold: Some(&envelope_a2),
            allow_inline_browser_tasks: true,
            authored_by: OPERATOR_USERNAME,
        })
        .await?;
    let provision_a2 = |idempotency_key: String| {
        json!({
            "ownerUserId": identity.user_id.as_str(),
            "templateId": template_a,
            "templateRevision": 2,
            "requestedEnvelope": envelope_a2,
            "idempotencyKey": idempotency_key,
        })
    };
    let a2_response = operator_api_request(
        &operator_app,
        Method::POST,
        "/admin/operator/v1/envelopes/provision",
        Some(provision_a2(format!("provision-a2-{suffix}"))),
    )
    .await?;
    assert_eq!(a2_response.status(), StatusCode::OK);
    let a2_response = response_json(a2_response).await?;
    let a2_retry = operator_api_request(
        &operator_app,
        Method::POST,
        "/admin/operator/v1/envelopes/provision",
        Some(provision_a2(format!("provision-a2-retry-{suffix}"))),
    )
    .await?;
    assert_eq!(a2_retry.status(), StatusCode::OK);
    assert_eq!(response_json(a2_retry).await?, a2_response);

    let bob_a2_response = operator_api_request(
        &operator_app,
        Method::POST,
        "/admin/operator/v1/envelopes/provision",
        Some(json!({
            "ownerUserId": bob_identity.user_id.as_str(),
            "templateId": template_a.as_str(),
            "templateRevision": 2,
            "requestedEnvelope": &envelope_a2,
            "idempotencyKey": format!("provision-bob-a2-{suffix}"),
        })),
    )
    .await?;
    assert_eq!(bob_a2_response.status(), StatusCode::OK);

    let requests = store.envelope_requests(&identity.user_id).await?;
    let a2 = requests
        .iter()
        .find(|request| {
            request.template_id.as_deref() == Some(template_a.as_str())
                && request.template_revision == Some(2)
                && request.status == EnvelopeRequestStatus::Provisioned
        })
        .cloned()
        .ok_or("operator API did not provision template A revision 2")?;
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.status == EnvelopeRequestStatus::Provisioned)
            .map(|request| request.id)
            .collect::<std::collections::BTreeSet<_>>(),
        [a2.id, b1.id].into_iter().collect(),
        "replacing one template must preserve unrelated active authority"
    );
    assert_eq!(
        requests
            .iter()
            .find(|request| request.id == a1.id)
            .map(|request| request.status),
        Some(EnvelopeRequestStatus::Stale)
    );
    let active = store
        .active_provisioned_user_envelopes(&identity.user_id)
        .await?;
    assert_eq!(active.len(), 2);
    let a2_digest = a2
        .envelope_digest
        .as_deref()
        .ok_or("provisioned Envelope omitted its digest")?;
    let selected = store
        .active_provisioned_user_envelopes_by_digest(&identity.user_id, a2_digest)
        .await?;
    assert_eq!(
        selected.iter().map(|record| record.id).collect::<Vec<_>>(),
        [a2.id]
    );
    let bob_requests = store.envelope_requests(&bob_identity.user_id).await?;
    let bob_a2 = bob_requests
        .iter()
        .find(|request| request.status == EnvelopeRequestStatus::Provisioned)
        .ok_or("operator API did not provision Bob's template A revision 2")?;
    assert_eq!(bob_a2.envelope_digest.as_deref(), Some(a2_digest));
    let bob_selected = store
        .active_provisioned_user_envelopes_by_digest(&bob_identity.user_id, a2_digest)
        .await?;
    assert_eq!(
        bob_selected
            .iter()
            .map(|record| record.id)
            .collect::<Vec<_>>(),
        [bob_a2.id],
        "the same digest must resolve independently for each owner"
    );
    let b1_digest = b1
        .envelope_digest
        .as_deref()
        .ok_or("provisioned template B omitted its digest")?;
    assert!(
        store
            .active_provisioned_user_envelopes_by_digest(&bob_identity.user_id, b1_digest)
            .await?
            .is_empty(),
        "an owner must not select another user's active Envelope by digest"
    );
    Ok(())
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

fn browser_direct_package_evidence(
    source: &str,
    repository_revision: Option<String>,
) -> Result<BrowserTaskEvidence, Box<dyn Error>> {
    let path = RelativePath::parse("task-definition.json")?;
    let closure = PackageClosure {
        contract_version: "steward.package-closure/v1".to_owned(),
        entry_point: path.clone(),
        entries: vec![ClosureEntry {
            kind: ClosureEntryKind::TaskDefinition,
            path: path.clone(),
            digest: ContentDigest::parse(format!("steward:sha256:{:x}", Sha256::digest(b"{}")))?,
            size_bytes: 2,
        }],
    };
    closure.validate()?;
    let closure_digest = ContentDigest::parse(format!(
        "steward:sha256:{:x}",
        Sha256::digest(canonical_json_bytes(&closure)?)
    ))?;
    let evidence = BrowserTaskEvidence {
        source: source.to_owned(),
        revision: repository_revision.unwrap_or_else(|| closure_digest.as_str().to_owned()),
        path,
        closure: Some(closure),
        closure_digest,
        inline_files: (source == "inline")
            .then(|| BTreeMap::from([("task-definition.json".to_owned(), "{}".to_owned())])),
        diagnostics: Default::default(),
        prompt_source: PromptSourceKind::Inline,
    };
    evidence.validate()?;
    Ok(evidence)
}

async fn reserve_browser_concurrency_fixture(
    store: &PgStore,
    idempotency_key: &str,
    service: &str,
    identity: &steward_types::CanonicalPrincipal,
    envelope_instance_id: &str,
    envelope_digest: &str,
    envelope: &Envelope,
    spec: &AgentRuntimeSpec,
    execution_binding: &TaskExecutionBinding,
    evidence: &BrowserTaskEvidence,
) -> Result<bool, StoreError> {
    let task_uid = Uuid::new_v4();
    let operation_id = Uuid::new_v4();
    let runtime_name = format!("task-{}", operation_id.simple());
    let digest = format!("sha256:{}", "a".repeat(64));
    let command = ["example-agent".to_owned(), "run".to_owned()];
    let decision = AdmissionDecision::Admit;
    store
        .reserve_task(&TaskReservationRequest {
            task_uid,
            operation_id,
            idempotency_key,
            submitter_service: service,
            acting_user: Some(identity.display_email.as_str()),
            acting_user_id: Some(identity.user_id.as_str()),
            owner: identity.display_email.as_str(),
            owner_user_id: identity.user_id.as_str(),
            workflow: "browser-limit@1",
            workflow_name: Some("browser-limit"),
            workflow_version: Some(1),
            workflow_digest: Some(&digest),
            user_envelope_instance_id: Some(envelope_instance_id),
            user_envelope_revision: Some(envelope.revision),
            user_envelope_digest: Some(envelope_digest),
            coding_agent_runtime: "example-agent@1",
            runtime_uid: None,
            runtime_namespace: "steward-test",
            runtime_name: &runtime_name,
            runtime_ownership: RuntimeOwnership::Provisioned,
            runtime_spec: spec,
            agent_command: &command,
            execution_binding: Some(execution_binding),
            source_provenance: None,
            direct_task_evidence: None,
            task_origin: TaskOrigin::Browser,
            browser_task_evidence: Some(evidence),
            user_envelope_snapshot: Some(envelope),
            candidate_digest: &digest,
            admission_decision: &decision,
            inert_manifest_digest: &digest,
            active_manifest_digest: &digest,
        })
        .await
        .map(|reservation| reservation.inserted)
}

async fn reserve_browser_direct_package_fixture(
    store: &PgStore,
    idempotency_key: &str,
    service: &str,
    identity: &steward_types::CanonicalPrincipal,
    envelope_instance_id: &str,
    envelope_digest: &str,
    envelope: &Envelope,
    spec: &AgentRuntimeSpec,
    execution_binding: &TaskExecutionBinding,
    evidence: &BrowserTaskEvidence,
) -> Result<bool, StoreError> {
    let task_uid = Uuid::new_v4();
    let operation_id = Uuid::new_v4();
    let runtime_name = format!("task-{}", operation_id.simple());
    let digest = format!("sha256:{}", "a".repeat(64));
    let command = ["example-agent".to_owned(), "run".to_owned()];
    let decision = AdmissionDecision::Admit;
    store
        .reserve_task(&TaskReservationRequest {
            task_uid,
            operation_id,
            idempotency_key,
            submitter_service: service,
            acting_user: Some(identity.display_email.as_str()),
            acting_user_id: Some(identity.user_id.as_str()),
            owner: identity.display_email.as_str(),
            owner_user_id: identity.user_id.as_str(),
            workflow: "direct:browser-package@1",
            workflow_name: None,
            workflow_version: None,
            workflow_digest: None,
            user_envelope_instance_id: Some(envelope_instance_id),
            user_envelope_revision: Some(envelope.revision),
            user_envelope_digest: Some(envelope_digest),
            coding_agent_runtime: "example-agent@1",
            runtime_uid: None,
            runtime_namespace: "steward-test",
            runtime_name: &runtime_name,
            runtime_ownership: RuntimeOwnership::Provisioned,
            runtime_spec: spec,
            agent_command: &command,
            execution_binding: Some(execution_binding),
            source_provenance: None,
            direct_task_evidence: None,
            task_origin: TaskOrigin::Browser,
            browser_task_evidence: Some(evidence),
            user_envelope_snapshot: Some(envelope),
            candidate_digest: &digest,
            admission_decision: &decision,
            inert_manifest_digest: &digest,
            active_manifest_digest: &digest,
        })
        .await
        .map(|reservation| reservation.inserted)
}

#[tokio::test]
async fn browser_task_rows_persist_and_concurrency_is_bounded_without_breaking_exact_retries()
-> Result<(), Box<dyn Error>> {
    install_rustls_crypto_provider()?;
    let database_url = env::var("STEWARD_TEST_DATABASE_URL")?;
    let store = PgStore::new(
        PgPoolOptions::new()
            .max_connections(8)
            .connect(&database_url)
            .await?,
    );
    store.migrate().await?;
    let suffix = Uuid::new_v4().simple().to_string();
    let identity = store
        .register_canonical_identity(
            &OrganizationIdentityPolicy::new(
                "https://accounts.google.com",
                "example.com",
                OrganizationId::parse("org_example")?,
            )?
            .validate(
                "https://accounts.google.com",
                &format!("browser-limit-{suffix}"),
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
            llms: Vec::new(),
            tools: Vec::new(),
            budget: Budget {
                monthly_limit: "1.00".to_owned(),
                single_run_limit: Some("0.10".to_owned()),
                currency: "USD".to_owned(),
            },
            runtime_minutes_limit: Some("10".to_owned()),
            ttl: Duration("15m".to_owned()),
            runner: RunnerRequirements::default(),
        },
    };
    let template_id = format!("browser-limit-{suffix}");
    store
        .insert_envelope_template_revision(EnvelopeTemplatePublication {
            template_id: &template_id,
            display_name: "Browser limit",
            member_roles: std::slice::from_ref(&template_id),
            ceiling: &envelope,
            auto_provision_threshold: Some(&envelope),
            allow_inline_browser_tasks: true,
            authored_by: "test-bootstrap",
        })
        .await?;
    let request = store
        .reserve_envelope_request(EnvelopeRequestReservationRequest {
            owner_user_id: &identity.user_id,
            template_id: Some(&template_id),
            template_revision: Some(1),
            requested_envelope: &envelope,
            idempotency_key: &format!("browser-limit-envelope-{suffix}"),
            actor: "test-bootstrap",
        })
        .await?
        .record;
    let envelope_instance_id = format!("env_{}", request.id.simple());
    let envelope_digest = format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&envelope)?)
    );
    store
        .append_envelope_request_status(
            request.id,
            EnvelopeRequestStatusUpdate {
                from: EnvelopeRequestStatus::Pending,
                to: EnvelopeRequestStatus::Provisioned,
                approval_id: Some(Uuid::new_v4()),
                envelope_instance_id: Some(&envelope_instance_id),
                envelope_digest: Some(&envelope_digest),
                reason: None,
                rationale: Some("bounded browser-task fixture"),
                evidence_url: None,
                expires_at: None,
                approved_envelope: Some(&envelope),
                actor: "test-bootstrap",
            },
        )
        .await?;
    let workflow_digest = format!("sha256:{}", "a".repeat(64));
    store
        .publish_initial_workflow(WorkflowPublication {
            name: "browser-limit",
            display_name: "Browser limit",
            agent: "example-agent@1",
            prompt: "Exercise the browser-origin concurrency boundary.",
            content_digest: &workflow_digest,
            published_by: "test-bootstrap",
        })
        .await?;
    let spec = AgentRuntimeSpec {
        principal: Principal::User {
            acting_user: identity.display_email.clone(),
        },
        owner: identity.display_email.clone(),
        canonical_authority: Some(CanonicalAuthorityBinding::new(
            identity.user_id.clone(),
            Some(identity.user_id.clone()),
        )?),
        agent_type: AgentType {
            name: "example-agent@1".to_owned(),
        },
        llms: Vec::new(),
        tools: Vec::new(),
        budget: envelope.spec.budget.clone(),
        ttl: envelope.spec.ttl.clone(),
        runner: envelope.spec.runner.clone(),
        bindings: None,
    };
    let execution_binding = disposable_execution_binding()?;
    let evidence: BrowserTaskEvidence = serde_json::from_value(json!({
        "source": "steward:registry/browser-limit",
        "revision": "steward:version:1",
        "path": "task-definition.json",
        "closureDigest": format!("steward:sha256:{}", "b".repeat(64))
    }))?;
    let service = format!("browser-limit-{suffix}");
    let inline_evidence = browser_direct_package_evidence("inline", None)?;
    let repository_evidence = browser_direct_package_evidence(
        "https://github.com/example-org/agentic-ops.git",
        Some(format!("git:sha1:{}", "e".repeat(40))),
    )?;
    for (kind, evidence) in [
        ("inline", &inline_evidence),
        ("repository", &repository_evidence),
    ] {
        let key = format!("browser-{kind}-task-{suffix}");
        assert!(
            reserve_browser_direct_package_fixture(
                &store,
                &key,
                &service,
                &identity,
                &envelope_instance_id,
                &envelope_digest,
                &envelope,
                &spec,
                &execution_binding,
                evidence,
            )
            .await?,
            "the {kind} browser package must persist through PgStore"
        );
        let persisted = store
            .task_by_idempotency(&service, identity.user_id.as_str(), &key)
            .await?
            .ok_or("browser package Task was not persisted")?;
        assert_eq!(persisted.task_origin, TaskOrigin::Browser);
        assert!(persisted.direct_task_evidence.is_none());
        assert_eq!(persisted.browser_task_evidence.as_ref(), Some(evidence));
        assert!(persisted.workflow_name.is_none());
        assert!(persisted.workflow_version.is_none());
        assert!(persisted.workflow_digest.is_none());
        assert_eq!(
            persisted.user_envelope_instance_id.as_deref(),
            Some(envelope_instance_id.as_str())
        );
        assert_eq!(persisted.user_envelope_revision, Some(envelope.revision));
        assert_eq!(
            persisted.user_envelope_digest.as_deref(),
            Some(envelope_digest.as_str())
        );
    }
    let first_idempotency_key = format!("browser-limit-task-{suffix}-0");
    for index in 0..(MAX_ACTIVE_BROWSER_TASKS_PER_USER - 2) {
        let idempotency_key = format!("browser-limit-task-{suffix}-{index}");
        assert!(
            reserve_browser_concurrency_fixture(
                &store,
                &idempotency_key,
                &service,
                &identity,
                &envelope_instance_id,
                &envelope_digest,
                &envelope,
                &spec,
                &execution_binding,
                &evidence,
            )
            .await?
        );
    }
    assert!(
        !reserve_browser_concurrency_fixture(
            &store,
            &first_idempotency_key,
            &service,
            &identity,
            &envelope_instance_id,
            &envelope_digest,
            &envelope,
            &spec,
            &execution_binding,
            &evidence,
        )
        .await?,
        "an exact retry remains recoverable at the concurrency limit"
    );
    let rejected = reserve_browser_concurrency_fixture(
        &store,
        &format!("browser-limit-task-{suffix}-overflow"),
        &service,
        &identity,
        &envelope_instance_id,
        &envelope_digest,
        &envelope,
        &spec,
        &execution_binding,
        &evidence,
    )
    .await;
    assert_eq!(rejected, Err(StoreError::BrowserTaskConcurrencyLimit));
    Ok(())
}

#[derive(Clone, Default)]
struct AmbiguousTaskRuntime {
    starts: Arc<AtomicUsize>,
    ensures: Arc<AtomicUsize>,
    deletes: Arc<AtomicUsize>,
    fail_next_ensure: Arc<AtomicBool>,
    succeed_next_start: Arc<AtomicBool>,
    terminal_observed: Arc<AtomicBool>,
    fail_next_observation: Arc<AtomicBool>,
    provider_control_bindings: Option<ProviderControlExecutionBindings>,
}

impl SandboxRuntime for AmbiguousTaskRuntime {
    fn provider_control_bindings(&self) -> Option<ProviderControlExecutionBindings> {
        self.provider_control_bindings.clone()
    }

    async fn ensure(&self, request: &SandboxRequest) -> Result<SandboxObservation, PortError> {
        self.ensures.fetch_add(1, Ordering::SeqCst);
        if self.fail_next_ensure.swap(false, Ordering::SeqCst) {
            return Err(PortError::SandboxFailed);
        }
        Ok(SandboxObservation::Running {
            refs: RuntimeRefs {
                workspace: Some(format!("workspace-{}", request.workspace_key)),
                sandbox: Some(format!("sandbox-{}", request.runtime.0)),
                litellm_key: None,
            },
        })
    }

    async fn delete(&self, _request: &SandboxRequest) -> Result<SandboxObservation, PortError> {
        self.deletes.fetch_add(1, Ordering::SeqCst);
        Ok(SandboxObservation::Absent)
    }
}

#[derive(Clone, Default)]
struct ActiveInference {
    provisions: Arc<AtomicUsize>,
    revocations: Arc<AtomicUsize>,
}

impl InferencePlane for ActiveInference {
    fn capabilities(&self) -> InferenceCapabilities {
        let mut capabilities = InferenceCapabilities::default();
        capabilities.model_allowlist = true;
        capabilities.spend_enforcement = true;
        capabilities
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
        request: &InferenceRequest,
    ) -> Result<ProvisionedInference, PortError> {
        self.provisions.fetch_add(1, Ordering::SeqCst);
        Ok(ProvisionedInference {
            reference: format!("inference-{}", request.runtime.0),
            credential: InferenceCredential::new("fixture-inference-credential".to_owned()),
        })
    }

    async fn reconcile_configuration(&self, _request: &InferenceRequest) -> Result<(), PortError> {
        Ok(())
    }

    async fn observe(&self, request: &InferenceRequest) -> Result<InferenceObservation, PortError> {
        Ok(InferenceObservation::Active {
            reference: format!("inference-{}", request.runtime.0),
            spend: steward_types::SpendSummary {
                observed_amount: "0".to_owned(),
                currency: request.budget.currency.clone(),
            },
        })
    }

    async fn revoke(&self, _request: &InferenceRequest) -> Result<(), PortError> {
        self.revocations.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

impl SandboxTaskRuntime for AmbiguousTaskRuntime {
    fn provider_control_bindings(&self) -> Option<ProviderControlExecutionBindings> {
        self.provider_control_bindings.clone()
    }

    async fn start_task(
        &self,
        _attempt_id: &TaskAttemptId,
        request: &SandboxTaskRequest,
        _input_archive: &[u8],
    ) -> Result<SandboxTaskObservation, PortError> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        if self.succeed_next_start.swap(false, Ordering::SeqCst) {
            if request.diagnostics.execution_log != ExecutionLogMode::Full {
                return Err(PortError::Failed {
                    reason: "successful fixture did not request full execution diagnostics"
                        .to_owned(),
                });
            }
            return Ok(SandboxTaskObservation::SucceededWithTranscript {
                adapter_observation_id: "successful-task-execution".to_owned(),
                output: SandboxTaskOutput {
                    archive: task_output_archive(&[("out/result.txt", b"successful task output")]),
                },
                transcript: SandboxTaskTranscript {
                    stdout: b"successful task stdout".to_vec(),
                    stderr: b"successful task stderr".to_vec(),
                },
            });
        }
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
async fn connections_runtime_error_fails_owner_operation_and_cleans_up()
-> Result<(), Box<dyn Error>> {
    install_rustls_crypto_provider()?;
    let database_url = env::var("STEWARD_TEST_DATABASE_URL")?;
    let store = PgStore::new(
        PgPoolOptions::new()
            .max_connections(8)
            .connect(&database_url)
            .await?,
    );
    store.migrate().await?;
    let suffix = Uuid::new_v4().simple().to_string();
    let alice_email = Email(format!("alice-{suffix}@example.com"));
    let alice = store
        .register_canonical_identity(
            &OrganizationIdentityPolicy::new(
                "https://accounts.google.com",
                "example.com",
                OrganizationId::parse("org_example")?,
            )?
            .validate(
                "https://accounts.google.com",
                &format!("connections-alice-{suffix}"),
                "example.com",
                alice_email.as_str(),
                true,
            )?,
            "test-bootstrap",
        )
        .await?;
    let bob_email = Email(format!("bob-{suffix}@example.org"));
    let bob = store
        .register_canonical_identity(
            &OrganizationIdentityPolicy::new(
                "https://accounts.google.com",
                "example.org",
                OrganizationId::parse("org_example")?,
            )?
            .validate(
                "https://accounts.google.com",
                &format!("connections-bob-{suffix}"),
                "example.org",
                bob_email.as_str(),
                true,
            )?,
            "test-bootstrap",
        )
        .await?;
    let config = GovernedConnectionsConfig::new(
        ConnectionExecutionBindings {
            artifact_trust_mode: "github-attestation".to_owned(),
            bridge_image_digest: format!("ghcr.io/example-org/bridge@sha256:{}", "a".repeat(64)),
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
    let alice_session = ConnectionSession {
        subject: ConnectionSubject {
            canonical_user_id: alice.user_id.clone(),
            display_email: alice_email.as_str().to_owned(),
        },
        binding: (),
    };
    let bob_session = ConnectionSession {
        subject: ConnectionSubject {
            canonical_user_id: bob.user_id,
            display_email: bob_email.as_str().to_owned(),
        },
        binding: (),
    };
    let reserved = broker
        .start(&alice_session)
        .await
        .map_err(|error| io::Error::other(format!("reserve connection start: {error:?}")))?;
    let task_uid = reserved.operation_id;
    let kubernetes = AmbiguousKubernetes::default();
    let (client, _server) = kubernetes_client(kubernetes.clone()).await?;
    let runtime = AmbiguousTaskRuntime {
        provider_control_bindings: Some(ProviderControlExecutionBindings {
            artifact_trust_mode: "github-attestation".to_owned(),
            bridge_image_digest: format!("ghcr.io/example-org/bridge@sha256:{}", "a".repeat(64)),
            mcp_gw_origin: "https://mcp-gw.example.test".to_owned(),
            mcp_gw_version: "0.3.2".to_owned(),
            namespace: "steward-test".to_owned(),
            runtime_class: "sandbox-vm".to_owned(),
        }),
        ..AmbiguousTaskRuntime::default()
    };

    reconcile_current(&client, &runtime, &store, task_uid).await?;
    reconcile_current(&client, &runtime, &store, task_uid).await?;
    reconcile_current(&client, &runtime, &store, task_uid).await?;
    reconcile_current(&client, &runtime, &store, task_uid).await?;
    reconcile_current(&client, &runtime, &store, task_uid).await?;
    assert_eq!(
        operation(&store, task_uid).await?.state,
        TaskOrchestrationState::ActivationPending
    );
    let active_runtime = kubernetes
        .runtime
        .lock()
        .map_err(|_| io::Error::other("runtime fixture was poisoned"))?
        .clone()
        .ok_or_else(|| io::Error::other("active connection runtime is absent"))?;
    reconcile_agent_runtime_work_item(
        &client,
        runtime.clone(),
        ActiveInference::default(),
        store.clone(),
        active_runtime,
    )
    .await?;
    runtime.fail_next_ensure.store(true, Ordering::SeqCst);
    let finalized_runtime = kubernetes
        .runtime
        .lock()
        .map_err(|_| io::Error::other("runtime fixture was poisoned"))?
        .clone()
        .ok_or_else(|| io::Error::other("finalized connection runtime is absent"))?;
    reconcile_agent_runtime_work_item(
        &client,
        runtime.clone(),
        ActiveInference::default(),
        store.clone(),
        finalized_runtime,
    )
    .await?;
    let failed_runtime = kubernetes
        .runtime
        .lock()
        .map_err(|_| io::Error::other("runtime fixture was poisoned"))?
        .clone()
        .ok_or_else(|| io::Error::other("failed connection runtime is absent"))?;
    assert_eq!(
        failed_runtime.status.as_ref().map(|status| status.phase),
        Some(Phase::Failed)
    );

    reconcile_current(&client, &runtime, &store, task_uid).await?;
    assert_eq!(
        operation(&store, task_uid).await?.state,
        TaskOrchestrationState::CleanupPending
    );
    let failed_task = store
        .task(task_uid)
        .await?
        .ok_or(StoreError::TaskNotFound)?;
    assert_eq!(failed_task.phase, TaskPhase::Failed);
    assert_eq!(
        failed_task.failure_reason.as_deref(),
        Some("runtime_start_failed")
    );

    reconcile_current(&client, &runtime, &store, task_uid).await?;
    let deleting_runtime = kubernetes
        .runtime
        .lock()
        .map_err(|_| io::Error::other("runtime fixture was poisoned"))?
        .clone()
        .ok_or_else(|| io::Error::other("deleting connection runtime is absent"))?;
    assert!(deleting_runtime.metadata.deletion_timestamp.is_some());
    reconcile_agent_runtime_work_item(
        &client,
        runtime.clone(),
        ActiveInference::default(),
        store.clone(),
        deleting_runtime,
    )
    .await?;
    reconcile_current(&client, &runtime, &store, task_uid).await?;
    reconcile_current(&client, &runtime, &store, task_uid).await?;
    assert_eq!(
        operation(&store, task_uid).await?.state,
        TaskOrchestrationState::Finalized
    );
    assert!(
        kubernetes
            .runtime
            .lock()
            .map_err(|_| io::Error::other("runtime fixture was poisoned"))?
            .is_none()
    );

    let connection_reconciler = ConnectionOperationReconciler::new(store.clone());
    connection_reconciler.reconcile_once().await?;
    connection_reconciler.reconcile_once().await?;
    let owner_operation = store
        .connection_operation(task_uid, &alice.user_id)
        .await?
        .ok_or_else(|| io::Error::other("owner connection operation is absent"))?;
    assert_eq!(owner_operation.finalization_state, "finalized");
    assert_eq!(owner_operation.cleanup_state, "clean");
    assert!(
        broker
            .start_operation(&bob_session, task_uid)
            .await
            .map_err(|error| io::Error::other(format!("read Bob operation: {error:?}")))?
            .is_none(),
        "a connection failure must remain scoped to its owner"
    );
    let Some(ConnectionStartOperation::Failed(error)) = broker
        .start_operation(&alice_session, task_uid)
        .await
        .map_err(|error| io::Error::other(format!("read Alice operation: {error:?}")))?
    else {
        return Err(io::Error::other("owner did not observe a failed connection operation").into());
    };
    assert_eq!(error, ConnectionBrokerError::RuntimeStartFailed);
    let response = error.into_response();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response_json(response).await?,
        json!({
            "apiVersion": "steward.connections/v1",
            "error": "runtime_start_failed"
        })
    );
    Ok(())
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
async fn multiple_named_envelope_templates_coexist_for_one_role_and_pin_requests_independently()
-> Result<(), Box<dyn Error>> {
    install_rustls_crypto_provider()?;
    let database_url = env::var("STEWARD_TEST_DATABASE_URL")?;
    let store = PgStore::new(
        PgPoolOptions::new()
            .max_connections(4)
            .connect(&database_url)
            .await?,
    );
    store.migrate().await?;
    let suffix = Uuid::new_v4().simple().to_string();
    let role = format!("engineer-{suffix}");
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
                &format!("template-subject-{suffix}"),
                "example.com",
                email.as_str(),
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
            tools: Vec::new(),
            budget: Budget {
                monthly_limit: "25.00".to_owned(),
                single_run_limit: Some("5.00".to_owned()),
                currency: "USD".to_owned(),
            },
            runtime_minutes_limit: None,
            ttl: Duration("1h".to_owned()),
            runner: RunnerRequirements::default(),
        },
    };
    let template_ids = [format!("develop-{suffix}"), format!("review-{suffix}")];
    for (template_id, display_name) in template_ids.iter().zip(["Develop", "Review"]) {
        store
            .insert_envelope_template_revision(EnvelopeTemplatePublication {
                template_id,
                display_name,
                member_roles: std::slice::from_ref(&role),
                ceiling: &envelope,
                auto_provision_threshold: Some(&envelope),
                allow_inline_browser_tasks: true,
                authored_by: "test-bootstrap",
            })
            .await?;
    }

    let eligible = store
        .available_envelope_templates(std::slice::from_ref(&role))
        .await?;
    assert_eq!(
        eligible
            .iter()
            .map(|template| template.template_id.as_str())
            .collect::<Vec<_>>(),
        template_ids.iter().map(String::as_str).collect::<Vec<_>>()
    );
    let mut request_ids = Vec::new();
    for (index, template_id) in template_ids.iter().enumerate() {
        let request = store
            .reserve_envelope_request(EnvelopeRequestReservationRequest {
                owner_user_id: &identity.user_id,
                template_id: Some(template_id),
                template_revision: Some(envelope.revision),
                requested_envelope: &envelope,
                idempotency_key: &format!("template-request-{suffix}-{index}"),
                actor: identity.user_id.as_str(),
            })
            .await?
            .record;
        assert_eq!(request.template_id.as_ref(), Some(template_id));
        request_ids.push(request.id);
    }
    assert_ne!(request_ids[0], request_ids[1]);

    let filing_token = store
        .claim_envelope_request_decision_filing(request_ids[0], "admin-test")
        .await?;
    assert!(matches!(
        store
            .claim_envelope_request_decision_filing(request_ids[0], "admin-retry")
            .await,
        Err(StoreError::DecisionFilingInProgress)
    ));
    store
        .complete_envelope_request_decision_filing(
            request_ids[0],
            filing_token,
            "PROJ-123",
            "https://jira.example.com/browse/PROJ-123",
            "admin-test",
        )
        .await?;
    let reference = store
        .envelope_request_decision_reference(request_ids[0])
        .await?
        .ok_or(StoreError::DecisionReferenceMismatch)?;
    assert_eq!(reference.decision_key, "PROJ-123");
    assert!(matches!(
        store
            .claim_envelope_request_decision_filing(request_ids[0], "admin-retry")
            .await,
        Err(StoreError::DecisionReferenceMismatch)
    ));

    let released_token = store
        .claim_envelope_request_decision_filing(request_ids[1], "admin-test")
        .await?;
    store
        .release_envelope_request_decision_filing(request_ids[1], released_token)
        .await?;
    store
        .claim_envelope_request_decision_filing(request_ids[1], "admin-retry")
        .await?;
    Ok(())
}

#[tokio::test]
async fn cumulative_spend_top_up_is_append_only_instance_scoped_and_idempotent()
-> Result<(), Box<dyn Error>> {
    install_rustls_crypto_provider()?;
    let database_url = env::var("STEWARD_TEST_DATABASE_URL")?;
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool.clone());
    store.migrate().await?;
    // Exercise the unified runtime-approval projection against real PostgreSQL even when this
    // isolated run has not yet created an exception approval.
    store.admin_approvals().await?;
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
                &format!("escalation-subject-{suffix}"),
                "example.com",
                email.as_str(),
                true,
            )?,
            "test-bootstrap",
        )
        .await?;
    let template_id = format!("develop-{suffix}");
    let envelope = Envelope {
        revision: 1,
        spec: EnvelopeSpec {
            llms: vec![ModelRef {
                provider: "example".to_owned(),
                model: "model-a".to_owned(),
            }],
            tools: Vec::new(),
            budget: Budget {
                monthly_limit: "100.00".to_owned(),
                single_run_limit: Some("10.00".to_owned()),
                currency: "USD".to_owned(),
            },
            runtime_minutes_limit: Some("1.00".to_owned()),
            ttl: Duration("1h".to_owned()),
            runner: RunnerRequirements::default(),
        },
    };
    store
        .insert_envelope_template_revision(EnvelopeTemplatePublication {
            template_id: &template_id,
            display_name: "Develop",
            member_roles: std::slice::from_ref(&template_id),
            ceiling: &envelope,
            auto_provision_threshold: Some(&envelope),
            allow_inline_browser_tasks: true,
            authored_by: "test-bootstrap",
        })
        .await?;
    let request = store
        .reserve_envelope_request(EnvelopeRequestReservationRequest {
            owner_user_id: &identity.user_id,
            template_id: Some(&template_id),
            template_revision: Some(envelope.revision),
            requested_envelope: &envelope,
            idempotency_key: &format!("envelope-{suffix}"),
            actor: identity.user_id.as_str(),
        })
        .await?
        .record;
    let approval_id = Uuid::new_v4();
    let envelope_instance_id = format!("env_{}", request.id.simple());
    let envelope_digest = format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&envelope)?)
    );
    store
        .append_envelope_request_status(
            request.id,
            EnvelopeRequestStatusUpdate {
                from: EnvelopeRequestStatus::Pending,
                to: EnvelopeRequestStatus::Provisioned,
                approval_id: Some(approval_id),
                envelope_instance_id: Some(&envelope_instance_id),
                envelope_digest: Some(&envelope_digest),
                reason: None,
                rationale: Some("bounded test approval"),
                evidence_url: None,
                expires_at: None,
                approved_envelope: Some(&envelope),
                actor: "test-bootstrap",
            },
        )
        .await?;
    let workflow = format!("escalation-{suffix}");
    let workflow_digest = format!("sha256:{:x}", Sha256::digest(workflow.as_bytes()));
    store
        .publish_initial_workflow(WorkflowPublication {
            name: &workflow,
            display_name: "Escalation test",
            agent: "example-agent@1",
            prompt: "Exercise cumulative spend escalation.",
            content_digest: &workflow_digest,
            published_by: "test-bootstrap",
        })
        .await?;
    let service = format!("escalation-{suffix}");
    let spec = AgentRuntimeSpec {
        principal: Principal::Service {
            name: service.clone(),
            acting_user: Some(email.clone()),
        },
        owner: email.clone(),
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
    let agent_command = ["example-agent".to_owned(), "run".to_owned()];
    let admission_decision = AdmissionDecision::Admit;
    let reservation = store
        .reserve_task(&TaskReservationRequest {
            task_uid,
            operation_id,
            idempotency_key: &format!("escalation-task-{suffix}"),
            submitter_service: &service,
            acting_user: Some(email.as_str()),
            acting_user_id: Some(identity.user_id.as_str()),
            owner: email.as_str(),
            owner_user_id: identity.user_id.as_str(),
            workflow: &workflow,
            workflow_name: Some(&workflow),
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
            source_provenance: None,
            direct_task_evidence: None,
            task_origin: steward_types::direct_package::TaskOrigin::Unknown,
            browser_task_evidence: None,
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
            b"live execution input",
        )
        .await?;
    store
        .request_task_execution(task_uid, &service, identity.user_id.as_str())
        .await?;
    store
        .authorize_task_runtime_creation(
            task_uid,
            reservation.operation.generation,
            "test-controller",
        )
        .await?;
    let operation = store
        .task_runtime_operation(task_uid)
        .await?
        .ok_or(StoreError::TaskNotFound)?;
    let runtime_uid = format!("runtime-{suffix}");
    store
        .record_task_runtime_observed(
            task_uid,
            operation.generation,
            &runtime_uid,
            "1",
            "test-controller",
        )
        .await?;
    let operation = store
        .task_runtime_operation(task_uid)
        .await?
        .ok_or(StoreError::TaskNotFound)?;
    store
        .decide_task_runtime_authority_v3(task_uid, operation.generation, "test-controller")
        .await?;
    let operation = store
        .task_runtime_operation(task_uid)
        .await?
        .ok_or(StoreError::TaskNotFound)?;
    store
        .decide_task_runtime_authority_v3(task_uid, operation.generation, "test-controller")
        .await?;
    let operation = store
        .task_runtime_operation(task_uid)
        .await?
        .ok_or(StoreError::TaskNotFound)?;
    store
        .record_task_activation_observed(
            task_uid,
            operation.generation,
            &TaskActivationObservation {
                runtime_uid: &runtime_uid,
                resource_version: "2",
                active_manifest_digest: &active_digest,
                provider_set_ready: true,
            },
            "test-controller",
        )
        .await?;
    let attempt = store
        .claim_task_execution_attempt(
            task_uid,
            &format!("sha256:{}", "a".repeat(64)),
            &format!("sha256:{}", "b".repeat(64)),
            "test-controller",
        )
        .await?;
    let attempt = match attempt {
        TaskExecutionTransition::Created(attempt) => attempt,
        transition => {
            return Err(io::Error::other(format!(
                "live-log execution attempt was not created: {transition:?}"
            ))
            .into());
        }
    };
    let attempt = match store
        .authorize_task_execution_start(attempt.attempt_id, attempt.generation, "test-controller")
        .await?
    {
        TaskExecutionTransition::Applied(attempt) => attempt,
        transition => {
            return Err(io::Error::other(format!(
                "live-log execution start was not authorized: {transition:?}"
            ))
            .into());
        }
    };
    let attempt = match store
        .record_task_execution_observation(
            attempt.attempt_id,
            attempt.generation,
            TaskExecutionObservation::Running {
                adapter_observation_id: "live-attempt",
                execution_stdout: Some(b"live stdout"),
                execution_stderr: Some(b"live stderr"),
            },
            "test-controller",
        )
        .await?
    {
        TaskExecutionTransition::Applied(attempt) => attempt,
        transition => {
            return Err(io::Error::other(format!(
                "live-log execution observation was not applied: {transition:?}"
            ))
            .into());
        }
    };
    let live_log = store
        .agent_run_execution_log(
            task_uid,
            Some(identity.user_id.as_str()),
            AgentRunLogStream::Stdout,
        )
        .await?
        .ok_or_else(|| io::Error::other("live execution log was not projected"))?;
    assert_eq!(live_log.content, b"live stdout");
    assert!(
        !live_log.complete,
        "a running transcript must remain pollable"
    );
    sqlx::query(
        "INSERT INTO task_lifecycle_events \
         (task_uid, event_kind, phase, provenance, at) \
         VALUES ($1, 'phase', 'running', 'recorded', clock_timestamp() - interval '2 minutes')",
    )
    .bind(task_uid)
    .execute(&pool)
    .await?;
    let runtime_minutes = store
        .observe_envelope_instance_runtime_minutes(
            &envelope_instance_id,
            task_uid,
            &runtime_uid,
            envelope
                .spec
                .runtime_minutes_limit
                .as_deref()
                .ok_or_else(|| io::Error::other("runtime-minute limit is missing"))?,
        )
        .await?;
    assert!(runtime_minutes.exhausted);
    let runtime_minutes_escalation = store
        .admin_escalations()
        .await?
        .into_iter()
        .find(|record| record.runtime_uid == runtime_uid && record.dimension == "runtime_minutes")
        .ok_or_else(|| io::Error::other("runtime-minute escalation was not projected"))?;
    assert_eq!(runtime_minutes_escalation.currency, "min");
    let runtime_grant = store
        .record_escalation_top_up(
            runtime_minutes_escalation.escalation_id,
            "10.00",
            "2999-01-01T00:00:00Z",
            "finish the bounded task",
            "test-admin",
        )
        .await?;
    assert_eq!(runtime_grant.base_limit, "1.00");
    assert_eq!(runtime_grant.target_limit, "11.00");
    assert_eq!(
        store
            .active_envelope_instance_runtime_minutes_top_up(&envelope_instance_id)
            .await?,
        Some("10.00".to_owned())
    );
    let resumed_runtime_minutes = store
        .observe_envelope_instance_runtime_minutes(
            &envelope_instance_id,
            task_uid,
            &runtime_uid,
            "1.00",
        )
        .await?;
    assert!(
        !resumed_runtime_minutes.exhausted,
        "the active instance grant must resume execution above observed runtime minutes"
    );
    store
        .record_spend_observation(
            &runtime_uid,
            1,
            "immutable-runtime-spec",
            &SpendSummary {
                observed_amount: "110.00".to_owned(),
                currency: "USD".to_owned(),
            },
            true,
        )
        .await?;

    let escalation = store
        .admin_escalations()
        .await?
        .into_iter()
        .find(|record| record.runtime_uid == runtime_uid && record.dimension == "llm_spend")
        .ok_or_else(|| io::Error::other("spend escalation was not projected"))?;
    assert_eq!(escalation.envelope_instance_id, envelope_instance_id);
    assert_eq!(escalation.limit, "100.00");
    assert_eq!(
        store
            .record_escalation_top_up(
                escalation.escalation_id,
                "5.00",
                "2999-01-01T00:00:00Z",
                "insufficient to resume the task",
                "test-admin",
            )
            .await,
        Err(StoreError::InvalidCumulativeEscalation),
        "a successful top-up must raise the effective limit above observed spend"
    );
    let grant = store
        .record_escalation_top_up(
            escalation.escalation_id,
            "25.00",
            "2999-01-01T00:00:00Z",
            "finish the bounded task",
            "test-admin",
        )
        .await?;
    assert_eq!(grant.base_limit, "100.00");
    assert_eq!(grant.target_limit, "125.00");
    assert_eq!(
        store
            .active_envelope_instance_spend_top_up(&envelope_instance_id)
            .await?,
        Some("25.00".to_owned())
    );
    let usage = store.envelope_usage(&envelope_instance_id).await?;
    assert_eq!(usage.observed_amount, Some("110.00".to_owned()));
    assert_eq!(usage.active_top_up_amount, Some("25.00".to_owned()));
    let retry = store
        .record_escalation_top_up(
            escalation.escalation_id,
            "25.0",
            "2999-01-01T00:00:00Z",
            "finish the bounded task",
            "test-admin",
        )
        .await?;
    assert_eq!(
        retry.id, grant.id,
        "retries must return the one append-only grant"
    );
    assert_eq!(
        store
            .record_escalation_top_up(
                escalation.escalation_id,
                "30.00",
                "2999-01-01T00:00:00Z",
                "different authority",
                "test-admin",
            )
            .await,
        Err(StoreError::CumulativeEscalationConflict)
    );
    let decided = store
        .cumulative_escalation(escalation.escalation_id)
        .await?
        .ok_or_else(|| io::Error::other("decided escalation was not projected"))?;
    assert_eq!(decided.grant_id, Some(grant.id));
    assert_eq!(decided.limit, "125.00");
    assert_eq!(decided.grant_active, Some(true));

    let historical_archive = task_output_archive(&[
        ("out/report.md", b"historical output"),
        (
            ".steward/diagnostics/stdout.log",
            b"historical archived stdout",
        ),
        (
            ".steward/diagnostics/stderr.log",
            b"historical archived stderr",
        ),
    ]);
    let terminal = store
        .record_task_execution_observation(
            attempt.attempt_id,
            attempt.generation,
            TaskExecutionObservation::Succeeded {
                adapter_observation_id: "live-attempt",
                result_digest: &format!("sha256:{}", "c".repeat(64)),
                result_reference: "historical-mixed-output",
                output_archive: &historical_archive,
                output_archive_contract: None,
                execution_stdout: Some(b"separate historical stdout"),
                execution_stderr: Some(b"separate historical stderr"),
            },
            "test-controller",
        )
        .await?;
    assert!(matches!(terminal, TaskExecutionTransition::Applied(_)));
    let stored_historical = store
        .agent_run_output_archive(task_uid, identity.user_id.as_str())
        .await?
        .ok_or_else(|| io::Error::other("historical mixed archive was not projected"))?;
    assert_eq!(stored_historical.contract, None);
    let historical_entries = task_output_archive_entries(
        &stored_historical.content,
        TaskOutputArchiveCompatibility::HistoricalMixedDiagnostics,
    )
    .map_err(|_| io::Error::other("historical mixed archive became unreadable"))?;
    assert_eq!(historical_entries.len(), 1);
    assert_eq!(historical_entries[0].path, "report.md");
    let historical_stdout = store
        .agent_run_execution_log(
            task_uid,
            Some(identity.user_id.as_str()),
            AgentRunLogStream::Stdout,
        )
        .await?
        .ok_or_else(|| io::Error::other("separate historical stdout is absent"))?;
    assert_eq!(historical_stdout.content, b"separate historical stdout");
    assert!(historical_stdout.complete);
    assert!(
        sqlx::query(
            "UPDATE task_submissions SET output_archive_contract = $2 WHERE task_uid = $1",
        )
        .bind(task_uid)
        .bind(TASK_OUTPUT_ARCHIVE_CONTRACT)
        .execute(&pool)
        .await
        .is_err(),
        "a historical archive must not be relabelled after its bytes become immutable"
    );
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
            runtime_minutes_limit: None,
            ttl: Duration("1h".to_owned()),
            runner: RunnerRequirements::default(),
        },
    };
    store
        .insert_envelope_template_revision(EnvelopeTemplatePublication {
            template_id: &member_role,
            display_name: &member_role,
            member_roles: std::slice::from_ref(&member_role),
            ceiling: &envelope,
            auto_provision_threshold: Some(&envelope),
            allow_inline_browser_tasks: true,
            authored_by: "admin@example.com",
        })
        .await?;
    let envelope_request = store
        .reserve_envelope_request(EnvelopeRequestReservationRequest {
            owner_user_id: &identity.user_id,
            template_id: Some(&member_role),
            template_revision: Some(envelope.revision),
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
                rationale: None,
                evidence_url: None,
                expires_at: None,
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
                rationale: None,
                evidence_url: None,
                expires_at: None,
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
            source_provenance: None,
            direct_task_evidence: None,
            task_origin: steward_types::direct_package::TaskOrigin::Unknown,
            browser_task_evidence: None,
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
    assert!(
        activated_runtime.status.is_none(),
        "activation must not fabricate executable status before ordinary reconciliation"
    );
    assert_eq!(
        operation(&store, task_uid).await?.state,
        TaskOrchestrationState::ActivationPending,
        "the Task orchestrator must wait for the ordinary runtime reconciler"
    );

    let inference = ActiveInference::default();
    reconcile_agent_runtime_work_item(
        &client,
        task_runtime.clone(),
        inference.clone(),
        store.clone(),
        activated_runtime,
    )
    .await?;
    let finalizing_runtime = kubernetes
        .runtime
        .lock()
        .map_err(|_| io::Error::other("runtime fixture was poisoned"))?
        .clone()
        .ok_or_else(|| io::Error::other("finalized runtime fixture is absent"))?;
    assert!(
        finalizing_runtime
            .metadata
            .finalizers
            .as_ref()
            .is_some_and(|finalizers| finalizers
                .iter()
                .any(|value| value == "agents.apelogic.ai/runtime")),
        "the production runtime callback must establish its cleanup finalizer"
    );
    reconcile_agent_runtime_work_item(
        &client,
        task_runtime.clone(),
        inference.clone(),
        store.clone(),
        finalizing_runtime,
    )
    .await?;
    let provisioned_runtime = kubernetes
        .runtime
        .lock()
        .map_err(|_| io::Error::other("runtime fixture was poisoned"))?
        .clone()
        .ok_or_else(|| io::Error::other("provisioned runtime fixture is absent"))?;
    let status = provisioned_runtime
        .status
        .as_ref()
        .ok_or_else(|| io::Error::other("provisioned runtime status is absent"))?;
    assert_eq!(status.phase, Phase::Running);
    assert_eq!(status.observed_generation, 2);
    assert_eq!(status.spec_digest, runtime_spec_digest(&spec)?);
    assert!(status.refs.workspace.is_some());
    assert!(status.refs.sandbox.is_some());
    assert!(status.refs.litellm_key.is_some());
    assert_eq!(inference.provisions.load(Ordering::SeqCst), 1);
    assert_eq!(task_runtime.ensures.load(Ordering::SeqCst), 1);
    assert!(kubernetes.status_patches.load(Ordering::SeqCst) >= 1);
    assert_eq!(
        task_runtime.starts.load(Ordering::SeqCst),
        0,
        "ordinary runtime provisioning must not start the Task"
    );

    reconcile_agent_runtime_work_item(
        &client,
        task_runtime.clone(),
        inference.clone(),
        store.clone(),
        provisioned_runtime,
    )
    .await?;
    assert_eq!(
        inference.provisions.load(Ordering::SeqCst),
        1,
        "a restarted reconciler must reuse the runtime's provisioned inference credential"
    );
    assert_eq!(
        task_runtime.starts.load(Ordering::SeqCst),
        0,
        "a restarted ordinary reconciler must not start Task execution"
    );

    reconcile_current(&client, &task_runtime, &store, task_uid).await?;
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
    let deleting_runtime = kubernetes
        .runtime
        .lock()
        .map_err(|_| io::Error::other("runtime fixture was poisoned"))?
        .clone()
        .ok_or_else(|| io::Error::other("deleting runtime fixture is absent"))?;
    assert!(
        deleting_runtime.metadata.deletion_timestamp.is_some(),
        "Task cleanup must request Kubernetes deletion before the runtime finalizer runs"
    );
    reconcile_agent_runtime_work_item(
        &client,
        task_runtime.clone(),
        inference.clone(),
        store.clone(),
        deleting_runtime,
    )
    .await?;
    let finalized_runtime = kubernetes
        .runtime
        .lock()
        .map_err(|_| io::Error::other("runtime fixture was poisoned"))?
        .clone()
        .ok_or_else(|| io::Error::other("finalized runtime fixture is absent"))?;
    assert!(
        finalized_runtime
            .metadata
            .finalizers
            .as_ref()
            .is_none_or(Vec::is_empty),
        "the production runtime cleanup must release its finalizer"
    );
    assert_eq!(task_runtime.deletes.load(Ordering::SeqCst), 1);
    assert_eq!(inference.revocations.load(Ordering::SeqCst), 1);
    assert!(
        kubernetes
            .credential_secret
            .lock()
            .map_err(|_| io::Error::other("credential Secret fixture was poisoned"))?
            .is_none(),
        "runtime finalization must delete the ephemeral inference credential"
    );
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
        ["runtime-uid-a", "runtime-uid-a"]
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
            source_provenance: None,
            direct_task_evidence: None,
            task_origin: steward_types::direct_package::TaskOrigin::Unknown,
            browser_task_evidence: None,
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

    let replacement_task_uid = Uuid::new_v4();
    let replacement_operation_id = Uuid::new_v4();
    let replacement_runtime_name = format!("task-{}", replacement_operation_id.simple());
    let replacement_inert_digest = manifest_digest_with_binding(
        replacement_task_uid,
        replacement_operation_id,
        &replacement_runtime_name,
        &inert_spec(&spec, &envelope),
        "inert",
        Some(&execution_binding),
    )?;
    let replacement_active_digest = manifest_digest_with_binding(
        replacement_task_uid,
        replacement_operation_id,
        &replacement_runtime_name,
        &spec,
        "active",
        Some(&execution_binding),
    )?;
    insert_task_projection_fixture(
        &pool,
        TaskProjectionFixture {
            source_task_uid: task_uid,
            task_uid: replacement_task_uid,
            operation_id: replacement_operation_id,
            idempotency_key: &format!("runtime-replacement-{suffix}"),
            task_runtime_name: &replacement_runtime_name,
            operation_runtime_name: &replacement_runtime_name,
            inert_manifest_digest: &replacement_inert_digest,
            active_manifest_digest: &replacement_active_digest,
            direct_task_evidence: None,
        },
    )
    .await?;
    store
        .put_task_inputs(
            replacement_task_uid,
            &service,
            identity.user_id.as_str(),
            b"replacement task input",
        )
        .await?;
    store
        .request_task_execution(replacement_task_uid, &service, identity.user_id.as_str())
        .await?;
    *kubernetes
        .next_runtime_uid
        .lock()
        .map_err(|_| io::Error::other("runtime UID fixture was poisoned"))? =
        Some("runtime-uid-original".to_owned());
    for _ in 0..8 {
        let current = operation(&store, replacement_task_uid).await?;
        if current.state == TaskOrchestrationState::ActivationPending
            && current.activation_effect_authorized_at.is_some()
        {
            break;
        }
        reconcile_current(&client, &task_runtime, &store, replacement_task_uid).await?;
    }
    kubernetes
        .replace_uid_after_update
        .store(true, Ordering::SeqCst);
    reconcile_current(&client, &task_runtime, &store, replacement_task_uid).await?;
    assert_eq!(
        operation(&store, replacement_task_uid).await?.state,
        TaskOrchestrationState::CleanupPending,
        "a same-name runtime replacement must never satisfy activation"
    );
    assert_eq!(
        store
            .task(replacement_task_uid)
            .await?
            .ok_or(StoreError::TaskNotFound)?
            .failure_reason
            .as_deref(),
        Some("observed_runtime_identity_changed")
    );
    for _ in 0..3 {
        if store
            .task(replacement_task_uid)
            .await?
            .ok_or(StoreError::TaskNotFound)?
            .finalized
        {
            break;
        }
        reconcile_current(&client, &task_runtime, &store, replacement_task_uid).await?;
    }
    assert!(
        store
            .task(replacement_task_uid)
            .await?
            .ok_or(StoreError::TaskNotFound)?
            .finalized
    );
    assert_eq!(
        kubernetes
            .runtime
            .lock()
            .map_err(|_| io::Error::other("runtime fixture was poisoned"))?
            .as_ref()
            .and_then(|runtime| runtime.metadata.uid.as_deref()),
        Some("runtime-uid-replacement"),
        "cleanup must leave the unbound replacement runtime untouched"
    );

    *kubernetes
        .runtime
        .lock()
        .map_err(|_| io::Error::other("runtime fixture was poisoned"))? = None;
    let failed_start_task_uid = Uuid::new_v4();
    let failed_start_operation_id = Uuid::new_v4();
    let failed_start_runtime_name = format!("task-{}", failed_start_operation_id.simple());
    let failed_start_inert_digest = manifest_digest_with_binding(
        failed_start_task_uid,
        failed_start_operation_id,
        &failed_start_runtime_name,
        &inert_spec(&spec, &envelope),
        "inert",
        Some(&execution_binding),
    )?;
    let failed_start_active_digest = manifest_digest_with_binding(
        failed_start_task_uid,
        failed_start_operation_id,
        &failed_start_runtime_name,
        &spec,
        "active",
        Some(&execution_binding),
    )?;
    insert_task_projection_fixture(
        &pool,
        TaskProjectionFixture {
            source_task_uid: task_uid,
            task_uid: failed_start_task_uid,
            operation_id: failed_start_operation_id,
            idempotency_key: &format!("failed-runtime-start-{suffix}"),
            task_runtime_name: &failed_start_runtime_name,
            operation_runtime_name: &failed_start_runtime_name,
            inert_manifest_digest: &failed_start_inert_digest,
            active_manifest_digest: &failed_start_active_digest,
            direct_task_evidence: None,
        },
    )
    .await?;
    store
        .put_task_inputs(
            failed_start_task_uid,
            &service,
            identity.user_id.as_str(),
            b"failed runtime start input",
        )
        .await?;
    store
        .request_task_execution(failed_start_task_uid, &service, identity.user_id.as_str())
        .await?;
    *kubernetes
        .next_runtime_uid
        .lock()
        .map_err(|_| io::Error::other("runtime UID fixture was poisoned"))? =
        Some("runtime-uid-failed-start".to_owned());
    for _ in 0..8 {
        let current = operation(&store, failed_start_task_uid).await?;
        if current.state == TaskOrchestrationState::ActivationPending
            && current.activation_effect_authorized_at.is_some()
        {
            break;
        }
        reconcile_current(&client, &task_runtime, &store, failed_start_task_uid).await?;
    }
    reconcile_current(&client, &task_runtime, &store, failed_start_task_uid).await?;
    {
        let mut runtime = kubernetes
            .runtime
            .lock()
            .map_err(|_| io::Error::other("runtime fixture was poisoned"))?;
        let runtime = runtime
            .as_mut()
            .ok_or_else(|| io::Error::other("failed-start runtime fixture is absent"))?;
        runtime.status = Some(AgentRuntimeStatus {
            phase: Phase::Failed,
            observed_generation: runtime.metadata.generation.unwrap_or_default(),
            spec_digest: runtime_spec_digest(&runtime.spec)?,
            refs: RuntimeRefs::default(),
            conditions: Vec::new(),
            spend: None,
        });
    }
    reconcile_current(&client, &task_runtime, &store, failed_start_task_uid).await?;
    assert_eq!(
        operation(&store, failed_start_task_uid).await?.state,
        TaskOrchestrationState::CleanupPending
    );
    assert_eq!(
        store
            .task(failed_start_task_uid)
            .await?
            .ok_or(StoreError::TaskNotFound)?
            .failure_reason
            .as_deref(),
        Some("runtime_start_failed")
    );
    for _ in 0..4 {
        if store
            .task(failed_start_task_uid)
            .await?
            .ok_or(StoreError::TaskNotFound)?
            .finalized
        {
            break;
        }
        reconcile_current(&client, &task_runtime, &store, failed_start_task_uid).await?;
    }
    let failed_start_task = store
        .task(failed_start_task_uid)
        .await?
        .ok_or(StoreError::TaskNotFound)?;
    assert!(failed_start_task.finalized);
    assert_eq!(failed_start_task.phase, TaskPhase::Failed);
    assert_eq!(
        failed_start_task.failure_reason.as_deref(),
        Some("runtime_start_failed")
    );

    let successful_task_uid = Uuid::new_v4();
    let successful_operation_id = Uuid::new_v4();
    let successful_runtime_name = format!("task-{}", successful_operation_id.simple());
    let successful_candidate_digest = digest(serde_json::to_value(&spec))?;
    let successful_inert_digest = manifest_digest_with_binding(
        successful_task_uid,
        successful_operation_id,
        &successful_runtime_name,
        &inert_spec(&spec, &envelope),
        "inert",
        Some(&execution_binding),
    )?;
    let successful_active_digest = manifest_digest_with_binding(
        successful_task_uid,
        successful_operation_id,
        &successful_runtime_name,
        &spec,
        "active",
        Some(&execution_binding),
    )?;
    let mut successful_evidence = browser_direct_package_evidence("inline", None)?;
    successful_evidence.diagnostics = DiagnosticsRequest {
        execution_log: ExecutionLogMode::Full,
    };
    successful_evidence.validate()?;
    *kubernetes
        .next_runtime_uid
        .lock()
        .map_err(|_| io::Error::other("runtime UID fixture was poisoned"))? =
        Some("runtime-uid-success".to_owned());
    store
        .reserve_task(&TaskReservationRequest {
            task_uid: successful_task_uid,
            operation_id: successful_operation_id,
            idempotency_key: &format!("successful-execution-{suffix}"),
            submitter_service: &service,
            acting_user: Some("alice@example.com"),
            acting_user_id: Some(identity.user_id.as_str()),
            owner: "alice@example.com",
            owner_user_id: identity.user_id.as_str(),
            workflow: "direct:browser-package@1",
            workflow_name: None,
            workflow_version: None,
            workflow_digest: None,
            user_envelope_instance_id: Some(&envelope_instance_id),
            user_envelope_revision: Some(envelope.revision),
            user_envelope_digest: Some(&envelope_digest),
            coding_agent_runtime: "example-agent@1",
            runtime_uid: None,
            runtime_namespace: "steward-test",
            runtime_name: &successful_runtime_name,
            runtime_ownership: RuntimeOwnership::Provisioned,
            runtime_spec: &spec,
            agent_command: &agent_command,
            execution_binding: Some(&execution_binding),
            source_provenance: None,
            direct_task_evidence: None,
            task_origin: TaskOrigin::Browser,
            browser_task_evidence: Some(&successful_evidence),
            user_envelope_snapshot: Some(&envelope),
            candidate_digest: &successful_candidate_digest,
            admission_decision: &admission_decision,
            inert_manifest_digest: &successful_inert_digest,
            active_manifest_digest: &successful_active_digest,
        })
        .await?;
    store
        .put_task_inputs(
            successful_task_uid,
            &service,
            identity.user_id.as_str(),
            b"successful task input",
        )
        .await?;
    store
        .request_task_execution(successful_task_uid, &service, identity.user_id.as_str())
        .await?;

    reconcile_current(&client, &task_runtime, &store, successful_task_uid).await?;
    reconcile_current(&client, &task_runtime, &store, successful_task_uid).await?;
    reconcile_current(&client, &task_runtime, &store, successful_task_uid).await?;
    reconcile_current(&client, &task_runtime, &store, successful_task_uid).await?;
    reconcile_current(&client, &task_runtime, &store, successful_task_uid).await?;
    let successful_activated_runtime = kubernetes
        .runtime
        .lock()
        .map_err(|_| io::Error::other("runtime fixture was poisoned"))?
        .clone()
        .ok_or_else(|| io::Error::other("successful active runtime fixture is absent"))?;
    assert_eq!(
        operation(&store, successful_task_uid).await?.state,
        TaskOrchestrationState::ActivationPending
    );

    let successful_inference = ActiveInference::default();
    reconcile_agent_runtime_work_item(
        &client,
        task_runtime.clone(),
        successful_inference.clone(),
        store.clone(),
        successful_activated_runtime,
    )
    .await?;
    let successful_finalizing_runtime = kubernetes
        .runtime
        .lock()
        .map_err(|_| io::Error::other("runtime fixture was poisoned"))?
        .clone()
        .ok_or_else(|| io::Error::other("successful finalized runtime fixture is absent"))?;
    reconcile_agent_runtime_work_item(
        &client,
        task_runtime.clone(),
        successful_inference.clone(),
        store.clone(),
        successful_finalizing_runtime,
    )
    .await?;
    reconcile_current(&client, &task_runtime, &store, successful_task_uid).await?;
    assert_eq!(
        operation(&store, successful_task_uid).await?.state,
        TaskOrchestrationState::Active
    );

    let starts_before_success = task_runtime.starts.load(Ordering::SeqCst);
    task_runtime
        .succeed_next_start
        .store(true, Ordering::SeqCst);
    for _ in 0..8 {
        reconcile_current(&client, &task_runtime, &store, successful_task_uid).await?;
        if task_runtime.starts.load(Ordering::SeqCst) > starts_before_success {
            break;
        }
    }
    assert_eq!(
        task_runtime.starts.load(Ordering::SeqCst),
        starts_before_success + 1,
        "successful execution must cross the Task adapter exactly once"
    );
    assert_eq!(
        operation(&store, successful_task_uid).await?.state,
        TaskOrchestrationState::CleanupPending,
        "successful execution must atomically authorize runtime cleanup"
    );
    assert_eq!(
        store
            .task(successful_task_uid)
            .await?
            .ok_or(StoreError::TaskNotFound)?
            .phase,
        TaskPhase::Succeeded
    );

    reconcile_current(&client, &task_runtime, &store, successful_task_uid).await?;
    let successful_deleting_runtime = kubernetes
        .runtime
        .lock()
        .map_err(|_| io::Error::other("runtime fixture was poisoned"))?
        .clone()
        .ok_or_else(|| io::Error::other("successful deleting runtime fixture is absent"))?;
    reconcile_agent_runtime_work_item(
        &client,
        task_runtime.clone(),
        successful_inference.clone(),
        store.clone(),
        successful_deleting_runtime,
    )
    .await?;
    reconcile_current(&client, &task_runtime, &store, successful_task_uid).await?;
    reconcile_current(&client, &task_runtime, &store, successful_task_uid).await?;
    let successful_task = store
        .task(successful_task_uid)
        .await?
        .ok_or(StoreError::TaskNotFound)?;
    assert!(successful_task.finalized);
    assert_eq!(successful_task.phase, TaskPhase::Succeeded);
    let successful_archive = store
        .agent_run_output_archive(successful_task_uid, identity.user_id.as_str())
        .await?
        .ok_or_else(|| io::Error::other("successful output archive is absent"))?;
    assert_eq!(
        successful_archive.contract.as_deref(),
        Some(TASK_OUTPUT_ARCHIVE_CONTRACT)
    );
    let successful_entries = task_output_archive_entries(
        &successful_archive.content,
        TaskOutputArchiveCompatibility::Strict,
    )
    .map_err(|_| io::Error::other("successful output archive violated its contract"))?;
    assert_eq!(successful_entries.len(), 1);
    assert_eq!(successful_entries[0].path, "result.txt");
    assert_eq!(
        &successful_archive.content[successful_entries[0].offset
            ..successful_entries[0].offset + successful_entries[0].size],
        b"successful task output"
    );
    for (stream, expected) in [
        (
            AgentRunLogStream::Stdout,
            b"successful task stdout".as_slice(),
        ),
        (
            AgentRunLogStream::Stderr,
            b"successful task stderr".as_slice(),
        ),
    ] {
        let log = store
            .agent_run_execution_log(successful_task_uid, Some(identity.user_id.as_str()), stream)
            .await?
            .ok_or_else(|| io::Error::other("successful execution transcript is absent"))?;
        assert_eq!(log.content, expected);
        assert!(log.complete);
    }
    let runner_transcript = store
        .task_output_transcript_for_submitter(
            successful_task_uid,
            &service,
            identity.user_id.as_str(),
        )
        .await?
        .ok_or_else(|| io::Error::other("runner output transcript is absent"))?;
    assert_eq!(
        runner_transcript.output_archive_contract.as_deref(),
        Some(TASK_OUTPUT_ARCHIVE_CONTRACT)
    );
    assert_eq!(
        runner_transcript.execution_stdout.as_deref(),
        Some(b"successful task stdout".as_slice())
    );
    assert_eq!(
        runner_transcript.execution_stderr.as_deref(),
        Some(b"successful task stderr".as_slice())
    );
    let runner_archive = task_output_archive_with_execution_transcript(
        successful_archive.content.clone(),
        b"successful task stdout",
        b"successful task stderr",
    )
    .map_err(|error| io::Error::other(format!("runner archive was not produced: {error:?}")))?;
    let runner_entries = task_output_archive_entries(
        &runner_archive,
        TaskOutputArchiveCompatibility::HistoricalMixedDiagnostics,
    )
    .map_err(|_| io::Error::other("runner archive violated its contract"))?;
    assert_eq!(
        runner_entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>(),
        ["result.txt"],
        "the reserved transcript is never listed as a Task output"
    );
    for (task_uid, submitter, owner) in [
        (
            successful_task_uid,
            "another-service",
            identity.user_id.as_str(),
        ),
        (successful_task_uid, service.as_str(), "another-owner"),
        (
            failed_start_task_uid,
            service.as_str(),
            identity.user_id.as_str(),
        ),
    ] {
        assert!(
            store
                .task_output_transcript_for_submitter(task_uid, submitter, owner)
                .await?
                .is_none(),
            "runner transcripts stay in the exact submitter scope of a succeeded Task"
        );
    }
    assert!(
        kubernetes
            .runtime
            .lock()
            .map_err(|_| io::Error::other("runtime fixture was poisoned"))?
            .is_none(),
        "successful Task finalization must remove its exact runtime"
    );

    let expired_task_uid = Uuid::new_v4();
    let expired_operation_id = Uuid::new_v4();
    let expired_runtime_name = format!("task-{}", expired_operation_id.simple());
    let expired_candidate_digest = digest(serde_json::to_value(&spec))?;
    let expired_inert_digest = manifest_digest_with_binding(
        expired_task_uid,
        expired_operation_id,
        &expired_runtime_name,
        &inert_spec(&spec, &envelope),
        "inert",
        Some(&execution_binding),
    )?;
    let expired_active_digest = manifest_digest_with_binding(
        expired_task_uid,
        expired_operation_id,
        &expired_runtime_name,
        &spec,
        "active",
        Some(&execution_binding),
    )?;
    *kubernetes
        .next_runtime_uid
        .lock()
        .map_err(|_| io::Error::other("runtime UID fixture was poisoned"))? =
        Some("runtime-uid-expired".to_owned());
    store
        .reserve_task(&TaskReservationRequest {
            task_uid: expired_task_uid,
            operation_id: expired_operation_id,
            idempotency_key: &format!("expired-runtime-{suffix}"),
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
            runtime_name: &expired_runtime_name,
            runtime_ownership: RuntimeOwnership::Provisioned,
            runtime_spec: &spec,
            agent_command: &agent_command,
            execution_binding: Some(&execution_binding),
            source_provenance: None,
            direct_task_evidence: None,
            task_origin: steward_types::direct_package::TaskOrigin::Unknown,
            browser_task_evidence: None,
            user_envelope_snapshot: Some(&envelope),
            candidate_digest: &expired_candidate_digest,
            admission_decision: &admission_decision,
            inert_manifest_digest: &expired_inert_digest,
            active_manifest_digest: &expired_active_digest,
        })
        .await?;
    store
        .put_task_inputs(
            expired_task_uid,
            &service,
            identity.user_id.as_str(),
            b"expired task input",
        )
        .await?;
    store
        .request_task_execution(expired_task_uid, &service, identity.user_id.as_str())
        .await?;

    for _ in 0..5 {
        reconcile_current(&client, &task_runtime, &store, expired_task_uid).await?;
    }
    assert_eq!(
        operation(&store, expired_task_uid).await?.state,
        TaskOrchestrationState::ActivationPending
    );
    let expired_activated_runtime = kubernetes
        .runtime
        .lock()
        .map_err(|_| io::Error::other("runtime fixture was poisoned"))?
        .clone()
        .ok_or_else(|| io::Error::other("expired active runtime fixture is absent"))?;
    let expired_inference = ActiveInference::default();
    reconcile_agent_runtime_work_item(
        &client,
        task_runtime.clone(),
        expired_inference.clone(),
        store.clone(),
        expired_activated_runtime,
    )
    .await?;
    let expired_finalizing_runtime = kubernetes
        .runtime
        .lock()
        .map_err(|_| io::Error::other("runtime fixture was poisoned"))?
        .clone()
        .ok_or_else(|| io::Error::other("expired finalized runtime fixture is absent"))?;
    reconcile_agent_runtime_work_item(
        &client,
        task_runtime.clone(),
        expired_inference.clone(),
        store.clone(),
        expired_finalizing_runtime,
    )
    .await?;
    reconcile_current(&client, &task_runtime, &store, expired_task_uid).await?;
    assert_eq!(
        operation(&store, expired_task_uid).await?.state,
        TaskOrchestrationState::Active
    );

    let mut expired_runtime = kubernetes
        .runtime
        .lock()
        .map_err(|_| io::Error::other("runtime fixture was poisoned"))?
        .clone()
        .ok_or_else(|| io::Error::other("expired provisioned runtime fixture is absent"))?;
    expired_runtime.metadata.creation_timestamp =
        Some(serde_json::from_value(json!("1970-01-01T00:00:00Z"))?);
    reconcile_agent_runtime_work_item(
        &client,
        task_runtime.clone(),
        expired_inference.clone(),
        store.clone(),
        expired_runtime,
    )
    .await?;
    let expired_operation = operation(&store, expired_task_uid).await?;
    assert_eq!(
        expired_operation.state,
        TaskOrchestrationState::CleanupPending,
        "Task TTL expiry must establish persisted cleanup authority before deletion"
    );
    assert_eq!(
        store
            .task(expired_task_uid)
            .await?
            .ok_or(StoreError::TaskNotFound)?
            .failure_reason
            .as_deref(),
        Some("task_runtime_ttl_expired")
    );
    assert!(
        kubernetes
            .runtime
            .lock()
            .map_err(|_| io::Error::other("runtime fixture was poisoned"))?
            .as_ref()
            .is_some_and(|runtime| runtime.metadata.deletion_timestamp.is_none()),
        "ordinary reconciliation must not delete an expired Task runtime before Task cleanup"
    );

    reconcile_current(&client, &task_runtime, &store, expired_task_uid).await?;
    let expired_deleting_runtime = kubernetes
        .runtime
        .lock()
        .map_err(|_| io::Error::other("runtime fixture was poisoned"))?
        .clone()
        .ok_or_else(|| io::Error::other("expired deleting runtime fixture is absent"))?;
    assert!(
        expired_deleting_runtime
            .metadata
            .deletion_timestamp
            .is_some()
    );
    reconcile_agent_runtime_work_item(
        &client,
        task_runtime.clone(),
        expired_inference.clone(),
        store.clone(),
        expired_deleting_runtime,
    )
    .await?;
    reconcile_current(&client, &task_runtime, &store, expired_task_uid).await?;
    reconcile_current(&client, &task_runtime, &store, expired_task_uid).await?;
    let expired_task = store
        .task(expired_task_uid)
        .await?
        .ok_or(StoreError::TaskNotFound)?;
    assert!(expired_task.finalized);
    assert_eq!(expired_task.phase, TaskPhase::Failed);
    assert_eq!(
        expired_task.failure_reason.as_deref(),
        Some("task_runtime_ttl_expired")
    );
    assert!(
        kubernetes
            .runtime
            .lock()
            .map_err(|_| io::Error::other("runtime fixture was poisoned"))?
            .is_none(),
        "expired Task finalization must remove its exact runtime"
    );

    let cancelled_task_uid = Uuid::new_v4();
    let cancelled_operation_id = Uuid::new_v4();
    let cancelled_runtime_name = format!("task-{}", cancelled_operation_id.simple());
    let cancelled_candidate_digest = digest(serde_json::to_value(&spec))?;
    let cancelled_inert_digest = manifest_digest_with_binding(
        cancelled_task_uid,
        cancelled_operation_id,
        &cancelled_runtime_name,
        &inert_spec(&spec, &envelope),
        "inert",
        Some(&execution_binding),
    )?;
    let cancelled_active_digest = manifest_digest_with_binding(
        cancelled_task_uid,
        cancelled_operation_id,
        &cancelled_runtime_name,
        &spec,
        "active",
        Some(&execution_binding),
    )?;
    *kubernetes
        .next_runtime_uid
        .lock()
        .map_err(|_| io::Error::other("runtime UID fixture was poisoned"))? =
        Some("runtime-uid-cancelled".to_owned());
    store
        .reserve_task(&TaskReservationRequest {
            task_uid: cancelled_task_uid,
            operation_id: cancelled_operation_id,
            idempotency_key: &format!("cancelled-runtime-{suffix}"),
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
            runtime_name: &cancelled_runtime_name,
            runtime_ownership: RuntimeOwnership::Provisioned,
            runtime_spec: &spec,
            agent_command: &agent_command,
            execution_binding: Some(&execution_binding),
            source_provenance: None,
            direct_task_evidence: None,
            task_origin: steward_types::direct_package::TaskOrigin::Unknown,
            browser_task_evidence: None,
            user_envelope_snapshot: Some(&envelope),
            candidate_digest: &cancelled_candidate_digest,
            admission_decision: &admission_decision,
            inert_manifest_digest: &cancelled_inert_digest,
            active_manifest_digest: &cancelled_active_digest,
        })
        .await?;
    store
        .put_task_inputs(
            cancelled_task_uid,
            &service,
            identity.user_id.as_str(),
            b"cancelled task input",
        )
        .await?;
    store
        .request_task_execution(cancelled_task_uid, &service, identity.user_id.as_str())
        .await?;

    for _ in 0..5 {
        reconcile_current(&client, &task_runtime, &store, cancelled_task_uid).await?;
    }
    assert_eq!(
        operation(&store, cancelled_task_uid).await?.state,
        TaskOrchestrationState::ActivationPending
    );
    let cancelled_activated_runtime = kubernetes
        .runtime
        .lock()
        .map_err(|_| io::Error::other("runtime fixture was poisoned"))?
        .clone()
        .ok_or_else(|| io::Error::other("cancelled active runtime fixture is absent"))?;
    let cancelled_inference = ActiveInference::default();
    reconcile_agent_runtime_work_item(
        &client,
        task_runtime.clone(),
        cancelled_inference.clone(),
        store.clone(),
        cancelled_activated_runtime,
    )
    .await?;
    let cancelled_finalizing_runtime = kubernetes
        .runtime
        .lock()
        .map_err(|_| io::Error::other("runtime fixture was poisoned"))?
        .clone()
        .ok_or_else(|| io::Error::other("cancelled finalized runtime fixture is absent"))?;
    reconcile_agent_runtime_work_item(
        &client,
        task_runtime.clone(),
        cancelled_inference.clone(),
        store.clone(),
        cancelled_finalizing_runtime,
    )
    .await?;
    reconcile_current(&client, &task_runtime, &store, cancelled_task_uid).await?;
    assert_eq!(
        operation(&store, cancelled_task_uid).await?.state,
        TaskOrchestrationState::Active
    );

    let starts_before_cancellation = task_runtime.starts.load(Ordering::SeqCst);
    let cancelled = store
        .request_task_finalization(cancelled_task_uid, &service, identity.user_id.as_str())
        .await?;
    assert!(cancelled.cancel_requested);
    assert_eq!(cancelled.phase, TaskPhase::Cancelled);
    reconcile_current(&client, &task_runtime, &store, cancelled_task_uid).await?;
    assert_eq!(
        operation(&store, cancelled_task_uid).await?.state,
        TaskOrchestrationState::CleanupPending,
        "cancellation must establish persisted cleanup authority before deletion"
    );
    assert_eq!(
        task_runtime.starts.load(Ordering::SeqCst),
        starts_before_cancellation,
        "a cancelled queued Task must not begin execution"
    );
    assert!(
        kubernetes
            .runtime
            .lock()
            .map_err(|_| io::Error::other("runtime fixture was poisoned"))?
            .as_ref()
            .is_some_and(|runtime| runtime.metadata.deletion_timestamp.is_none()),
        "Task cancellation must not delete the runtime before cleanup is authorized"
    );

    reconcile_current(&client, &task_runtime, &store, cancelled_task_uid).await?;
    let cancelled_deleting_runtime = kubernetes
        .runtime
        .lock()
        .map_err(|_| io::Error::other("runtime fixture was poisoned"))?
        .clone()
        .ok_or_else(|| io::Error::other("cancelled deleting runtime fixture is absent"))?;
    assert!(
        cancelled_deleting_runtime
            .metadata
            .deletion_timestamp
            .is_some()
    );
    reconcile_agent_runtime_work_item(
        &client,
        task_runtime.clone(),
        cancelled_inference.clone(),
        store.clone(),
        cancelled_deleting_runtime,
    )
    .await?;
    reconcile_current(&client, &task_runtime, &store, cancelled_task_uid).await?;
    reconcile_current(&client, &task_runtime, &store, cancelled_task_uid).await?;
    let cancelled_task = store
        .task(cancelled_task_uid)
        .await?
        .ok_or(StoreError::TaskNotFound)?;
    assert!(cancelled_task.finalized);
    assert_eq!(cancelled_task.phase, TaskPhase::Cancelled);
    assert!(
        kubernetes
            .runtime
            .lock()
            .map_err(|_| io::Error::other("runtime fixture was poisoned"))?
            .is_none(),
        "cancelled Task finalization must remove its exact runtime"
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
            source_provenance: None,
            direct_task_evidence: None,
            task_origin: steward_types::direct_package::TaskOrigin::Unknown,
            browser_task_evidence: None,
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
            direct_task_evidence: None,
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
            direct_task_evidence: None,
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

    let mut github_candidates = Vec::new();
    for (attempt, run_id) in [(3_u32, "900001"), (2_u32, "900001"), (4_u32, "900002")] {
        let candidate_task_uid = Uuid::new_v4();
        let candidate_operation_id = Uuid::new_v4();
        let candidate_runtime_name = format!("task-{}", candidate_operation_id.simple());
        let candidate_inert_digest = manifest_digest_with_binding(
            candidate_task_uid,
            candidate_operation_id,
            &candidate_runtime_name,
            &inert_spec(&spec, &envelope),
            "inert",
            Some(&execution_binding),
        )?;
        let candidate_active_digest = manifest_digest_with_binding(
            candidate_task_uid,
            candidate_operation_id,
            &candidate_runtime_name,
            &spec,
            "active",
            Some(&execution_binding),
        )?;
        let evidence = direct_task_evidence_fixture(
            candidate_task_uid,
            attempt,
            run_id,
            envelope.revision,
            &envelope_digest,
        )?;
        insert_task_projection_fixture(
            &pool,
            TaskProjectionFixture {
                source_task_uid: task_uid,
                task_uid: candidate_task_uid,
                operation_id: candidate_operation_id,
                idempotency_key: &format!("github-rerun-{run_id}-{attempt}-{suffix}"),
                task_runtime_name: &candidate_runtime_name,
                operation_runtime_name: &candidate_runtime_name,
                inert_manifest_digest: &candidate_inert_digest,
                active_manifest_digest: &candidate_active_digest,
                direct_task_evidence: Some(&evidence),
            },
        )
        .await?;
        github_candidates.push((attempt, run_id, candidate_task_uid));
    }
    let attempt_two = store
        .github_rerun_task(identity.user_id.as_str(), "example-org/caller", "900001", 1)
        .await?
        .ok_or_else(|| io::Error::other("GitHub attempt two was not correlated"))?;
    assert_eq!(attempt_two.task_uid, github_candidates[1].2);
    let attempt_three = store
        .github_rerun_task(identity.user_id.as_str(), "example-org/caller", "900001", 2)
        .await?
        .ok_or_else(|| io::Error::other("GitHub attempt three was not correlated"))?;
    assert_eq!(attempt_three.task_uid, github_candidates[0].2);
    assert!(
        store
            .github_rerun_task(identity.user_id.as_str(), "example-org/caller", "900001", 3)
            .await?
            .is_none()
    );
    assert!(
        store
            .github_rerun_task(
                "usr_abcdefabcdefabcdefabcdefabcdefab",
                "example-org/caller",
                "900001",
                1,
            )
            .await?
            .is_none(),
        "a later attempt owned by another canonical user must remain invisible"
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
    direct_task_evidence: Option<&'a serde_json::Value>,
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
          agent_command, execution_binding, source_provenance, direct_task_evidence, envelope_revision, \
          orchestration_version, orchestration_operation_id, candidate_digest, \
          service_envelope_digest, original_admission_decision, original_admission_deltas) \
         SELECT $1, $2, submitter_service, acting_user, acting_user_id, owner, owner_user_id, \
                identity_binding_state, workflow, \
                CASE WHEN $6::jsonb IS NULL THEN workflow_name ELSE NULL END, \
                CASE WHEN $6::jsonb IS NULL THEN workflow_version ELSE NULL END, \
                CASE WHEN $6::jsonb IS NULL THEN workflow_digest ELSE NULL END, \
                user_envelope_instance_id, user_envelope_revision, user_envelope_digest, \
                authority_kind, user_envelope_snapshot, coding_agent_runtime, NULL, \
                runtime_namespace, $3, runtime_ownership, 'submitted', runtime_spec, agent_command, \
                execution_binding, \
                CASE WHEN $6::jsonb IS NULL \
                     THEN source_provenance ELSE $6::jsonb -> 'sourceProvenance' END, \
                COALESCE($6::jsonb, direct_task_evidence), envelope_revision, orchestration_version, \
                $4, candidate_digest, service_envelope_digest, original_admission_decision, \
                original_admission_deltas \
         FROM task_submissions WHERE task_uid = $5",
    )
    .bind(fixture.task_uid)
    .bind(fixture.idempotency_key)
    .bind(fixture.task_runtime_name)
    .bind(fixture.operation_id)
    .bind(fixture.source_task_uid)
    .bind(fixture.direct_task_evidence)
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

fn direct_task_evidence_fixture(
    task_uid: Uuid,
    attempt: u32,
    run_id: &str,
    envelope_revision: i64,
    envelope_digest: &str,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let mut evidence: serde_json::Value = serde_json::from_str(include_str!(
        "../docs/contracts/task/v2/fixtures/positive/task-binding-evidence.json"
    ))?;
    evidence["taskUid"] = json!(task_uid);
    evidence["sourceProvenance"]["run"]["id"] = json!(run_id);
    evidence["sourceProvenance"]["run"]["attempt"] = json!(attempt);
    evidence["envelope"]["revision"] = json!(envelope_revision);
    evidence["envelope"]["digest"] = json!(format!("steward:{envelope_digest}"));
    Ok(evidence)
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
                runtime.metadata.uid = Some(
                    state
                        .next_runtime_uid
                        .lock()
                        .ok()
                        .and_then(|mut runtime_uid| runtime_uid.take())
                        .unwrap_or_else(|| "runtime-uid-a".to_owned()),
                );
                runtime.metadata.resource_version = Some("1".to_owned());
                runtime.metadata.generation = Some(1);
                runtime.metadata.creation_timestamp = Some(
                    match serde_json::from_value(serde_json::json!("2099-01-01T00:00:00Z")) {
                        Ok(timestamp) => timestamp,
                        Err(_) => {
                            return Ok(status_response(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "InternalError",
                            ));
                        }
                    },
                );
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
                desired
                    .metadata
                    .creation_timestamp
                    .clone_from(&current.metadata.creation_timestamp);
                desired.metadata.resource_version = Some("2".to_owned());
                desired.metadata.generation = Some(2);
                desired.status = current.status;
                *stored = Some(desired.clone());
                if state.replace_uid_after_update.swap(false, Ordering::SeqCst) {
                    let replacement = stored.as_mut().unwrap_or_else(|| unreachable!());
                    replacement.metadata.uid = Some("runtime-uid-replacement".to_owned());
                    replacement.metadata.resource_version = Some("3".to_owned());
                }
                json_response(StatusCode::OK, serde_json::to_value(&desired))
            }
        }
        Method::PATCH if path.starts_with(RUNTIME_PATH_PREFIX) => {
            let bytes = match to_bytes(request.into_body(), 1024 * 1024).await {
                Ok(bytes) => bytes,
                Err(_) => return Ok(status_response(StatusCode::BAD_REQUEST, "BadRequest")),
            };
            let patch = match serde_json::from_slice::<serde_json::Value>(&bytes) {
                Ok(patch) => patch,
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
            let Some(runtime) = stored.as_mut() else {
                return Ok(status_response(StatusCode::NOT_FOUND, "NotFound"));
            };
            if path.ends_with("/status") {
                let status = match patch
                    .get("status")
                    .cloned()
                    .map(serde_json::from_value::<AgentRuntimeStatus>)
                {
                    Some(Ok(status)) => status,
                    _ => {
                        return Ok(status_response(
                            StatusCode::BAD_REQUEST,
                            "InvalidStatusPatch",
                        ));
                    }
                };
                runtime.status = Some(status);
                state.status_patches.fetch_add(1, Ordering::SeqCst);
            } else if let Some(operations) = patch.as_array() {
                for operation in operations {
                    let Some(path) = operation.get("path").and_then(serde_json::Value::as_str)
                    else {
                        return Ok(status_response(StatusCode::BAD_REQUEST, "InvalidPatch"));
                    };
                    match (
                        operation.get("op").and_then(serde_json::Value::as_str),
                        path,
                    ) {
                        (Some("test"), "/metadata/finalizers") => {}
                        (Some("test"), path) if path.starts_with("/metadata/finalizers/") => {}
                        (Some("add"), "/metadata/finalizers") => {
                            runtime.metadata.finalizers = operation
                                .get("value")
                                .cloned()
                                .and_then(|value| serde_json::from_value(value).ok());
                        }
                        (Some("add"), "/metadata/finalizers/-") => {
                            let Some(value) =
                                operation.get("value").and_then(serde_json::Value::as_str)
                            else {
                                return Ok(status_response(
                                    StatusCode::BAD_REQUEST,
                                    "InvalidPatch",
                                ));
                            };
                            runtime
                                .metadata
                                .finalizers
                                .get_or_insert_default()
                                .push(value.to_owned());
                        }
                        (Some("remove"), path) if path.starts_with("/metadata/finalizers/") => {
                            let Some(index) = path
                                .rsplit('/')
                                .next()
                                .and_then(|value| value.parse::<usize>().ok())
                            else {
                                return Ok(status_response(
                                    StatusCode::BAD_REQUEST,
                                    "InvalidPatch",
                                ));
                            };
                            let Some(finalizers) = runtime.metadata.finalizers.as_mut() else {
                                return Ok(status_response(StatusCode::CONFLICT, "Conflict"));
                            };
                            if index >= finalizers.len() {
                                return Ok(status_response(StatusCode::CONFLICT, "Conflict"));
                            }
                            finalizers.remove(index);
                        }
                        _ => {
                            return Ok(status_response(StatusCode::BAD_REQUEST, "InvalidPatch"));
                        }
                    }
                }
            } else {
                return Ok(status_response(StatusCode::BAD_REQUEST, "InvalidPatch"));
            }
            runtime.metadata.resource_version = Some(
                runtime
                    .metadata
                    .resource_version
                    .as_deref()
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or_default()
                    .saturating_add(1)
                    .to_string(),
            );
            json_response(StatusCode::OK, serde_json::to_value(runtime))
        }
        Method::GET if path.starts_with(SECRET_PATH_PREFIX) => {
            match state.credential_secret.lock() {
                Ok(secret) => secret.as_ref().map_or_else(
                    || status_response(StatusCode::NOT_FOUND, "NotFound"),
                    |secret| json_response(StatusCode::OK, Ok(secret.clone())),
                ),
                Err(_) => status_response(StatusCode::INTERNAL_SERVER_ERROR, "InternalError"),
            }
        }
        Method::POST if path == SECRET_PATH_PREFIX => {
            let bytes = match to_bytes(request.into_body(), 1024 * 1024).await {
                Ok(bytes) => bytes,
                Err(_) => return Ok(status_response(StatusCode::BAD_REQUEST, "BadRequest")),
            };
            let mut secret = match serde_json::from_slice::<serde_json::Value>(&bytes) {
                Ok(secret) => secret,
                Err(_) => return Ok(status_response(StatusCode::BAD_REQUEST, "BadRequest")),
            };
            let credential_present = secret
                .pointer("/stringData/access-token")
                .and_then(serde_json::Value::as_str)
                .is_some();
            if credential_present {
                secret["data"] = serde_json::json!({
                    "access-token": "Zml4dHVyZS1pbmZlcmVuY2UtY3JlZGVudGlhbA=="
                });
                if let Some(object) = secret.as_object_mut() {
                    object.remove("stringData");
                }
            }
            match state.credential_secret.lock() {
                Ok(mut stored) => {
                    *stored = Some(secret.clone());
                    json_response(StatusCode::CREATED, Ok(secret))
                }
                Err(_) => status_response(StatusCode::INTERNAL_SERVER_ERROR, "InternalError"),
            }
        }
        Method::DELETE if path.starts_with(SECRET_PATH_PREFIX) => {
            match state.credential_secret.lock() {
                Ok(mut stored) => {
                    *stored = None;
                    status_response(StatusCode::OK, "Success")
                }
                Err(_) => status_response(StatusCode::INTERNAL_SERVER_ERROR, "InternalError"),
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
            } else if stored.as_ref().is_some_and(|runtime| {
                runtime
                    .metadata
                    .finalizers
                    .as_ref()
                    .is_some_and(|finalizers| !finalizers.is_empty())
            }) {
                let runtime = stored.as_mut().unwrap_or_else(|| unreachable!());
                if runtime.metadata.deletion_timestamp.is_none() {
                    runtime.metadata.deletion_timestamp = Some(
                        match serde_json::from_value(serde_json::json!("2099-01-01T00:30:00Z")) {
                            Ok(timestamp) => timestamp,
                            Err(_) => {
                                return Ok(status_response(
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    "InternalError",
                                ));
                            }
                        },
                    );
                    runtime.metadata.resource_version = Some(
                        runtime
                            .metadata
                            .resource_version
                            .as_deref()
                            .and_then(|value| value.parse::<u64>().ok())
                            .unwrap_or_default()
                            .saturating_add(1)
                            .to_string(),
                    );
                }
                status_response(StatusCode::OK, "Success")
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
