use std::env;
use std::error::Error;
use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use sqlx::Row;
use sqlx::postgres::PgPoolOptions;
use steward_admission::AdmissionDelta;
use steward_store::{ApproveAdmission, ParkRejection, PgStore, StoreError};
use steward_types::{AgentRuntimeSpec, AgentType, Budget, Duration, Email, ModelRef, Principal};

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
         (id, runtime_uid, spec_digest, envelope_rev, verdict, deltas, proposed_spec, actor, member_role) \
         VALUES ($1::uuid, $2, 'digest-a', 1, 'reject', '[]'::jsonb, '{}'::jsonb, \
                 'alice@example.com', 'engineer')",
    )
    .bind(&decision_id)
    .bind(&runtime_a)
    .execute(store.pool())
    .await?;
    sqlx::query(
        "INSERT INTO approvals \
         (id, runtime_uid, admission_decision_id, state, jira_key) \
         VALUES ($1::uuid, $2, $3::uuid, 'pending', 'PROJ-123')",
    )
    .bind(&approval_id)
    .bind(&runtime_a)
    .bind(&decision_id)
    .execute(store.pool())
    .await?;

    let inserted = sqlx::query(
        "INSERT INTO grants \
         (id, runtime_uid, dimension, granted_value, approval_id, expires_at) \
         VALUES ($1::uuid, $2, 'budget', \
                 '{\"requested\":\"220.00\",\"currency\":\"USD\"}'::jsonb, \
                 $3::uuid, NULL)",
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
    let parked = store
        .park_rejection(ParkRejection {
            runtime_uid: "runtime-evidence-a",
            spec_digest: "digest-evidence-a",
            envelope_revision: 1,
            deltas: &deltas,
            proposed_spec: &proposed_spec,
            actor: "alice@example.com",
            member_role: "engineer",
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
    assert_eq!(approved.member_role, "engineer");
    assert_eq!(approved.jira_key, "PROJ-123");
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
        store.grants_for_runtime("runtime-evidence-a").await?,
        deltas,
        "the approved runtime must read back its structured grant"
    );
    assert!(
        store
            .grants_for_runtime("runtime-evidence-b")
            .await?
            .is_empty(),
        "a second runtime must never inherit the first runtime's grant"
    );
    let retried = store
        .approve_admission(ApproveAdmission {
            approval_id: parked.approval_id,
            decided_by: "admin@example.com",
            rationale: "approved for this runtime",
            evidence_url: "https://jira.example.com/browse/PROJ-123",
        })
        .await
        .map_err(|error| {
            io::Error::other(format!(
                "an identical approval retry must be idempotent: {error}"
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
    Ok(())
}
