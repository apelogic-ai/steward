use std::env;
use std::error::Error;
use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use sqlx::Row;
use sqlx::postgres::PgPoolOptions;
use steward_admission::{AdmissionDelta, Envelope, EnvelopeScopeKind, EnvelopeSpec};
use steward_store::{
    AgentRunQuery, AgentRunTimelineKind, AgentRunTimelineProvenance, ApproveAdmission,
    ParkRejection, PgStore, StoreError, TaskReservationRequest,
};
use steward_types::{
    AgentRuntimeSpec, AgentType, Budget, Duration, Email, ModelRef, Principal, RuntimeOwnership,
    SpendSummary, TaskPhase,
};

fn proposed_spec() -> AgentRuntimeSpec {
    AgentRuntimeSpec {
        principal: Principal::User {
            acting_user: Email("alice@example.com".to_owned()),
        },
        owner: Email("alice@example.com".to_owned()),
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
            currency: "USD".to_owned(),
        },
        ttl: Duration("24h".to_owned()),
        bindings: None,
    }
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
    let runtime_name = format!("task-{suffix}");
    let runtime_uid = format!("runtime-{suffix}");
    let spec = proposed_spec();
    let command = vec!["agent-v1".to_owned()];
    let request = TaskReservationRequest {
        idempotency_key: &idempotency_key,
        submitter_service: "steward-run",
        acting_user: Some("alice@example.com"),
        owner: "alice@example.com",
        workflow: "code-review",
        coding_agent_runtime: "agent-v1",
        runtime_namespace: "team-a",
        runtime_name: &runtime_name,
        runtime_ownership: RuntimeOwnership::Provisioned,
        runtime_spec: &spec,
        agent_command: &command,
        envelope_revision: 1,
    };
    let first = store.reserve_task(&request).await?;
    assert!(first.inserted);
    let second = store.reserve_task(&request).await?;
    assert!(!second.inserted);
    assert_eq!(second.record.task_uid, first.record.task_uid);

    store
        .bind_task_runtime(first.record.task_uid, &runtime_uid, TaskPhase::Submitted)
        .await?;
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
    let lifecycle_before_inputs = store
        .agent_run(first.record.task_uid)
        .await?
        .ok_or_else(|| io::Error::other("bound task disappeared from Agent Runs"))?;
    sqlx::query("SELECT pg_sleep(0.002)")
        .execute(store.pool())
        .await?;
    let archive = b"neutral-tar-fixture";
    store
        .put_task_inputs(
            first.record.task_uid,
            "steward-run",
            Some("alice@example.com"),
            archive,
        )
        .await?;
    let after_inputs = store
        .agent_run(first.record.task_uid)
        .await?
        .ok_or_else(|| io::Error::other("input-bearing task disappeared from Agent Runs"))?;
    assert_ne!(
        after_inputs.updated_at, lifecycle_before_inputs.updated_at,
        "put_task_inputs must advance current Task freshness"
    );
    assert_eq!(
        after_inputs.lifecycle_observed_at, lifecycle_before_inputs.lifecycle_observed_at,
        "put_task_inputs must not advance append-only lifecycle freshness"
    );
    store
        .request_task_execution(
            first.record.task_uid,
            "steward-run",
            Some("alice@example.com"),
        )
        .await?;
    assert!(store.claim_task_execution(first.record.task_uid).await?);
    assert!(
        !store.claim_task_execution(first.record.task_uid).await?,
        "a task execution must have only one durable winner"
    );
    store
        .complete_task_execution(first.record.task_uid, b"neutral-output-tar")
        .await?;
    let completed = store
        .task(first.record.task_uid)
        .await?
        .ok_or_else(|| io::Error::other("completed task disappeared"))?;
    assert_eq!(completed.phase, TaskPhase::Succeeded);
    assert_eq!(
        completed.output_archive.as_deref(),
        Some(b"neutral-output-tar".as_slice())
    );
    store
        .request_task_finalization(
            first.record.task_uid,
            "steward-run",
            Some("alice@example.com"),
        )
        .await?;
    store.mark_task_finalized(first.record.task_uid).await?;
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

#[tokio::test]
async fn agent_runs_filtered_pagination_is_stable_under_concurrent_updates()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the Agent Runs Postgres test")
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
    let workflow = format!("filtered-workflow-{suffix}");
    let spec = proposed_spec();
    let command = vec!["agent-v1".to_owned()];
    let mut matching = Vec::new();

    for index in 0..4 {
        let idempotency_key = format!("pagination-{suffix}-{index}");
        let runtime_name = format!("pagination-task-{suffix}-{index}");
        let runtime_uid = format!("pagination-runtime-{suffix}-{index}");
        let task_workflow = if index == 3 {
            format!("other-workflow-{suffix}")
        } else {
            workflow.clone()
        };
        let reservation = store
            .reserve_task(&TaskReservationRequest {
                idempotency_key: &idempotency_key,
                submitter_service: "steward-run",
                acting_user: Some("alice@example.com"),
                owner: "alice@example.com",
                workflow: &task_workflow,
                coding_agent_runtime: "agent-v1",
                runtime_namespace: "team-a",
                runtime_name: &runtime_name,
                runtime_ownership: RuntimeOwnership::Provisioned,
                runtime_spec: &spec,
                agent_command: &command,
                envelope_revision: 1,
            })
            .await?;
        store
            .bind_task_runtime(
                reservation.record.task_uid,
                &runtime_uid,
                TaskPhase::Running,
            )
            .await?;
        let created_at = match index {
            0 | 1 => "2030-01-02T00:00:00Z",
            2 => "2030-01-01T00:00:00Z",
            _ => "2030-01-03T00:00:00Z",
        };
        sqlx::query("UPDATE task_submissions SET created_at = $2::timestamptz WHERE task_uid = $1")
            .bind(reservation.record.task_uid)
            .bind(created_at)
            .execute(store.pool())
            .await?;
        if index < 3 {
            matching.push((reservation.record.task_uid, runtime_uid));
        }
    }
    let oldest_uid = matching[2].0;
    matching.sort_by(|left, right| {
        let left_boundary = if left.0 == oldest_uid { 0 } else { 1 };
        let right_boundary = if right.0 == oldest_uid { 0 } else { 1 };
        right_boundary
            .cmp(&left_boundary)
            .then_with(|| right.0.cmp(&left.0))
    });

    let query = |cursor| AgentRunQuery {
        limit: 1,
        cursor,
        phase: Some(TaskPhase::Running),
        workflow: Some(workflow.clone()),
    };
    let first_page = store.agent_runs(&query(None)).await?;
    assert_eq!(first_page.records.len(), 1);
    assert_eq!(first_page.records[0].task_uid, matching[0].0);
    assert_eq!(first_page.next_cursor, Some(matching[0].0));

    sqlx::query(
        "UPDATE task_submissions SET phase = 'succeeded', updated_at = now() WHERE task_uid = $1",
    )
    .bind(matching[0].0)
    .execute(store.pool())
    .await?;
    for amount in ["1.00", "2.00"] {
        sqlx::query(
            "INSERT INTO spend_observations \
             (runtime_uid, observed_amount, currency, exhausted, at) \
             VALUES ($1, $2::numeric, 'USD', false, '2030-01-04T00:00:00Z'::timestamptz)",
        )
        .bind(&matching[1].1)
        .bind(amount)
        .execute(store.pool())
        .await?;
    }

    let second_page = store.agent_runs(&query(first_page.next_cursor)).await?;
    assert_eq!(second_page.records.len(), 1);
    assert_eq!(second_page.records[0].task_uid, matching[1].0);
    assert_eq!(second_page.next_cursor, Some(matching[1].0));
    assert_eq!(
        second_page.records[0]
            .spend
            .as_ref()
            .map(|spend| spend.observed_amount.as_str()),
        Some("2.00"),
        "equal-timestamp spend observations must use the highest append-only id"
    );

    sqlx::query(
        "UPDATE task_submissions SET phase = 'succeeded', updated_at = now() WHERE task_uid = $1",
    )
    .bind(matching[1].0)
    .execute(store.pool())
    .await?;
    let third_page = store.agent_runs(&query(second_page.next_cursor)).await?;
    assert_eq!(third_page.records.len(), 1);
    assert_eq!(third_page.records[0].task_uid, matching[2].0);
    assert!(third_page.next_cursor.is_none());
    assert_eq!(
        vec![
            first_page.records[0].task_uid,
            second_page.records[0].task_uid,
            third_page.records[0].task_uid,
        ],
        matching.iter().map(|entry| entry.0).collect::<Vec<_>>(),
        "phase and spend updates must not duplicate or reorder the immutable cursor walk"
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
                currency: spec.budget.currency,
            },
            ttl: spec.ttl,
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
            currency: "USD".to_owned(),
        },
        ttl: Duration("24h".to_owned()),
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
