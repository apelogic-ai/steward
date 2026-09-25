use std::borrow::Cow;
use std::env;
use std::error::Error;
use std::io;

use sqlx::migrate::Migrator;
use sqlx::postgres::PgPoolOptions;
use steward_store::{
    FederatedSubjectAssociation, FederatedSubjectAuditAction, FederatedSubjectDisable,
    FederatedSubjectObservation, FederatedSubjectState, PgStore, StoreError,
};
use steward_types::CanonicalUserId;

fn migration_set(maximum_version: Option<i64>) -> Migrator {
    let embedded = sqlx::migrate!("../migrations");
    Migrator {
        migrations: Cow::Owned(
            embedded
                .migrations
                .iter()
                .filter(|migration| {
                    maximum_version.is_none_or(|maximum| migration.version <= maximum)
                })
                .cloned()
                .collect(),
        ),
        ignore_missing: false,
        locking: false,
        no_tx: false,
    }
}

#[tokio::test]
async fn tls_required_postgres_accepts_store_migrations() -> Result<(), Box<dyn Error>> {
    let plaintext_url = env::var("STEWARD_TEST_PLAINTEXT_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_PLAINTEXT_DATABASE_URL is required for the TLS test")
    })?;
    let plaintext = PgPoolOptions::new()
        .max_connections(1)
        .connect(&plaintext_url)
        .await;
    assert!(
        plaintext.is_err(),
        "the PostgreSQL fixture must reject plaintext connections"
    );

    let tls_url = env::var("STEWARD_TEST_TLS_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_TLS_DATABASE_URL is required for the TLS test")
    })?;
    let verified_tls_url = env::var("STEWARD_TEST_VERIFIED_TLS_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_VERIFIED_TLS_DATABASE_URL is required for the TLS test")
    })?;
    let encrypted_store = PgStore::connect(&tls_url).await.map_err(|error| {
        io::Error::other(format!(
            "Steward must connect when PostgreSQL requires encryption: {error}"
        ))
    })?;
    let encrypted =
        sqlx::query_scalar::<_, bool>("SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()")
            .fetch_one(encrypted_store.pool())
            .await?;
    assert!(
        encrypted,
        "sslmode=require must establish an encrypted session"
    );
    let wrong_ca_url = env::var("STEWARD_TEST_WRONG_CA_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_WRONG_CA_DATABASE_URL is required for the TLS test")
    })?;
    assert!(
        PgStore::connect(&wrong_ca_url).await.is_err(),
        "Steward must fail closed when PostgreSQL presents a certificate from another CA"
    );
    let wrong_hostname_url =
        env::var("STEWARD_TEST_WRONG_HOSTNAME_DATABASE_URL").map_err(|_| {
            io::Error::other(
                "STEWARD_TEST_WRONG_HOSTNAME_DATABASE_URL is required for the TLS test",
            )
        })?;
    assert!(
        PgStore::connect(&wrong_hostname_url).await.is_err(),
        "Steward must fail closed when the PostgreSQL certificate does not cover the hostname"
    );
    let store = PgStore::connect(&verified_tls_url).await.map_err(|error| {
        io::Error::other(format!(
            "Steward must connect when PostgreSQL requires sslmode=verify-full: {error}"
        ))
    })?;
    migration_set(Some(38))
        .run(store.pool())
        .await
        .map_err(|error| {
            io::Error::other(format!(
                "Steward v0.1.23 migrations must complete over the required TLS session: {error}"
            ))
        })?;

    seed_v0123_upgrade_fixture(&store).await?;
    let refused = migration_set(None).run(store.pool()).await;
    assert!(
        refused.is_err_and(|error| error.to_string().contains(
            "v0.2 upgrade refuses unfinished Tasks without one complete immutable authority"
        )),
        "v0.2 must fail closed while an unfinished legacy Task has ambiguous authority"
    );
    sqlx::query(
        "UPDATE task_submissions \
         SET phase = 'cancelled', finalize_requested = true, finalized = true, \
             failure_reason = 'upgrade_precondition' \
         WHERE idempotency_key = 'legacy-unfinished'",
    )
    .execute(store.pool())
    .await?;
    migration_set(Some(39))
        .run(store.pool())
        .await
        .map_err(|error| {
            io::Error::other(format!(
                "Steward v0.2 migrations must complete over the required TLS session: {error}"
            ))
        })?;

    assert_v02_upgrade_result(&store).await?;
    let historical_before_federated_identity = historical_task_identity_snapshot(&store).await?;
    migration_set(None)
        .run(store.pool())
        .await
        .map_err(|error| {
            io::Error::other(format!(
                "Steward federated-subject migration must complete over the required TLS session: {error}"
            ))
        })?;
    assert_eq!(
        historical_task_identity_snapshot(&store).await?,
        historical_before_federated_identity,
        "migration 0040 must not update, backfill, or reinterpret historical Tasks, runs, or canonical identities"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*)::bigint FROM federated_subjects")
            .fetch_one(store.pool())
            .await?,
        0,
        "migration 0040 must not synthesize federated subjects from historical identity data"
    );
    verify_federated_subject_lifecycle(&store).await?;

    let tls_active =
        sqlx::query_scalar::<_, bool>("SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()")
            .fetch_one(store.pool())
            .await?;
    assert!(
        tls_active,
        "the successful Steward database session must be encrypted"
    );

    let applied_migrations =
        sqlx::query_scalar::<_, i64>("SELECT count(*)::bigint FROM _sqlx_migrations")
            .fetch_one(store.pool())
            .await?;
    assert!(
        applied_migrations > 0,
        "the TLS database must contain applied Steward migrations"
    );

    let mut transaction = store.pool().begin().await?;
    sqlx::query(
        "CREATE TEMP TABLE connection_runtime_class_probe \
         (LIKE connection_operations INCLUDING CONSTRAINTS) ON COMMIT DROP",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "DO $probe$ \
         DECLARE column_name text; \
         BEGIN \
           FOR column_name IN \
             SELECT attribute.attname \
             FROM pg_attribute attribute \
             WHERE attribute.attrelid = 'pg_temp.connection_runtime_class_probe'::regclass \
               AND attribute.attnum > 0 \
               AND NOT attribute.attisdropped \
               AND attribute.attname <> 'runtime_class' \
           LOOP \
             EXECUTE format( \
               'ALTER TABLE pg_temp.connection_runtime_class_probe DROP COLUMN %I CASCADE', \
               column_name \
             ); \
           END LOOP; \
         END \
         $probe$",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query("INSERT INTO connection_runtime_class_probe (runtime_class) VALUES ('')")
        .execute(&mut *transaction)
        .await?;
    let whitespace_runtime_class =
        sqlx::query("INSERT INTO connection_runtime_class_probe (runtime_class) VALUES ('   ')")
            .execute(&mut *transaction)
            .await;
    assert!(
        whitespace_runtime_class.is_err(),
        "the connection runtime class must be empty for the cluster default or non-blank"
    );
    transaction.rollback().await?;

    let authority_kind_exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (\
             SELECT 1 FROM information_schema.columns \
             WHERE table_schema = 'public' AND table_name = 'task_submissions' \
               AND column_name = 'authority_kind'\
         )",
    )
    .fetch_one(store.pool())
    .await?;
    assert!(
        authority_kind_exists,
        "v0.2 Task rows must identify their durable user or internal authority kind"
    );

    let user_snapshot_exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (\
             SELECT 1 FROM information_schema.columns \
             WHERE table_schema = 'public' AND table_name = 'task_submissions' \
               AND column_name = 'user_envelope_snapshot'\
         )",
    )
    .fetch_one(store.pool())
    .await?;
    assert!(
        user_snapshot_exists,
        "v0.2 user Tasks must persist the exact approved User Envelope snapshot"
    );

    let new_writer_constraint = sqlx::query_scalar::<_, String>(
        "SELECT pg_get_constraintdef(oid) \
         FROM pg_constraint \
         WHERE conrelid = 'task_submissions'::regclass \
           AND conname = 'task_submissions_new_orchestration_version_required'",
    )
    .fetch_one(store.pool())
    .await?;
    assert!(
        new_writer_constraint.contains("orchestration_version = 3"),
        "new Task writers must use the User-Envelope-only orchestration contract: {new_writer_constraint}"
    );

    Ok(())
}

async fn historical_task_identity_snapshot(
    store: &PgStore,
) -> Result<serde_json::Value, Box<dyn Error>> {
    Ok(sqlx::query_scalar(
        "SELECT jsonb_build_object(\
             'tasks', (\
                 SELECT jsonb_agg(to_jsonb(task_row) ORDER BY task_row.task_uid) \
                 FROM task_submissions task_row\
             ), \
             'runs', (\
                 SELECT jsonb_agg(to_jsonb(event_row) ORDER BY event_row.id) \
                 FROM task_lifecycle_events event_row\
             ), \
             'canonicalUsers', (\
                 SELECT jsonb_agg(to_jsonb(user_row) ORDER BY user_row.user_id) \
                 FROM canonical_users user_row\
             )\
         )",
    )
    .fetch_one(store.pool())
    .await?)
}

async fn verify_federated_subject_lifecycle(store: &PgStore) -> Result<(), Box<dyn Error>> {
    let observation = || FederatedSubjectObservation {
        issuer: "https://identity.example.test",
        subject: "github-actions:actor:16106037",
        actor_login: Some("alice-gh"),
        display_name: Some("Alice"),
    };
    let (first, second) = tokio::join!(
        store.observe_federated_subject(observation()),
        store.observe_federated_subject(observation()),
    );
    let first = first?;
    let second = second?;
    assert_eq!(first.subject_id, second.subject_id);
    assert_eq!(first.state, FederatedSubjectState::Observed);
    assert_eq!(first.revision, 1);
    assert_eq!(
        store
            .federated_subject_audit(first.subject_id)
            .await?
            .iter()
            .map(|event| event.action)
            .collect::<Vec<_>>(),
        [FederatedSubjectAuditAction::Observed],
        "concurrent first observation must converge on one subject and one audit fact"
    );
    assert!(matches!(
        store
            .resolve_federated_subject(&first.issuer, &first.subject)
            .await,
        Err(StoreError::FederatedSubjectUnassociated)
    ));

    let alice = CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")
        .map_err(io::Error::other)?;
    let (seeded_first, seeded_second) = tokio::join!(
        store.seed_federated_subject_association(observation(), &alice, "steward-task-v2"),
        store.seed_federated_subject_association(observation(), &alice, "steward-task-v2"),
    );
    let seeded_first = seeded_first?;
    let seeded_second = seeded_second?;
    assert_eq!(seeded_first.subject_id, seeded_second.subject_id);
    assert_eq!(seeded_first.state, seeded_second.state);
    assert_eq!(
        seeded_first.canonical_user_id,
        seeded_second.canonical_user_id
    );
    assert_eq!(seeded_first.revision, seeded_second.revision);
    assert_eq!(seeded_first.state, FederatedSubjectState::Associated);
    assert_eq!(seeded_first.canonical_user_id.as_ref(), Some(&alice));
    assert_eq!(seeded_first.revision, 2);
    assert_eq!(
        store
            .federated_subject_audit(first.subject_id)
            .await?
            .iter()
            .map(|event| event.action)
            .collect::<Vec<_>>(),
        [
            FederatedSubjectAuditAction::Observed,
            FederatedSubjectAuditAction::V2Seeded,
        ],
        "concurrent v2 seeding must create exactly one association fact"
    );
    let resolved = store
        .resolve_federated_subject(&first.issuer, &first.subject)
        .await?;
    assert_eq!(resolved.user_id, alice);
    assert_eq!(resolved.display_email.as_str(), "alice@example.com");

    let bob = CanonicalUserId::parse("usr_abcdef0123456789abcdef0123456789")
        .map_err(io::Error::other)?;
    sqlx::query(
        "INSERT INTO canonical_users (user_id, organization_id, display_email) \
         VALUES ($1, 'org_example', 'bob@example.org')",
    )
    .bind(bob.as_str())
    .execute(store.pool())
    .await?;
    let replaced = store
        .replace_federated_subject_association(FederatedSubjectAssociation {
            subject_id: first.subject_id,
            expected_revision: 2,
            canonical_user_id: &bob,
            actor: "usr_0123456789abcdef0123456789abcdef",
        })
        .await?;
    assert_eq!(replaced.canonical_user_id.as_ref(), Some(&bob));
    assert_eq!(replaced.revision, 3);
    assert!(matches!(
        store
            .replace_federated_subject_association(FederatedSubjectAssociation {
                subject_id: first.subject_id,
                expected_revision: 2,
                canonical_user_id: &alice,
                actor: "usr_0123456789abcdef0123456789abcdef",
            })
            .await,
        Err(StoreError::FederatedSubjectConflict)
    ));
    let disabled = store
        .disable_federated_subject(FederatedSubjectDisable {
            subject_id: first.subject_id,
            expected_revision: 3,
            actor: "usr_0123456789abcdef0123456789abcdef",
            reason: Some("access revoked"),
        })
        .await?;
    assert_eq!(disabled.state, FederatedSubjectState::Disabled);
    assert_eq!(disabled.revision, 4);
    assert!(matches!(
        store
            .resolve_federated_subject(&first.issuer, &first.subject)
            .await,
        Err(StoreError::FederatedSubjectDisabled)
    ));

    let similarity = store
        .observe_federated_subject(FederatedSubjectObservation {
            issuer: "https://identity.example.test",
            subject: "github-actions:actor:27182818",
            actor_login: Some("alice"),
            display_name: Some("alice@example.com"),
        })
        .await?;
    assert_eq!(similarity.state, FederatedSubjectState::Observed);
    assert!(similarity.canonical_user_id.is_none());
    assert!(matches!(
        store
            .resolve_federated_subject(&similarity.issuer, &similarity.subject)
            .await,
        Err(StoreError::FederatedSubjectUnassociated)
    ));

    assert!(
        sqlx::query(
            "UPDATE federated_subject_audit SET actor = 'tampered' WHERE subject_id = $1",
        )
        .bind(first.subject_id)
        .execute(store.pool())
        .await
        .is_err(),
        "federated-subject audit must reject mutation"
    );
    Ok(())
}

async fn seed_v0123_upgrade_fixture(store: &PgStore) -> Result<(), Box<dyn Error>> {
    let user_id = "usr_0123456789abcdef0123456789abcdef";
    let user_digest = format!("sha256:{}", "b".repeat(64));
    let service_digest = format!("sha256:{}", "c".repeat(64));
    let internal_digest = format!("sha256:{}", "d".repeat(64));
    let workflow_digest = format!("sha256:{}", "e".repeat(64));
    let candidate_digest = format!("sha256:{}", "f".repeat(64));
    let approved_envelope = serde_json::json!({
        "revision": 7,
        "spec": {
            "llms": [{"provider": "provider-a", "model": "model-a"}],
            "tools": [],
            "budget": {"monthlyLimit": "100.00", "currency": "USD"},
            "ttl": "24h",
            "runner": {"platforms": ["linux"], "architectures": []}
        }
    });
    let runtime_spec = serde_json::json!({
        "canonicalAuthority": {
            "schemaVersion": "steward/canonical-authority-binding/v1",
            "ownerUserId": user_id,
            "actingUserId": user_id
        }
    });

    sqlx::query(
        "INSERT INTO canonical_users \
         (user_id, organization_id, display_email) VALUES ($1, 'org_example', 'alice@example.com')",
    )
    .bind(user_id)
    .execute(store.pool())
    .await?;
    sqlx::query(
        "INSERT INTO workflow_revisions \
         (name, version, display_name, agent, prompt, content_digest, published_by) \
         VALUES ('release-summary', 1, 'Release summary', 'agent@1', 'Summarize.', $1, 'admin')",
    )
    .bind(&workflow_digest)
    .execute(store.pool())
    .await?;
    let envelope_request_id = "00000000-0000-0000-0000-000000000100";
    sqlx::query(
        "INSERT INTO envelope_requests \
         (id, owner_user_id, template_id, template_revision, requested_envelope, idempotency_key) \
         VALUES ($1::text::uuid, $2, 'engineer', 7, $3, 'upgrade-envelope')",
    )
    .bind(envelope_request_id)
    .bind(user_id)
    .bind(&approved_envelope)
    .execute(store.pool())
    .await?;
    sqlx::query(
        "INSERT INTO envelope_request_events \
         (request_id, status, envelope_instance_id, envelope_digest, approved_envelope) \
         VALUES ($1::text::uuid, 'provisioned', 'envelope-instance-7', $2, $3)",
    )
    .bind(envelope_request_id)
    .bind(&user_digest)
    .bind(&approved_envelope)
    .execute(store.pool())
    .await?;

    for (task_number, idempotency_key, workflow, finalized, authority) in [
        (1_u128, "legacy-unfinished", "legacy-smoke", false, "legacy"),
        (2, "user-unfinished", "release-summary@1", false, "user"),
        (
            3,
            "internal-unfinished",
            "connections.github.status",
            false,
            "internal",
        ),
        (4, "legacy-terminal", "legacy-history", true, "legacy"),
    ] {
        let task_uid = format!("00000000-0000-0000-0000-{task_number:012}");
        let operation_id = format!("00000000-0000-0000-0000-{:012}", task_number + 1000);
        let mut transaction = store.pool().begin().await?;
        sqlx::query(
            "INSERT INTO task_submissions (\
                 task_uid, idempotency_key, submitter_service, acting_user, owner, \
                 workflow, coding_agent_runtime, runtime_namespace, runtime_name, \
                 runtime_ownership, phase, runtime_spec, agent_command, \
                 finalize_requested, finalized, acting_user_id, owner_user_id, \
                 identity_binding_state, workflow_name, workflow_version, workflow_digest, \
                 user_envelope_instance_id, user_envelope_revision, user_envelope_digest, \
                 internal_authority_id, internal_authority_version, internal_authority_digest, \
                 execution_binding, orchestration_version, orchestration_operation_id, \
                 candidate_digest, service_envelope_digest, original_admission_decision, \
                 original_admission_deltas\
             ) VALUES (\
                 $1::text::uuid, $2, 'steward-run', 'alice@example.com', 'alice@example.com', \
                 $3, 'agent@1', 'steward-workflows', $4, 'provisioned', $5, $6, \
                 '[\"agent\"]'::jsonb, $7, $7, $8, $8, 'bound', \
                 $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, 2, $19::text::uuid, \
                 $20, $21, 'admit', '[]'::jsonb\
             )",
        )
        .bind(&task_uid)
        .bind(idempotency_key)
        .bind(workflow)
        .bind(format!("task-{task_number}"))
        .bind(if finalized { "succeeded" } else { "submitted" })
        .bind(&runtime_spec)
        .bind(finalized)
        .bind(user_id)
        .bind((authority == "user").then_some("release-summary"))
        .bind((authority == "user").then_some(1_i64))
        .bind((authority == "user").then_some(workflow_digest.as_str()))
        .bind((authority == "user").then_some("envelope-instance-7"))
        .bind((authority == "user").then_some(7_i64))
        .bind((authority == "user").then_some(user_digest.as_str()))
        .bind((authority == "internal").then_some("connections.github.status"))
        .bind((authority == "internal").then_some(1_i64))
        .bind((authority == "internal").then_some(internal_digest.as_str()))
        .bind((authority == "user").then_some(serde_json::json!({})))
        .bind(&operation_id)
        .bind(&candidate_digest)
        .bind(&service_digest)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "INSERT INTO task_runtime_operations (\
                 task_uid, operation_id, state, runtime_ownership, runtime_namespace, \
                 runtime_name, inert_manifest_digest, active_manifest_digest\
             ) VALUES (\
                 $1::text::uuid, $2::text::uuid, 'intent_recorded', 'provisioned', \
                 'steward-workflows', $3, $4, $4\
             )",
        )
        .bind(&task_uid)
        .bind(&operation_id)
        .bind(format!("task-{task_number}"))
        .bind(&candidate_digest)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
    }
    Ok(())
}

async fn assert_v02_upgrade_result(store: &PgStore) -> Result<(), Box<dyn Error>> {
    let user = sqlx::query_as::<
        _,
        (
            i16,
            Option<String>,
            Option<serde_json::Value>,
            Option<String>,
        ),
    >(
        "SELECT orchestration_version, authority_kind, user_envelope_snapshot, \
                service_envelope_digest \
         FROM task_submissions WHERE idempotency_key = 'user-unfinished'",
    )
    .fetch_one(store.pool())
    .await?;
    assert_eq!(user.0, 3);
    assert_eq!(user.1.as_deref(), Some("user-envelope"));
    assert_eq!(
        user.2
            .and_then(|snapshot| snapshot.get("revision").cloned()),
        Some(7.into())
    );
    assert!(user.3.is_none());

    let internal = sqlx::query_as::<_, (i16, Option<String>, Option<String>)>(
        "SELECT orchestration_version, authority_kind, service_envelope_digest \
         FROM task_submissions WHERE idempotency_key = 'internal-unfinished'",
    )
    .fetch_one(store.pool())
    .await?;
    assert_eq!(internal.0, 3);
    assert_eq!(internal.1.as_deref(), Some("internal"));
    assert!(internal.2.is_none());

    let historical = sqlx::query_as::<_, (i16, Option<String>, bool)>(
        "SELECT orchestration_version, authority_kind, finalized \
         FROM task_submissions WHERE idempotency_key = 'legacy-terminal'",
    )
    .fetch_one(store.pool())
    .await?;
    assert_eq!(historical, (2, None, true));
    Ok(())
}
