use std::env;
use std::error::Error;
use std::io;
use std::time::Duration as StdDuration;

use sha2::{Digest, Sha256};
use sqlx::postgres::PgPoolOptions;
use sqlx::types::Uuid;
use steward_admission::{Envelope, EnvelopeSpec};
use steward_store::{
    CreatePersonalAgentRequest, EnvelopeRequestReservationRequest, EnvelopeRequestStatus,
    EnvelopeRequestStatusUpdate, EnvelopeTemplatePublication, PersonalAgentTurnStatus, PgStore,
    StoreError,
};
use steward_types::{
    Budget, CanonicalUserId, Duration, ModelRef, OrganizationId, OrganizationIdentityPolicy,
    RunnerRequirements, ToolGrant,
};

#[tokio::test]
async fn personal_agent_inbox_is_owner_scoped_merged_and_restart_safe() -> Result<(), Box<dyn Error>>
{
    install_rustls_crypto_provider()?;
    let database_url = env::var("STEWARD_TEST_DATABASE_URL")?;
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool.clone());
    store.migrate().await?;

    let suffix = Uuid::new_v4().simple().to_string();
    let alice = register_user(&store, "alice", &suffix).await?;
    let bob = register_user(&store, "bob", &suffix).await?;
    let envelope = provision_envelope(&store, &alice, &suffix).await?;
    let inactive_envelope = reserve_envelope(&store, &alice, &format!("inactive-{suffix}")).await?;

    let foreign = store
        .create_personal_agent(CreatePersonalAgentRequest {
            id: Uuid::new_v4(),
            owner_user_id: &bob,
            name: "Foreign envelope",
            agent_ref: "example-agent@1.0.0",
            instructions: "Reply briefly.",
            envelope_request_id: envelope,
        })
        .await;
    assert!(matches!(
        foreign,
        Err(StoreError::PersonalAgentEnvelopeNotFound)
    ));

    let inactive = store
        .create_personal_agent(CreatePersonalAgentRequest {
            id: Uuid::new_v4(),
            owner_user_id: &alice,
            name: "Inactive envelope",
            agent_ref: "example-agent@1.0.0",
            instructions: "Reply briefly.",
            envelope_request_id: inactive_envelope,
        })
        .await;
    assert!(matches!(
        inactive,
        Err(StoreError::PersonalAgentEnvelopeInactive)
    ));

    let agent = store
        .create_personal_agent(CreatePersonalAgentRequest {
            id: Uuid::new_v4(),
            owner_user_id: &alice,
            name: "Helper",
            agent_ref: "example-agent@1.0.0",
            instructions: "Reply briefly.",
            envelope_request_id: envelope,
        })
        .await?;
    assert!(store.personal_agents(&bob).await?.is_empty());
    assert!(store.personal_agent(&bob, agent.id).await?.is_none());
    assert!(store.personal_agent_thread(&bob, agent.id).await?.is_none());
    assert!(matches!(
        store
            .post_personal_agent_message(
                &bob,
                agent.id,
                Uuid::new_v4(),
                "Do not expose this agent.",
                "bob-message",
            )
            .await,
        Err(StoreError::PersonalAgentNotFound)
    ));

    let first = store
        .post_personal_agent_message(
            &alice,
            agent.id,
            Uuid::new_v4(),
            "First message",
            "message-1",
        )
        .await?;
    assert!(!first.duplicate);
    let duplicate = store
        .post_personal_agent_message(
            &alice,
            agent.id,
            Uuid::new_v4(),
            "First message",
            "message-1",
        )
        .await?;
    assert!(duplicate.duplicate);
    assert_eq!(duplicate.turn.id, first.turn.id);
    assert!(matches!(
        store
            .post_personal_agent_message(
                &alice,
                agent.id,
                Uuid::new_v4(),
                "Changed replay",
                "message-1",
            )
            .await,
        Err(StoreError::PersonalAgentMessageConflict)
    ));

    let running = store
        .claim_personal_agent_turn("worker-a", 1, 8)
        .await?
        .ok_or_else(|| io::Error::other("first turn was not claimable"))?;
    assert_eq!(running.turn.id, first.turn.id);
    assert_eq!(running.turn.status, PersonalAgentTurnStatus::Running);

    let second = store
        .post_personal_agent_message(
            &alice,
            agent.id,
            Uuid::new_v4(),
            "Second message",
            "message-2",
        )
        .await?;
    let third = store
        .post_personal_agent_message(
            &alice,
            agent.id,
            Uuid::new_v4(),
            "Third message",
            "message-3",
        )
        .await?;
    assert_eq!(second.turn.id, third.turn.id);
    assert_ne!(second.turn.id, running.turn.id);
    assert!(
        store
            .claim_personal_agent_turn("worker-b", 1, 8)
            .await?
            .is_none(),
        "a queued turn must not run beside the current turn for the same agent"
    );

    tokio::time::sleep(StdDuration::from_millis(1_100)).await;
    let restarted = store
        .claim_personal_agent_turn("worker-b", 1, 8)
        .await?
        .ok_or_else(|| io::Error::other("expired turn was not reclaimable"))?;
    assert_eq!(restarted.turn.id, running.turn.id);
    assert_eq!(
        store
            .personal_agent_thread(&alice, agent.id)
            .await?
            .ok_or_else(|| io::Error::other("thread disappeared"))?
            .len(),
        2,
        "a restart must resume the running turn instead of creating another one"
    );
    store
        .complete_personal_agent_turn(restarted.turn.id, "worker-b", "First reply")
        .await?;

    let merged = store
        .claim_personal_agent_turn("worker-c", 1, 8)
        .await?
        .ok_or_else(|| io::Error::other("merged next turn was not claimable"))?;
    assert_eq!(merged.turn.id, second.turn.id);
    assert_eq!(
        merged
            .turn
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        ["Second message", "Third message"]
    );
    let events = store
        .personal_agent_events_after(&alice, agent.id, 0)
        .await?
        .ok_or_else(|| io::Error::other("event stream disappeared"))?;
    assert!(
        events.len() >= 7,
        "each durable turn change must append an event"
    );
    assert!(
        events
            .windows(2)
            .all(|events| events[0].seq < events[1].seq),
        "durable event sequences must be strictly increasing"
    );
    assert!(events.iter().any(|event| {
        event.turn.status == PersonalAgentTurnStatus::Done
            && event
                .turn
                .reply
                .as_ref()
                .map(|reply| reply.content.as_str())
                == Some("First reply")
    }));
    assert!(
        store
            .personal_agent_events_after(&bob, agent.id, 0)
            .await?
            .is_none(),
        "another owner must not distinguish an existing event stream"
    );
    let replay_cursor = events[2].seq;
    let replayed = store
        .personal_agent_events_after(&alice, agent.id, replay_cursor)
        .await?
        .ok_or_else(|| io::Error::other("event replay disappeared"))?;
    assert_eq!(
        replayed.iter().map(|event| event.seq).collect::<Vec<_>>(),
        events
            .iter()
            .filter(|event| event.seq > replay_cursor)
            .map(|event| event.seq)
            .collect::<Vec<_>>(),
        "Last-Event-ID replay must return each later durable event exactly once"
    );
    let snapshot = store
        .personal_agent_thread_snapshot(&alice, agent.id)
        .await?
        .ok_or_else(|| io::Error::other("thread snapshot disappeared"))?;
    assert_eq!(snapshot.event_sequence, events.last().ok_or("event")?.seq);

    let mut listener = store.personal_agent_event_listener().await?;
    store
        .post_personal_agent_message(
            &alice,
            agent.id,
            Uuid::new_v4(),
            "Fourth message",
            "message-4",
        )
        .await?;
    let notified_sequence =
        tokio::time::timeout(StdDuration::from_secs(2), listener.recv_sequence())
            .await
            .map_err(|_| io::Error::other("personal-agent event notification timed out"))??;
    let notified_events = store
        .personal_agent_events_after(&alice, agent.id, snapshot.event_sequence)
        .await?
        .ok_or_else(|| io::Error::other("notified event stream disappeared"))?;
    assert_eq!(notified_events.len(), 1);
    assert_eq!(notified_events[0].seq, notified_sequence);
    Ok(())
}

async fn register_user(
    store: &PgStore,
    name: &str,
    suffix: &str,
) -> Result<CanonicalUserId, Box<dyn Error>> {
    let identity = OrganizationIdentityPolicy::new(
        "https://accounts.google.com",
        "example.com",
        OrganizationId::parse("org_example")?,
    )?
    .validate(
        "https://accounts.google.com",
        &format!("{name}-{suffix}"),
        "example.com",
        &format!("{name}-{suffix}@example.com"),
        true,
    )?;
    Ok(store
        .register_canonical_identity(&identity, "test-bootstrap")
        .await?
        .user_id)
}

fn test_envelope() -> Envelope {
    Envelope {
        revision: 1,
        spec: EnvelopeSpec {
            llms: vec![ModelRef {
                provider: "example".to_owned(),
                model: "model-a".to_owned(),
            }],
            tools: vec![ToolGrant {
                provider: "example".to_owned(),
                resource: "documents".to_owned(),
                action: "read".to_owned(),
            }],
            budget: Budget {
                monthly_limit: "10.00".to_owned(),
                single_run_limit: Some("1.00".to_owned()),
                currency: "USD".to_owned(),
            },
            runtime_minutes_limit: Some("10".to_owned()),
            workspace: None,
            ttl: Duration("30m".to_owned()),
            runner: RunnerRequirements::default(),
        },
    }
}

async fn reserve_envelope(
    store: &PgStore,
    owner: &CanonicalUserId,
    suffix: &str,
) -> Result<Uuid, Box<dyn Error>> {
    let envelope = test_envelope();
    let template_id = format!("personal-agent-{suffix}");
    store
        .insert_envelope_template_revision(EnvelopeTemplatePublication {
            template_id: &template_id,
            display_name: "Personal agent",
            member_roles: std::slice::from_ref(&template_id),
            ceiling: &envelope,
            auto_provision_threshold: None,
            allow_inline_browser_tasks: true,
            authored_by: "test-bootstrap",
        })
        .await?;
    Ok(store
        .reserve_envelope_request(EnvelopeRequestReservationRequest {
            owner_user_id: owner,
            template_id: Some(&template_id),
            template_revision: Some(envelope.revision),
            requested_envelope: &envelope,
            idempotency_key: &format!("personal-agent-envelope-{suffix}"),
            justification: None,
            ceiling_exceedances: &[],
            actor: "test-bootstrap",
        })
        .await?
        .record
        .id)
}

async fn provision_envelope(
    store: &PgStore,
    owner: &CanonicalUserId,
    suffix: &str,
) -> Result<Uuid, Box<dyn Error>> {
    let request_id = reserve_envelope(store, owner, suffix).await?;
    let envelope = test_envelope();
    let digest = format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&envelope)?)
    );
    store
        .append_envelope_request_status(
            request_id,
            EnvelopeRequestStatusUpdate {
                from: EnvelopeRequestStatus::Pending,
                to: EnvelopeRequestStatus::Provisioned,
                approval_id: Some(Uuid::new_v4()),
                envelope_instance_id: Some(&format!("env_{}", request_id.simple())),
                envelope_digest: Some(&digest),
                reason: None,
                rationale: Some("personal agent process test"),
                evidence_url: None,
                expires_at: None,
                approved_envelope: Some(&envelope),
                actor: "test-bootstrap",
            },
        )
        .await?;
    Ok(request_id)
}

fn install_rustls_crypto_provider() -> Result<(), io::Error> {
    match tokio_rustls::rustls::crypto::ring::default_provider().install_default() {
        Ok(()) => Ok(()),
        Err(_) if tokio_rustls::rustls::crypto::CryptoProvider::get_default().is_some() => Ok(()),
        Err(_) => Err(io::Error::other("install rustls crypto provider")),
    }
}
