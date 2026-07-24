use std::env;
use std::error::Error;
use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use sqlx::postgres::PgPoolOptions;
use steward_admission::{AdmissionDelta, Envelope, EnvelopeSpec};
use steward_store::{ParkRejection, PgStore};
use steward_types::{AgentRuntimeSpec, AgentType, Budget, Duration, Email, ModelRef, Principal};

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
                currency: "USD".to_owned(),
            },
            ttl: Duration("24h".to_owned()),
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
        agent_type: AgentType {
            name: "base".to_owned(),
        },
        llms: envelope.spec.llms.clone(),
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
            runtime_uid: &runtime_uid,
            spec_digest: "digest-a",
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
