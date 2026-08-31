use std::env;
use std::error::Error;
use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, Executor, PgConnection};
use steward_admission::{AdmissionDelta, Envelope, EnvelopeSpec};
use steward_store::{
    EnvelopeRequestReservationRequest, EnvelopeRequestStatus, EnvelopeRequestStatusUpdate,
    StoreError,
};
use steward_store::{ParkRejection, PgStore, WorkflowPublication};
use steward_types::{
    AgentRuntimeSpec, AgentType, Budget, Duration, Email, ModelRef, OrganizationId,
    OrganizationIdentityPolicy, Principal, RunnerRequirements,
};

#[tokio::test]
async fn s3_postgres_migrations_apply_from_empty() -> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the S3 Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);

    store.migrate().await.map_err(|error| {
        io::Error::other(format!(
            "S3 migrations must apply cleanly to an empty Postgres database: {error}"
        ))
    })?;
    Ok(())
}

#[tokio::test]
async fn envelope_event_audit_migration_upgrades_history_and_accepts_old_writers()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the S3 Postgres test")
    })?;
    let mut connection = PgConnection::connect(&database_url).await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let schema = format!("migration_0020_{suffix}");
    connection
        .execute(format!("CREATE SCHEMA {schema}; SET search_path TO {schema}").as_str())
        .await?;
    connection
        .execute(
            "CREATE FUNCTION steward_reject_history_mutation() RETURNS trigger \
             LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'append-only' USING ERRCODE = '55000'; END $$; \
             CREATE TABLE envelope_requests (id uuid PRIMARY KEY, template_revision bigint NOT NULL); \
             CREATE TABLE envelope_request_events (\
                 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, \
                 request_id uuid NOT NULL REFERENCES envelope_requests(id), \
                 status text NOT NULL\
             ); \
             CREATE TRIGGER envelope_request_events_are_append_only \
             BEFORE UPDATE OR DELETE ON envelope_request_events \
             FOR EACH ROW EXECUTE FUNCTION steward_reject_history_mutation(); \
             INSERT INTO envelope_requests (id, template_revision) \
             VALUES ('00000000-0000-4000-8000-000000000020', 7); \
             INSERT INTO envelope_request_events (request_id, status) \
             VALUES ('00000000-0000-4000-8000-000000000020', 'pending');",
        )
        .await?;

    sqlx::raw_sql(include_str!(
        "../migrations/0020_envelope_request_transition_audit.sql"
    ))
    .execute(&mut connection)
    .await?;

    let migrated: (String, i64) =
        sqlx::query_as("SELECT actor, template_revision FROM envelope_request_events WHERE id = 1")
            .fetch_one(&mut connection)
            .await?;
    assert_eq!(migrated, ("steward-system/legacy-migration".to_owned(), 7));

    sqlx::query(
        "INSERT INTO envelope_request_events (request_id, status) \
         VALUES ('00000000-0000-4000-8000-000000000020', 'pending')",
    )
    .execute(&mut connection)
    .await?;
    let old_writer: (String, i64) =
        sqlx::query_as("SELECT actor, template_revision FROM envelope_request_events WHERE id = 2")
            .fetch_one(&mut connection)
            .await?;
    assert_eq!(old_writer, ("steward-system/legacy-writer".to_owned(), 7));

    let mutation =
        sqlx::query("UPDATE envelope_request_events SET status = 'rejected' WHERE id = 1")
            .execute(&mut connection)
            .await;
    assert!(
        mutation.is_err(),
        "migration must restore append-only enforcement"
    );
    connection
        .execute(format!("DROP SCHEMA {schema} CASCADE").as_str())
        .await?;
    Ok(())
}

#[tokio::test]
async fn workflow_revisions_are_immutable_and_task_pins_are_atomic() -> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the S3 Postgres test")
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
    let workflow_name = format!("repository-review-{suffix}");
    let first_digest = format!("sha256:{}", "a".repeat(64));
    let second_digest = format!("sha256:{}", "b".repeat(64));
    let first = store
        .publish_initial_workflow(WorkflowPublication {
            name: &workflow_name,
            display_name: "Repository review",
            agent: "codex@0.117.0",
            prompt: "Review the repository state that triggered this GitHub Actions run.",
            content_digest: &first_digest,
            published_by: "admin@example.com",
        })
        .await?;
    let second = store
        .publish_next_workflow(WorkflowPublication {
            name: &workflow_name,
            display_name: "Repository review",
            agent: "codex@0.117.0",
            prompt: "Review the repository state and summarize actionable findings.",
            content_digest: &second_digest,
            published_by: "admin@example.com",
        })
        .await?;
    assert_eq!(first.version, 1);
    assert_eq!(second.version, 2);

    let mutation = sqlx::query(
        "UPDATE workflow_revisions SET prompt = 'mutated' WHERE name = $1 AND version = 1",
    )
    .bind(&workflow_name)
    .execute(store.pool())
    .await;
    assert!(
        mutation.is_err(),
        "a published Workflow revision must reject in-place mutation"
    );
    let deletion = sqlx::query("DELETE FROM workflow_revisions WHERE name = $1 AND version = 1")
        .bind(&workflow_name)
        .execute(store.pool())
        .await;
    assert!(
        deletion.is_err(),
        "a published Workflow revision must reject deletion"
    );
    assert_eq!(
        store.workflow_revision(&workflow_name, 1).await?,
        Some(first.clone()),
        "publishing version 2 must leave version 1 unchanged"
    );

    let partial_pins = sqlx::query(
        "INSERT INTO task_submissions \
         (task_uid, idempotency_key, submitter_service, owner, workflow, workflow_name, \
          coding_agent_runtime, runtime_namespace, runtime_name, runtime_ownership, phase, \
          runtime_spec, agent_command) \
         VALUES (gen_random_uuid(), $1, 'steward-run', \
                 'alice@example.com', $2, $3, 'codex@0.117.0', 'steward-test', 'task-a', \
                 'provisioned', 'submitted', '{}'::jsonb, '[]'::jsonb)",
    )
    .bind(format!("partial-{suffix}"))
    .bind(format!("{workflow_name}@1"))
    .bind(&workflow_name)
    .execute(store.pool())
    .await;
    assert!(
        partial_pins.is_err(),
        "a Task must not persist a partial Workflow or User Envelope pin set"
    );

    let complete_pins = sqlx::query(
        "INSERT INTO task_submissions \
         (task_uid, idempotency_key, submitter_service, owner, workflow, workflow_name, \
          workflow_version, workflow_digest, user_envelope_instance_id, \
          user_envelope_revision, user_envelope_digest, coding_agent_runtime, \
          runtime_namespace, runtime_name, runtime_ownership, phase, runtime_spec, agent_command) \
         VALUES (gen_random_uuid(), $1, 'steward-run', \
                 'alice@example.com', $2, $3, 1, $4, 'env_test', 7, $5, 'codex@0.117.0', \
                 'steward-test', 'task-b', 'provisioned', 'submitted', '{}'::jsonb, '[]'::jsonb)",
    )
    .bind(format!("complete-{suffix}"))
    .bind(format!("{workflow_name}@1"))
    .bind(&workflow_name)
    .bind(&first_digest)
    .bind(format!("sha256:{}", "c".repeat(64)))
    .execute(store.pool())
    .await?;
    assert_eq!(complete_pins.rows_affected(), 1);

    Ok(())
}

#[tokio::test]
async fn s3_postgres_keeps_envelopes_immutable_and_parks_exact_rejections()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the S3 Postgres test")
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
    let member_role = format!("engineer-{suffix}");
    let runtime_uid = format!("runtime-{suffix}");
    let envelope = Envelope {
        revision: 1,
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
            runner: RunnerRequirements::default(),
        },
    };
    store
        .insert_envelope(&member_role, &envelope, "admin@example.com")
        .await
        .map_err(|error| {
            io::Error::other(format!(
                "an authored member-role envelope must be persisted: {error}"
            ))
        })?;
    assert_eq!(
        store.latest_envelope(&member_role).await?,
        Some(envelope.clone()),
        "latest envelope lookup must return the immutable authored revision"
    );

    let mutation = sqlx::query(
        "UPDATE envelopes SET authored_by = 'other@example.com' \
         WHERE scope_kind = 'member_role' AND scope_ref = $1 AND revision = 1",
    )
    .bind(&member_role)
    .execute(store.pool())
    .await;
    assert!(
        mutation.is_err(),
        "the database must reject mutation of an authored envelope revision"
    );

    let proposed_spec = AgentRuntimeSpec {
        principal: Principal::User {
            acting_user: Email("alice@example.com".to_owned()),
        },
        owner: Email("alice@example.com".to_owned()),
        canonical_authority: None,
        agent_type: AgentType {
            name: "base".to_owned(),
        },
        llms: envelope.spec.llms.clone(),
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
    let parked = store
        .park_rejection(ParkRejection {
            runtime_uid: &runtime_uid,
            runtime_namespace: "team-a",
            runtime_name: "runtime-a",
            spec_digest: "digest-a",
            base_spec_digest: "base-digest-a",
            base_pending_approval_digest: None,
            base_spec: &base_spec,
            envelope_revision: envelope.revision,
            deltas: &deltas,
            proposed_spec: &proposed_spec,
            actor: "alice@example.com",
            member_role: &member_role,
        })
        .await
        .map_err(|error| {
            io::Error::other(format!(
                "a rejected manifest and its counterexample must park atomically: {error}"
            ))
        })?;
    let rebound = sqlx::query("UPDATE approvals SET runtime_uid = 'runtime-other' WHERE id = $1")
        .bind(parked.approval_id)
        .execute(store.pool())
        .await;
    assert!(
        rebound.is_err(),
        "an approval must never be rebound to another runtime UID"
    );
    let other_runtime_uid = format!("runtime-other-{suffix}");
    let other = store
        .park_rejection(ParkRejection {
            runtime_uid: &other_runtime_uid,
            runtime_namespace: "team-a",
            runtime_name: "runtime-b",
            spec_digest: "digest-b",
            base_spec_digest: "base-digest-b",
            base_pending_approval_digest: None,
            base_spec: &base_spec,
            envelope_revision: envelope.revision,
            deltas: &deltas,
            proposed_spec: &proposed_spec,
            actor: "bob@example.org",
            member_role: &member_role,
        })
        .await?;
    let rebound = sqlx::query("UPDATE approvals SET admission_decision_id = $1 WHERE id = $2")
        .bind(other.decision_id)
        .bind(parked.approval_id)
        .execute(store.pool())
        .await;
    assert!(
        rebound.is_err(),
        "an approval must never be rebound to another admission decision"
    );
    let queue = store.pending_approvals().await?;
    let row = queue
        .iter()
        .find(|row| row.approval_id == parked.approval_id)
        .ok_or_else(|| io::Error::other("parked rejection is missing from the approval queue"))?;
    assert_eq!(row.decision_id, parked.decision_id);
    assert_eq!(row.runtime_uid, runtime_uid);
    assert_eq!(row.deltas, deltas);
    assert_eq!(row.proposed_spec, proposed_spec);
    assert_eq!(row.actor, "alice@example.com");
    assert_eq!(row.member_role, member_role);
    Ok(())
}

#[tokio::test]
async fn envelope_requests_are_idempotent_audited_and_bound_to_the_exact_template_revision()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the S3 Postgres test")
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
    let member_role = format!("analyst-{suffix}");
    let email = Email::parse(format!("envelope-{suffix}@example.com"))?;
    let subject = format!("envelope-subject-{suffix}");
    let identity = OrganizationIdentityPolicy::new(
        "https://accounts.google.com",
        "example.com",
        OrganizationId::parse("org_example")?,
    )?
    .validate(
        "https://accounts.google.com",
        &subject,
        "example.com",
        email.as_str(),
        true,
    )?;
    let principal = store
        .register_canonical_identity(&identity, "identity-admin")
        .await?;
    let template = Envelope {
        revision: 1,
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
            runner: RunnerRequirements::default(),
        },
    };
    store
        .insert_envelope(&member_role, &template, "admin@example.com")
        .await?;

    let idempotency_key = format!("envelope-request-{suffix}");
    let reservation = store
        .reserve_envelope_request(EnvelopeRequestReservationRequest {
            owner_user_id: &principal.user_id,
            template_id: &member_role,
            template_revision: template.revision,
            requested_envelope: &template,
            idempotency_key: &idempotency_key,
            actor: principal.user_id.as_str(),
        })
        .await?;
    assert!(reservation.inserted);
    assert_eq!(reservation.record.status, EnvelopeRequestStatus::Pending);
    assert_eq!(reservation.record.status_actor, principal.user_id.as_str());
    assert_eq!(
        reservation.record.status_template_revision,
        template.revision
    );

    let repeated = store
        .reserve_envelope_request(EnvelopeRequestReservationRequest {
            owner_user_id: &principal.user_id,
            template_id: &member_role,
            template_revision: template.revision,
            requested_envelope: &template,
            idempotency_key: &idempotency_key,
            actor: principal.user_id.as_str(),
        })
        .await?;
    assert!(!repeated.inserted);
    assert_eq!(repeated.record.id, reservation.record.id);

    let instance_id = format!("env_{suffix}");
    let digest = format!("sha256:{}", "a".repeat(64));
    let approval_id = "00000000-0000-0000-0000-000000000031".parse()?;
    let provisioned = store
        .append_envelope_request_status(
            reservation.record.id,
            EnvelopeRequestStatusUpdate {
                from: EnvelopeRequestStatus::Pending,
                to: EnvelopeRequestStatus::Provisioned,
                approval_id: Some(approval_id),
                envelope_instance_id: Some(&instance_id),
                envelope_digest: Some(&digest),
                reason: None,
                approved_envelope: Some(&template),
                actor: "usr_abcdef0123456789abcdef0123456789",
            },
        )
        .await?;
    assert_eq!(provisioned.status, EnvelopeRequestStatus::Provisioned);
    assert_eq!(provisioned.approved_envelope, Some(template.clone()));
    assert_eq!(
        provisioned.envelope_instance_id.as_deref(),
        Some(instance_id.as_str())
    );
    assert_eq!(
        provisioned.status_actor,
        "usr_abcdef0123456789abcdef0123456789"
    );
    assert_eq!(provisioned.status_template_revision, template.revision);

    let next_template = Envelope {
        revision: 2,
        ..template.clone()
    };
    store
        .insert_envelope(&member_role, &next_template, "admin@example.com")
        .await?;
    let repeated_after_revision = store
        .append_envelope_request_status(
            reservation.record.id,
            EnvelopeRequestStatusUpdate {
                from: EnvelopeRequestStatus::Pending,
                to: EnvelopeRequestStatus::Provisioned,
                approval_id: Some(approval_id),
                envelope_instance_id: Some(&instance_id),
                envelope_digest: Some(&digest),
                reason: None,
                approved_envelope: Some(&template),
                actor: "usr_abcdef0123456789abcdef0123456789",
            },
        )
        .await?;
    assert_eq!(repeated_after_revision, provisioned);

    let replacement_key = format!("replacement-envelope-request-{suffix}");
    let replacement = store
        .reserve_envelope_request(EnvelopeRequestReservationRequest {
            owner_user_id: &principal.user_id,
            template_id: &member_role,
            template_revision: next_template.revision,
            requested_envelope: &next_template,
            idempotency_key: &replacement_key,
            actor: principal.user_id.as_str(),
        })
        .await?;
    let replacement_instance_id = format!("env_replacement_{suffix}");
    store
        .append_envelope_request_status(
            replacement.record.id,
            EnvelopeRequestStatusUpdate {
                from: EnvelopeRequestStatus::Pending,
                to: EnvelopeRequestStatus::Provisioned,
                approval_id: None,
                envelope_instance_id: Some(&replacement_instance_id),
                envelope_digest: Some(&digest),
                reason: None,
                approved_envelope: Some(&next_template),
                actor: principal.user_id.as_str(),
            },
        )
        .await?;
    let owner_requests = store.envelope_requests(&principal.user_id).await?;
    let active: Vec<_> = owner_requests
        .iter()
        .filter(|request| request.status == EnvelopeRequestStatus::Provisioned)
        .collect();
    assert_eq!(
        active.len(),
        1,
        "provisioning must preserve one active envelope"
    );
    assert_eq!(active[0].id, replacement.record.id);
    assert_eq!(
        owner_requests
            .iter()
            .find(|request| request.id == reservation.record.id)
            .map(|request| request.status),
        Some(EnvelopeRequestStatus::Stale),
        "the prior active envelope must be superseded by an append-only event"
    );

    let concurrent_a = store
        .reserve_envelope_request(EnvelopeRequestReservationRequest {
            owner_user_id: &principal.user_id,
            template_id: &member_role,
            template_revision: next_template.revision,
            requested_envelope: &next_template,
            idempotency_key: &format!("concurrent-a-{suffix}"),
            actor: principal.user_id.as_str(),
        })
        .await?;
    let concurrent_b = store
        .reserve_envelope_request(EnvelopeRequestReservationRequest {
            owner_user_id: &principal.user_id,
            template_id: &member_role,
            template_revision: next_template.revision,
            requested_envelope: &next_template,
            idempotency_key: &format!("concurrent-b-{suffix}"),
            actor: principal.user_id.as_str(),
        })
        .await?;
    let concurrent_a_id = format!("env_concurrent_a_{suffix}");
    let concurrent_b_id = format!("env_concurrent_b_{suffix}");
    let (a_result, b_result) = tokio::join!(
        store.append_envelope_request_status(
            concurrent_a.record.id,
            EnvelopeRequestStatusUpdate {
                from: EnvelopeRequestStatus::Pending,
                to: EnvelopeRequestStatus::Provisioned,
                approval_id: None,
                envelope_instance_id: Some(&concurrent_a_id),
                envelope_digest: Some(&digest),
                reason: None,
                approved_envelope: Some(&next_template),
                actor: principal.user_id.as_str(),
            },
        ),
        store.append_envelope_request_status(
            concurrent_b.record.id,
            EnvelopeRequestStatusUpdate {
                from: EnvelopeRequestStatus::Pending,
                to: EnvelopeRequestStatus::Provisioned,
                approval_id: None,
                envelope_instance_id: Some(&concurrent_b_id),
                envelope_digest: Some(&digest),
                reason: None,
                approved_envelope: Some(&next_template),
                actor: principal.user_id.as_str(),
            },
        )
    );
    a_result?;
    b_result?;
    let after_concurrent = store.envelope_requests(&principal.user_id).await?;
    let active_after_concurrent: Vec<_> = after_concurrent
        .iter()
        .filter(|request| request.status == EnvelopeRequestStatus::Provisioned)
        .collect();
    assert_eq!(
        active_after_concurrent.len(),
        1,
        "concurrent provisioning must serialize to one active envelope"
    );
    assert!(
        [concurrent_a.record.id, concurrent_b.record.id].contains(&active_after_concurrent[0].id)
    );

    let stale_key = format!("stale-envelope-request-{suffix}");
    let stale = store
        .reserve_envelope_request(EnvelopeRequestReservationRequest {
            owner_user_id: &principal.user_id,
            template_id: &member_role,
            template_revision: template.revision,
            requested_envelope: &template,
            idempotency_key: &stale_key,
            actor: principal.user_id.as_str(),
        })
        .await?;
    assert_eq!(
        store
            .append_envelope_request_status(
                stale.record.id,
                EnvelopeRequestStatusUpdate {
                    from: EnvelopeRequestStatus::Pending,
                    to: EnvelopeRequestStatus::Provisioned,
                    approval_id: Some("00000000-0000-0000-0000-000000000032".parse()?),
                    envelope_instance_id: Some("env_stale"),
                    envelope_digest: Some(&digest),
                    reason: None,
                    approved_envelope: Some(&template),
                    actor: "usr_abcdef0123456789abcdef0123456789",
                },
            )
            .await,
        Err(StoreError::EnvelopeRequestTemplateStale)
    );

    let request_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM envelope_requests WHERE owner_user_id = $1 AND idempotency_key = $2",
    )
    .bind(principal.user_id.as_str())
    .bind(&idempotency_key)
    .fetch_one(store.pool())
    .await?;
    let event_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM envelope_request_events WHERE request_id = $1")
            .bind(reservation.record.id)
            .fetch_one(store.pool())
            .await?;
    assert_eq!(
        request_count, 1,
        "request retries must reserve one immutable request"
    );
    assert_eq!(
        event_count, 3,
        "request, provisioning, and later supersession must remain append-only"
    );
    Ok(())
}
