use std::env;
use std::error::Error;
use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use sqlx::Row;
use sqlx::postgres::PgPoolOptions;
use steward_admission::{AdmissionDelta, Envelope, EnvelopeSpec};
use steward_store::{ApproveAdmission, ParkRejection, PgStore, StoreError};
use steward_types::{AgentRuntimeSpec, AgentType, Budget, Duration, Email, ModelRef, Principal};

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
    assert_eq!(
        store
            .retire_pending_approval_if_superseded(
                next_escalation.approval_id,
                active_application.approval_id,
                "runtime-evidence-a",
                "steward-apiserver",
                "superseded by an active approval during create convergence",
            )
            .await,
        Err(StoreError::DecisionFilingInProgress),
        "retirement must not invalidate an external decision request in flight"
    );
    store
        .complete_decision_filing(
            next_escalation.approval_id,
            filing_token,
            "PROJ-456",
            "https://jira.example.com/browse/PROJ-456",
        )
        .await?;
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
        "the losing post-park approval must be retired atomically"
    );
    assert!(
        !store
            .pending_approvals()
            .await?
            .iter()
            .any(|pending| pending.approval_id == next_escalation.approval_id),
        "a retired loser must not remain reachable through the approval queue"
    );
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
    let current_escalation = store
        .park_rejection(ParkRejection {
            runtime_uid: "runtime-evidence-a",
            runtime_namespace: "team-a",
            runtime_name: "runtime-evidence-a",
            spec_digest: "digest-evidence-a",
            base_spec_digest: "base-digest-evidence-a",
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
