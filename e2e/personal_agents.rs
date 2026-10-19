use std::env;
use std::error::Error;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use sha2::{Digest, Sha256};
use sqlx::postgres::PgPoolOptions;
use sqlx::types::Uuid;
use steward_admission::{Envelope, EnvelopeSpec};
use steward_apiserver::browser_auth::{
    BrowserAuthFailure, BrowserAuthService, BrowserAuthorizationRequest, BrowserIdentityResolver,
    BrowserOidcProvider, BrowserPrincipal, BrowserRole, GoogleOidcConfig,
    VerifiedOrganizationClaims, browser_auth_router,
};
use steward_apiserver::personal_agents::{PersonalAgentConfig, protected_router, spawn_dispatcher};
use steward_store::{
    CreatePersonalAgentRequest, EnvelopeRequestReservationRequest, EnvelopeRequestStatus,
    EnvelopeRequestStatusUpdate, EnvelopeTemplatePublication, PersonalAgentLimits,
    PersonalAgentTurnStatus, PgStore, StoreError,
};
use steward_types::{
    Budget, CanonicalUserId, Duration, Email, GOOGLE_ORGANIZATION_ISSUER, ModelRef, OrganizationId,
    OrganizationIdentity, OrganizationIdentityPolicy, RunnerRequirements, ToolGrant,
};
use tower::ServiceExt;

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
    let limits = test_limits();
    let dispatcher_config =
        PersonalAgentConfig::new(8, 5, 1, 2, 3, limits).map_err(io::Error::other)?;
    let cascading_history_links = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM pg_constraint \
         WHERE contype = 'f' AND confdeltype <> 'r' \
           AND conrelid IN ('personal_agent_turns'::regclass, \
                            'personal_agent_messages'::regclass, \
                            'personal_agent_turn_events'::regclass) \
           AND confrelid IN ('personal_agents'::regclass, 'personal_agent_turns'::regclass)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(cascading_history_links, 0);

    let suffix = Uuid::new_v4().simple().to_string();
    let alice = register_user(&store, "alice", &suffix).await?;
    let bob = register_user(&store, "bob", &suffix).await?;
    let envelope = provision_envelope(&store, &alice, &suffix).await?;
    let inactive_envelope = reserve_envelope(&store, &alice, &format!("inactive-{suffix}")).await?;

    let foreign = store
        .create_personal_agent(
            CreatePersonalAgentRequest {
                id: Uuid::new_v4(),
                owner_user_id: &bob,
                name: "Foreign envelope",
                agent_ref: "example-agent@1",
                instructions: "Reply briefly.",
                model: None,
                envelope_request_id: envelope,
            },
            limits,
        )
        .await;
    assert!(matches!(
        foreign,
        Err(StoreError::PersonalAgentEnvelopeNotFound)
    ));

    let inactive = store
        .create_personal_agent(
            CreatePersonalAgentRequest {
                id: Uuid::new_v4(),
                owner_user_id: &alice,
                name: "Inactive envelope",
                agent_ref: "example-agent@1",
                instructions: "Reply briefly.",
                model: None,
                envelope_request_id: inactive_envelope,
            },
            limits,
        )
        .await;
    assert!(matches!(
        inactive,
        Err(StoreError::PersonalAgentEnvelopeInactive)
    ));

    let agent = store
        .create_personal_agent(
            CreatePersonalAgentRequest {
                id: Uuid::new_v4(),
                owner_user_id: &alice,
                name: "Helper",
                agent_ref: "example-agent@1",
                instructions: "Reply briefly.",
                model: None,
                envelope_request_id: envelope,
            },
            limits,
        )
        .await?;
    assert_eq!(agent.model, test_envelope().spec.llms[0]);
    let mut multiple_models = test_envelope();
    multiple_models.spec.llms.push(ModelRef {
        provider: "future-provider".to_owned(),
        model: "future-model".to_owned(),
    });
    let multiple_envelope = provision_envelope_with(
        &store,
        &alice,
        &format!("multiple-{suffix}"),
        &multiple_models,
    )
    .await?;
    let missing_model = store
        .create_personal_agent(
            CreatePersonalAgentRequest {
                id: Uuid::new_v4(),
                owner_user_id: &alice,
                name: "Choose a model",
                agent_ref: "example-agent@1",
                instructions: "Reply briefly.",
                model: None,
                envelope_request_id: multiple_envelope,
            },
            limits,
        )
        .await;
    assert!(matches!(
        missing_model,
        Err(StoreError::PersonalAgentModelRequired)
    ));
    let selected_model = multiple_models.spec.llms[1].clone();
    let selected_agent = store
        .create_personal_agent(
            CreatePersonalAgentRequest {
                id: Uuid::new_v4(),
                owner_user_id: &alice,
                name: "Selected model",
                agent_ref: "example-agent@1",
                instructions: "Reply briefly.",
                model: Some(&selected_model),
                envelope_request_id: multiple_envelope,
            },
            limits,
        )
        .await?;
    assert_eq!(selected_agent.model, selected_model);
    personal_agent_router_statuses(
        store.clone(),
        alice.clone(),
        multiple_envelope,
        dispatcher_config,
    )
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
                limits,
            )
            .await,
        Err(StoreError::PersonalAgentNotFound)
    ));
    let too_small_for_a_turn = PersonalAgentLimits {
        max_prompt_bytes: 128,
        ..limits
    };
    assert!(matches!(
        store
            .post_personal_agent_message(
                &alice,
                agent.id,
                Uuid::new_v4(),
                "This message fits its own limit but not the assembled prompt.",
                "conversation-too-large",
                too_small_for_a_turn,
            )
            .await,
        Err(StoreError::PersonalAgentConversationTooLarge)
    ));

    let first = store
        .post_personal_agent_message(
            &alice,
            agent.id,
            Uuid::new_v4(),
            "First message",
            "message-1",
            limits,
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
            limits,
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
                limits,
            )
            .await,
        Err(StoreError::PersonalAgentMessageConflict)
    ));

    let dispatcher = spawn_dispatcher(
        store.clone(),
        super::published_workflow_task_config("example")?,
        dispatcher_config,
    );
    let mut running = None;
    for _ in 0..50 {
        running = store
            .personal_agent_thread(&alice, agent.id)
            .await?
            .and_then(|turns| turns.into_iter().find(|turn| turn.task_uid.is_some()));
        if running.is_some() {
            break;
        }
        tokio::time::sleep(StdDuration::from_millis(100)).await;
    }
    let running = running.ok_or_else(|| io::Error::other("dispatcher did not attach a Task"))?;
    assert_eq!(running.id, first.turn.id);
    assert_eq!(running.status, PersonalAgentTurnStatus::Running);
    let task_uid = running
        .task_uid
        .ok_or_else(|| io::Error::other("running turn omitted its Task UID"))?;
    let task = store
        .task_for_submitter(task_uid, "steward-personal-agent", alice.as_str())
        .await?
        .ok_or_else(|| io::Error::other("attached Task disappeared"))?;
    assert!(
        task.runtime_spec.tools.is_empty(),
        "the admitted personal-agent Task must remove every Envelope tool"
    );

    let second = store
        .post_personal_agent_message(
            &alice,
            agent.id,
            Uuid::new_v4(),
            "Second message",
            "message-2",
            limits,
        )
        .await?;
    let third = store
        .post_personal_agent_message(
            &alice,
            agent.id,
            Uuid::new_v4(),
            "Third message",
            "message-3",
            limits,
        )
        .await?;
    let mut concurrent_posts = tokio::task::JoinSet::new();
    let agent_id = agent.id;
    for index in 0..6 {
        let store = store.clone();
        let owner = alice.clone();
        concurrent_posts.spawn(async move {
            store
                .post_personal_agent_message(
                    &owner,
                    agent_id,
                    Uuid::new_v4(),
                    &format!("Concurrent message {index}"),
                    &format!("concurrent-message-{index}"),
                    limits,
                )
                .await
        });
    }
    while let Some(posted) = concurrent_posts.join_next().await {
        let posted = posted.map_err(io::Error::other)??;
        assert_eq!(posted.turn.id, second.turn.id);
    }
    assert_eq!(second.turn.id, third.turn.id);
    assert_ne!(second.turn.id, running.id);
    let (claim_a, claim_b, claim_c) = tokio::join!(
        store.claim_personal_agent_turn("worker-b-a", 1, 8),
        store.claim_personal_agent_turn("worker-b-b", 1, 8),
        store.claim_personal_agent_turn("worker-b-c", 1, 8),
    );
    assert!(claim_a?.is_none() && claim_b?.is_none() && claim_c?.is_none());
    let (queued_count, running_count) = sqlx::query_as::<_, (i64, i64)>(
        "SELECT count(*) FILTER (WHERE status = 'queued')::bigint, \
                count(*) FILTER (WHERE status = 'running')::bigint \
         FROM personal_agent_turns WHERE agent_id = $1",
    )
    .bind(agent.id)
    .fetch_one(&pool)
    .await?;
    assert_eq!((queued_count, running_count), (1, 1));

    dispatcher.abort();
    let _dispatcher_result = dispatcher.await;
    tokio::time::sleep(StdDuration::from_millis(5_100)).await;
    let restarted = store
        .claim_personal_agent_turn("worker-b", 1, 8)
        .await?
        .ok_or_else(|| io::Error::other("expired turn was not reclaimable"))?;
    assert_eq!(restarted.turn.id, running.id);
    assert_eq!(restarted.turn.task_uid, Some(task_uid));
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
        .complete_personal_agent_turn(
            restarted.turn.id,
            "worker-b",
            "First reply",
            limits.max_reply_bytes,
        )
        .await?;

    let merged = store
        .claim_personal_agent_turn("worker-c", 1, 8)
        .await?
        .ok_or_else(|| io::Error::other("merged next turn was not claimable"))?;
    assert_eq!(merged.turn.id, second.turn.id);
    let merged_messages = merged
        .turn
        .messages
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>();
    assert_eq!(&merged_messages[..2], ["Second message", "Third message"]);
    assert_eq!(merged_messages.len(), 8);
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
            limits,
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

#[derive(Clone)]
struct TestOidcProvider {
    nonce: Arc<Mutex<Option<String>>>,
}

impl BrowserOidcProvider for TestOidcProvider {
    fn authorization_url(
        &self,
        flow: &BrowserAuthorizationRequest,
    ) -> Result<String, BrowserAuthFailure> {
        *self
            .nonce
            .lock()
            .map_err(|_| BrowserAuthFailure::ProviderUnavailable)? = Some(flow.nonce().to_owned());
        Ok(format!(
            "https://steward.example.test/admin/auth/callback?code=e2e-code&state={}",
            flow.state()
        ))
    }

    fn exchange_code<'a>(
        &'a self,
        code: &'a str,
        _pkce_verifier: &'a str,
        _callback_uri: &'a str,
        expected_nonce: &'a str,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<VerifiedOrganizationClaims, BrowserAuthFailure>> + Send + 'a,
        >,
    > {
        Box::pin(async move {
            let nonce = self
                .nonce
                .lock()
                .map_err(|_| BrowserAuthFailure::ProviderUnavailable)?
                .take()
                .ok_or(BrowserAuthFailure::InvalidFlow)?;
            if code != "e2e-code" || nonce != expected_nonce {
                return Err(BrowserAuthFailure::InvalidIdentity);
            }
            Ok(VerifiedOrganizationClaims::new(
                GOOGLE_ORGANIZATION_ISSUER.to_owned(),
                "personal-agent-e2e".to_owned(),
                "example.com".to_owned(),
                "alice@example.com".to_owned(),
                true,
                Some("Alice Example".to_owned()),
                nonce,
            ))
        })
    }
}

#[derive(Clone)]
struct TestIdentityResolver {
    user_id: CanonicalUserId,
}

impl BrowserIdentityResolver for TestIdentityResolver {
    fn resolve_or_register<'a>(
        &'a self,
        _identity: &'a OrganizationIdentity,
    ) -> Pin<Box<dyn Future<Output = Result<BrowserPrincipal, BrowserAuthFailure>> + Send + 'a>>
    {
        Box::pin(async move {
            Ok(BrowserPrincipal {
                canonical_user_id: self.user_id.clone(),
                display_name: "Alice Example".to_owned(),
                display_email: Email::parse("alice@example.com")
                    .map_err(|_| BrowserAuthFailure::IdentityUnavailable)?,
                role: BrowserRole::User,
                member_roles: Vec::new(),
            })
        })
    }
}

async fn personal_agent_router_statuses(
    store: PgStore,
    owner: CanonicalUserId,
    multiple_envelope: Uuid,
    config: PersonalAgentConfig,
) -> Result<(), Box<dyn Error>> {
    let origin = "https://steward.example.test";
    let auth = BrowserAuthService::google(
        GoogleOidcConfig::new(
            "personal-agent-e2e",
            origin,
            format!("{origin}/admin/auth/callback"),
            "example.com",
            OrganizationId::parse("org_example")?,
        )?,
        Arc::new(TestOidcProvider {
            nonce: Arc::new(Mutex::new(None)),
        }),
        Arc::new(TestIdentityResolver { user_id: owner }),
    )?;
    let routes = || {
        protected_router(
            store.clone(),
            ["example-agent@1".to_owned()],
            auth.clone(),
            config,
        )
    };
    let unauthorized = routes()
        .await?
        .oneshot(
            Request::builder()
                .uri("/app/api/v1/agents")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let login = browser_auth_router(auth.clone())
        .oneshot(
            Request::builder()
                .uri("/admin/auth/login")
                .body(Body::empty())?,
        )
        .await?;
    let flow_cookie = response_cookie(&login, "__Secure-steward-oidc-flow")?;
    let callback = login
        .headers()
        .get(header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix(origin))
        .ok_or_else(|| io::Error::other("login omitted callback redirect"))?;
    let callback = browser_auth_router(auth.clone())
        .oneshot(
            Request::builder()
                .uri(callback)
                .header(header::COOKIE, flow_cookie)
                .body(Body::empty())?,
        )
        .await?;
    let session_cookie = response_cookie(&callback, "__Host-steward-session")?;
    let session = browser_auth_router(auth.clone())
        .oneshot(
            Request::builder()
                .uri("/admin/api/v1/session")
                .header(header::COOKIE, &session_cookie)
                .body(Body::empty())?,
        )
        .await?;
    let session_body = to_bytes(session.into_body(), 1024 * 1024).await?;
    let csrf = serde_json::from_slice::<serde_json::Value>(&session_body)?["csrf"]
        .as_str()
        .ok_or_else(|| io::Error::other("session omitted CSRF value"))?
        .to_owned();

    let malformed = routes()
        .await?
        .oneshot(mutation_request(
            "/app/api/v1/agents",
            &session_cookie,
            &csrf,
            origin,
            "{",
        )?)
        .await?;
    assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
    let missing = routes()
        .await?
        .oneshot(
            Request::builder()
                .uri(format!("/app/api/v1/agents/{}", Uuid::new_v4()))
                .header(header::COOKIE, &session_cookie)
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    let conflict_body = serde_json::json!({
        "name": "Choose a model",
        "agentRef": "example-agent@1",
        "instructions": "Reply briefly.",
        "envelopeRequestId": multiple_envelope,
    })
    .to_string();
    let conflict = routes()
        .await?
        .oneshot(mutation_request(
            "/app/api/v1/agents",
            &session_cookie,
            &csrf,
            origin,
            &conflict_body,
        )?)
        .await?;
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
    Ok(())
}

fn mutation_request(
    uri: &str,
    cookie: &str,
    csrf: &str,
    origin: &str,
    body: &str,
) -> Result<Request<Body>, axum::http::Error> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::COOKIE, cookie)
        .header(header::ORIGIN, origin)
        .header("sec-fetch-site", "same-origin")
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-steward-csrf", csrf)
        .body(Body::from(body.to_owned()))
}

fn response_cookie(response: &axum::response::Response, name: &str) -> Result<String, io::Error> {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find(|value| value.starts_with(&format!("{name}=")))
        .and_then(|value| value.split(';').next())
        .map(str::to_owned)
        .ok_or_else(|| io::Error::other(format!("response omitted {name} cookie")))
}

fn test_limits() -> PersonalAgentLimits {
    PersonalAgentLimits {
        max_name_bytes: 200,
        max_instructions_bytes: 16 * 1024,
        max_message_bytes: 16 * 1024,
        max_queued_bytes: 24 * 1024,
        max_reply_bytes: 1024 * 1024,
        max_prompt_bytes: 32 * 1024,
    }
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
    reserve_envelope_with(store, owner, suffix, &envelope).await
}

async fn reserve_envelope_with(
    store: &PgStore,
    owner: &CanonicalUserId,
    suffix: &str,
    envelope: &Envelope,
) -> Result<Uuid, Box<dyn Error>> {
    let template_id = format!("personal-agent-{suffix}");
    store
        .insert_envelope_template_revision(EnvelopeTemplatePublication {
            template_id: &template_id,
            display_name: "Personal agent",
            member_roles: std::slice::from_ref(&template_id),
            ceiling: envelope,
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
            requested_envelope: envelope,
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
    let envelope = test_envelope();
    provision_envelope_with(store, owner, suffix, &envelope).await
}

async fn provision_envelope_with(
    store: &PgStore,
    owner: &CanonicalUserId,
    suffix: &str,
    envelope: &Envelope,
) -> Result<Uuid, Box<dyn Error>> {
    let request_id = reserve_envelope_with(store, owner, suffix, envelope).await?;
    let digest = format!("sha256:{:x}", Sha256::digest(serde_json::to_vec(envelope)?));
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
                approved_envelope: Some(envelope),
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
