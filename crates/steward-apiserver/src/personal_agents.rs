//! Browser-session personal agents and their one-turn-at-a-time dispatcher.

use std::collections::{BTreeSet, HashMap};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, Router};
use futures::Stream;
use serde::{Deserialize, Serialize};
use steward_store::{
    CreatePersonalAgentRequest as StoreCreatePersonalAgentRequest, EnvelopeRequestStatus,
    PersonalAgentDispatchRecord, PersonalAgentEventListener, PersonalAgentEventRecord,
    PersonalAgentLimits, PersonalAgentMessageRecord, PersonalAgentRecord, PersonalAgentReplyRecord,
    PersonalAgentTurnRecord, PersonalAgentTurnStatus, PgStore, StoreError,
    personal_agent_turn_prompt,
};
use steward_types::ModelRef;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::TaskApiConfig;
use crate::browser_auth::{
    BrowserAuthService, BrowserMutationProof, BrowserSessionContext, protect_browser_routes,
};
use crate::tasks::{
    PersonalAgentTaskError, PersonalAgentTaskRunner, PersonalAgentTaskState,
    personal_agent_task_runner,
};

pub const PERSONAL_AGENTS_API_VERSION: &str = "steward.personal-agents/v1";
pub const DEFAULT_RECENT_TURN_LIMIT: i64 = 8;
pub const DEFAULT_LEASE_SECONDS: i64 = 30;
pub const DEFAULT_POLL_INTERVAL_SECONDS: u64 = 2;
pub const DEFAULT_MAX_CONCURRENT_TURNS: usize = 8;
pub const DEFAULT_MAX_TRANSIENT_ATTEMPTS: i32 = 3;
pub const DEFAULT_RETRY_BACKOFF_SECONDS: i64 = 2;
pub const DEFAULT_MAX_NAME_CHARACTERS: usize = 200;
pub const DEFAULT_MAX_INSTRUCTIONS_BYTES: usize = 16 * 1024;
pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 16 * 1024;
pub const DEFAULT_MAX_QUEUED_BYTES: usize = 24 * 1024;
pub const DEFAULT_MAX_REPLY_BYTES: usize = 1024 * 1024;
pub const DEFAULT_MAX_PROMPT_BYTES: usize = 32 * 1024;
const PERSONAL_AGENT_EVENT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
const PERSONAL_AGENT_EVENT_RECONNECT_INTERVAL: Duration = Duration::from_secs(1);
const PERSONAL_AGENT_EVENT_NOTIFICATION_BUFFER: usize = 256;

fn personal_agent_sse_response<S>(stream: S) -> Response
where
    S: Stream<Item = Result<Event, Infallible>> + Send + 'static,
{
    let mut response = Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(PERSONAL_AGENT_EVENT_HEARTBEAT_INTERVAL)
                .text("ping"),
        )
        .into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response.headers_mut().insert(
        HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    response
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PersonalAgentConfig {
    recent_turn_limit: i64,
    lease_seconds: i64,
    poll_interval: Duration,
    max_concurrent_turns: usize,
    max_transient_attempts: i32,
    retry_backoff_seconds: i64,
    limits: PersonalAgentLimits,
}

impl PersonalAgentConfig {
    pub fn new(
        recent_turn_limit: i64,
        lease_seconds: i64,
        poll_interval_seconds: u64,
        max_concurrent_turns: usize,
        max_transient_attempts: i32,
        retry_backoff_seconds: i64,
        limits: PersonalAgentLimits,
    ) -> Result<Self, String> {
        if !(1..=100).contains(&recent_turn_limit) {
            return Err("personal agent thread turn limit must be between 1 and 100".to_owned());
        }
        if !(5..=3_600).contains(&lease_seconds) {
            return Err("personal agent lease must be between 5 and 3600 seconds".to_owned());
        }
        if poll_interval_seconds == 0 || poll_interval_seconds > lease_seconds as u64 / 2 {
            return Err(
                "personal agent poll fallback must be positive and no more than half the lease"
                    .to_owned(),
            );
        }
        if max_concurrent_turns == 0
            || max_transient_attempts <= 0
            || !(1..=3_600).contains(&retry_backoff_seconds)
        {
            return Err("personal agent concurrency and retry limits must be positive".to_owned());
        }
        if limits.max_name_characters == 0
            || limits.max_instructions_bytes == 0
            || limits.max_message_bytes == 0
            || limits.max_queued_bytes < limits.max_message_bytes
            || limits.max_reply_bytes == 0
            || limits.max_prompt_bytes == 0
        {
            return Err("personal agent content limits must be positive and consistent".to_owned());
        }
        Ok(Self {
            recent_turn_limit,
            lease_seconds,
            poll_interval: Duration::from_secs(poll_interval_seconds),
            max_concurrent_turns,
            max_transient_attempts,
            retry_backoff_seconds,
            limits,
        })
    }
}

#[derive(Clone)]
pub(crate) struct PersonalAgentApiState {
    store: PgStore,
    agent_refs: Arc<BTreeSet<String>>,
    event_notifications: broadcast::Sender<i64>,
    limits: PersonalAgentLimits,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CreatePersonalAgentBody {
    name: String,
    agent_ref: String,
    instructions: String,
    model: Option<ModelRef>,
    #[schema(value_type = String, format = "uuid")]
    envelope_request_id: Uuid,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PostPersonalAgentMessageBody {
    content: String,
    idempotency_key: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PersonalAgentView {
    #[schema(value_type = String, format = "uuid")]
    id: Uuid,
    name: String,
    agent_ref: String,
    instructions: String,
    model: ModelRef,
    #[schema(value_type = String, format = "uuid")]
    envelope_request_id: Uuid,
    created_at: String,
    updated_at: String,
}

impl From<PersonalAgentRecord> for PersonalAgentView {
    fn from(record: PersonalAgentRecord) -> Self {
        Self {
            id: record.id,
            name: record.name,
            agent_ref: record.agent_ref,
            instructions: record.instructions,
            model: record.model,
            envelope_request_id: record.envelope_request_id,
            created_at: record.created_at,
            updated_at: record.updated_at,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PersonalAgentTurnStatusView {
    Queued,
    Running,
    Done,
    Failed,
}

impl From<PersonalAgentTurnStatus> for PersonalAgentTurnStatusView {
    fn from(status: PersonalAgentTurnStatus) -> Self {
        match status {
            PersonalAgentTurnStatus::Queued => Self::Queued,
            PersonalAgentTurnStatus::Running => Self::Running,
            PersonalAgentTurnStatus::Done => Self::Done,
            PersonalAgentTurnStatus::Failed => Self::Failed,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PersonalAgentMessageView {
    #[schema(value_type = String, format = "uuid")]
    id: Uuid,
    content: String,
    created_at: String,
}

impl From<PersonalAgentMessageRecord> for PersonalAgentMessageView {
    fn from(record: PersonalAgentMessageRecord) -> Self {
        Self {
            id: record.id,
            content: record.content,
            created_at: record.created_at,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PersonalAgentReplyView {
    content: String,
    created_at: String,
}

impl From<PersonalAgentReplyRecord> for PersonalAgentReplyView {
    fn from(record: PersonalAgentReplyRecord) -> Self {
        Self {
            content: record.content,
            created_at: record.created_at,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PersonalAgentTurnView {
    #[schema(value_type = String, format = "uuid")]
    id: Uuid,
    status: PersonalAgentTurnStatusView,
    messages: Vec<PersonalAgentMessageView>,
    reply: Option<PersonalAgentReplyView>,
    #[schema(value_type = Option<String>, format = "uuid")]
    run_id: Option<Uuid>,
    failure_reason: Option<String>,
    created_at: String,
    started_at: Option<String>,
    completed_at: Option<String>,
}

impl From<PersonalAgentTurnRecord> for PersonalAgentTurnView {
    fn from(record: PersonalAgentTurnRecord) -> Self {
        Self {
            id: record.id,
            status: record.status.into(),
            messages: record.messages.into_iter().map(Into::into).collect(),
            reply: record.reply.map(Into::into),
            run_id: record.task_uid,
            failure_reason: record.failure_reason,
            created_at: record.created_at,
            started_at: record.started_at,
            completed_at: record.completed_at,
        }
    }
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PersonalAgentsResponse {
    api_version: &'static str,
    agents: Vec<PersonalAgentView>,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PersonalAgentResponse {
    api_version: &'static str,
    agent: PersonalAgentView,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PersonalAgentTurnsResponse {
    api_version: &'static str,
    turns: Vec<PersonalAgentTurnView>,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PersonalAgentTurnResponse {
    api_version: &'static str,
    turn: PersonalAgentTurnView,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PersonalAgentErrorResponse {
    api_version: &'static str,
    error: &'static str,
    failure_reason: &'static str,
}

pub async fn protected_router(
    store: PgStore,
    agent_refs: impl IntoIterator<Item = String>,
    browser_auth: BrowserAuthService,
    config: PersonalAgentConfig,
) -> Result<Router, StoreError> {
    let listener = store.personal_agent_event_listener().await?;
    let (event_notifications, _) = broadcast::channel(PERSONAL_AGENT_EVENT_NOTIFICATION_BUFFER);
    tokio::spawn(forward_personal_agent_event_notifications(
        store.clone(),
        listener,
        event_notifications.clone(),
    ));
    let routes = Router::new()
        .route(
            "/app/api/v1/agents",
            get(list_personal_agents).post(create_personal_agent),
        )
        .route("/app/api/v1/agents/{agent_id}", get(get_personal_agent))
        .route(
            "/app/api/v1/agents/{agent_id}/messages",
            get(list_personal_agent_messages).post(post_personal_agent_message),
        )
        .route(
            "/app/api/v1/agents/{agent_id}/events",
            get(personal_agent_events),
        )
        .with_state(PersonalAgentApiState {
            store,
            agent_refs: Arc::new(agent_refs.into_iter().collect()),
            event_notifications,
            limits: config.limits,
        });
    Ok(protect_browser_routes(routes, browser_auth))
}

async fn forward_personal_agent_event_notifications(
    store: PgStore,
    mut listener: PersonalAgentEventListener,
    notifications: broadcast::Sender<i64>,
) {
    loop {
        match listener.recv_sequence().await {
            Ok(sequence) => {
                let _ = notifications.send(sequence);
            }
            Err(_) => loop {
                tokio::time::sleep(PERSONAL_AGENT_EVENT_RECONNECT_INTERVAL).await;
                match store.personal_agent_event_listener().await {
                    Ok(reconnected) => {
                        listener = reconnected;
                        let _ = notifications.send(0);
                        break;
                    }
                    Err(_) => continue,
                }
            },
        }
    }
}

pub fn spawn_dispatcher(
    store: PgStore,
    task_api_config: TaskApiConfig,
    config: PersonalAgentConfig,
) -> tokio::task::JoinHandle<()> {
    let runner = personal_agent_task_runner(store.clone(), task_api_config);
    tokio::spawn(run_dispatcher(store, runner, config))
}

#[utoipa::path(
    get,
    operation_id = "listPersonalAgents",
    path = "/app/api/v1/agents",
    responses(
        (status = 200, body = PersonalAgentsResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 503, body = PersonalAgentErrorResponse)
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn list_personal_agents(
    session: Option<Extension<BrowserSessionContext>>,
    State(state): State<PersonalAgentApiState>,
) -> Response {
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    match state
        .store
        .personal_agents(&session.principal.canonical_user_id)
        .await
    {
        Ok(agents) => Json(PersonalAgentsResponse {
            api_version: PERSONAL_AGENTS_API_VERSION,
            agents: agents.into_iter().map(Into::into).collect(),
        })
        .into_response(),
        Err(_) => personal_agent_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "personal_agents_unavailable",
            "Personal agents are unavailable. Try again.",
        ),
    }
}

#[utoipa::path(
    post,
    operation_id = "createPersonalAgent",
    path = "/app/api/v1/agents",
    request_body = CreatePersonalAgentBody,
    params(("X-Steward-CSRF" = String, Header)),
    responses(
        (status = 201, body = PersonalAgentResponse),
        (status = 400, body = PersonalAgentErrorResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 404, body = PersonalAgentErrorResponse),
        (status = 409, body = PersonalAgentErrorResponse),
        (status = 503, body = PersonalAgentErrorResponse)
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn create_personal_agent(
    session: Option<Extension<BrowserSessionContext>>,
    mutation: Option<Extension<BrowserMutationProof>>,
    State(state): State<PersonalAgentApiState>,
    body: Result<Json<CreatePersonalAgentBody>, JsonRejection>,
) -> Response {
    let (Some(Extension(session)), Some(Extension(_))) = (session, mutation) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Ok(Json(body)) = body else {
        return invalid_request();
    };
    if !state.agent_refs.contains(&body.agent_ref) {
        return personal_agent_error(
            StatusCode::BAD_REQUEST,
            "agent_ref_unavailable",
            "Choose a coding agent that is currently available.",
        );
    }
    match state
        .store
        .create_personal_agent(
            StoreCreatePersonalAgentRequest {
                id: Uuid::new_v4(),
                owner_user_id: &session.principal.canonical_user_id,
                name: &body.name,
                agent_ref: &body.agent_ref,
                instructions: &body.instructions,
                model: body.model.as_ref(),
                envelope_request_id: body.envelope_request_id,
            },
            state.limits,
        )
        .await
    {
        Ok(agent) => (
            StatusCode::CREATED,
            Json(PersonalAgentResponse {
                api_version: PERSONAL_AGENTS_API_VERSION,
                agent: agent.into(),
            }),
        )
            .into_response(),
        Err(error) => store_error_response(error),
    }
}

#[utoipa::path(
    get,
    operation_id = "getPersonalAgent",
    path = "/app/api/v1/agents/{agent_id}",
    params(("agent_id" = String, Path, format = "uuid")),
    responses(
        (status = 200, body = PersonalAgentResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 404, body = PersonalAgentErrorResponse),
        (status = 503, body = PersonalAgentErrorResponse)
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn get_personal_agent(
    session: Option<Extension<BrowserSessionContext>>,
    State(state): State<PersonalAgentApiState>,
    Path(agent_id): Path<Uuid>,
) -> Response {
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    match state
        .store
        .personal_agent(&session.principal.canonical_user_id, agent_id)
        .await
    {
        Ok(Some(agent)) => Json(PersonalAgentResponse {
            api_version: PERSONAL_AGENTS_API_VERSION,
            agent: agent.into(),
        })
        .into_response(),
        Ok(None) => agent_not_found(),
        Err(_) => unavailable(),
    }
}

#[utoipa::path(
    get,
    operation_id = "listPersonalAgentMessages",
    path = "/app/api/v1/agents/{agent_id}/messages",
    params(("agent_id" = String, Path, format = "uuid")),
    responses(
        (status = 200, body = PersonalAgentTurnsResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 404, body = PersonalAgentErrorResponse),
        (status = 503, body = PersonalAgentErrorResponse)
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn list_personal_agent_messages(
    session: Option<Extension<BrowserSessionContext>>,
    State(state): State<PersonalAgentApiState>,
    Path(agent_id): Path<Uuid>,
) -> Response {
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    match state
        .store
        .personal_agent_thread(&session.principal.canonical_user_id, agent_id)
        .await
    {
        Ok(Some(turns)) => Json(PersonalAgentTurnsResponse {
            api_version: PERSONAL_AGENTS_API_VERSION,
            turns: turns.into_iter().map(Into::into).collect(),
        })
        .into_response(),
        Ok(None) => agent_not_found(),
        Err(_) => unavailable(),
    }
}

#[utoipa::path(
    post,
    operation_id = "postPersonalAgentMessage",
    path = "/app/api/v1/agents/{agent_id}/messages",
    params(
        ("agent_id" = String, Path, format = "uuid"),
        ("X-Steward-CSRF" = String, Header)
    ),
    request_body = PostPersonalAgentMessageBody,
    responses(
        (status = 200, body = PersonalAgentTurnResponse),
        (status = 202, body = PersonalAgentTurnResponse),
        (status = 400, body = PersonalAgentErrorResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 404, body = PersonalAgentErrorResponse),
        (status = 409, body = PersonalAgentErrorResponse),
        (status = 503, body = PersonalAgentErrorResponse)
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn post_personal_agent_message(
    session: Option<Extension<BrowserSessionContext>>,
    mutation: Option<Extension<BrowserMutationProof>>,
    State(state): State<PersonalAgentApiState>,
    Path(agent_id): Path<Uuid>,
    body: Result<Json<PostPersonalAgentMessageBody>, JsonRejection>,
) -> Response {
    let (Some(Extension(session)), Some(Extension(_))) = (session, mutation) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Ok(Json(body)) = body else {
        return invalid_request();
    };
    match state
        .store
        .post_personal_agent_message(
            &session.principal.canonical_user_id,
            agent_id,
            Uuid::new_v4(),
            &body.content,
            &body.idempotency_key,
            state.limits,
        )
        .await
    {
        Ok(result) => (
            if result.duplicate {
                StatusCode::OK
            } else {
                StatusCode::ACCEPTED
            },
            Json(PersonalAgentTurnResponse {
                api_version: PERSONAL_AGENTS_API_VERSION,
                turn: result.turn.into(),
            }),
        )
            .into_response(),
        Err(error) => store_error_response(error),
    }
}

#[utoipa::path(
    get,
    operation_id = "personalAgentEvents",
    path = "/app/api/v1/agents/{agent_id}/events",
    params(
        ("agent_id" = String, Path, format = "uuid"),
        ("Last-Event-ID" = Option<i64>, Header)
    ),
    responses(
        (status = 200, body = String, content_type = "text/event-stream"),
        (status = 400, body = PersonalAgentErrorResponse),
        (status = 401, description = "Browser session is absent or invalid"),
        (status = 404, body = PersonalAgentErrorResponse),
        (status = 503, body = PersonalAgentErrorResponse)
    ),
    security(("browserSession" = []))
)]
pub(crate) async fn personal_agent_events(
    session: Option<Extension<BrowserSessionContext>>,
    State(state): State<PersonalAgentApiState>,
    Path(agent_id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    let Some(Extension(session)) = session else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let last_event_id = match personal_agent_last_event_id(&headers) {
        Ok(last_event_id) => last_event_id,
        Err(()) => return invalid_request(),
    };
    let owner_user_id = session.principal.canonical_user_id;
    let mut notifications = state.event_notifications.subscribe();
    let (mut cursor, initial_events) = if let Some(last_event_id) = last_event_id {
        let events = match state
            .store
            .personal_agent_events_after(&owner_user_id, agent_id, last_event_id)
            .await
        {
            Ok(Some(events)) => events,
            Ok(None) => return agent_not_found(),
            Err(_) => return unavailable(),
        };
        let mut cursor = last_event_id;
        let mut initial = Vec::with_capacity(events.len());
        for event in events {
            cursor = event.seq;
            let Ok(event) = personal_agent_turn_sse_event(event) else {
                return unavailable();
            };
            initial.push(event);
        }
        (cursor, initial)
    } else {
        let snapshot = match state
            .store
            .personal_agent_thread_snapshot(&owner_user_id, agent_id)
            .await
        {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => return agent_not_found(),
            Err(_) => return unavailable(),
        };
        let payload = PersonalAgentTurnsResponse {
            api_version: PERSONAL_AGENTS_API_VERSION,
            turns: snapshot.turns.into_iter().map(Into::into).collect(),
        };
        let Ok(event) =
            personal_agent_sse_event(snapshot.event_sequence, "thread.snapshot", &payload)
        else {
            return unavailable();
        };
        (snapshot.event_sequence, vec![event])
    };
    let store = state.store.clone();
    let stream = async_stream::stream! {
        for event in initial_events {
            yield Ok::<Event, Infallible>(event);
        }
        loop {
            match notifications.recv().await {
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            }
            let events = match store
                .personal_agent_events_after(&owner_user_id, agent_id, cursor)
                .await
            {
                Ok(Some(events)) => events,
                Ok(None) | Err(_) => break,
            };
            for event in events {
                cursor = event.seq;
                let Ok(event) = personal_agent_turn_sse_event(event) else {
                    return;
                };
                yield Ok::<Event, Infallible>(event);
            }
        }
    };
    personal_agent_sse_response(stream)
}

fn personal_agent_last_event_id(headers: &HeaderMap) -> Result<Option<i64>, ()> {
    let Some(value) = headers.get("last-event-id") else {
        return Ok(None);
    };
    let sequence = value
        .to_str()
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|sequence| *sequence >= 0)
        .ok_or(())?;
    Ok(Some(sequence))
}

fn personal_agent_turn_sse_event(
    event: PersonalAgentEventRecord,
) -> Result<Event, serde_json::Error> {
    personal_agent_sse_event(
        event.seq,
        "turn.updated",
        &PersonalAgentTurnResponse {
            api_version: PERSONAL_AGENTS_API_VERSION,
            turn: event.turn.into(),
        },
    )
}

fn personal_agent_sse_event(
    sequence: i64,
    kind: &'static str,
    payload: &impl Serialize,
) -> Result<Event, serde_json::Error> {
    Ok(Event::default()
        .id(sequence.to_string())
        .event(kind)
        .data(serde_json::to_string(payload)?))
}

fn personal_agent_error(
    status: StatusCode,
    error: &'static str,
    failure_reason: &'static str,
) -> Response {
    (
        status,
        Json(PersonalAgentErrorResponse {
            api_version: PERSONAL_AGENTS_API_VERSION,
            error,
            failure_reason,
        }),
    )
        .into_response()
}

fn agent_not_found() -> Response {
    personal_agent_error(
        StatusCode::NOT_FOUND,
        "agent_not_found",
        "Personal agent does not exist.",
    )
}

fn unavailable() -> Response {
    personal_agent_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "personal_agents_unavailable",
        "Personal agents are unavailable. Try again.",
    )
}

fn invalid_request() -> Response {
    personal_agent_error(
        StatusCode::BAD_REQUEST,
        "invalid_personal_agent_request",
        "Check the personal agent details and try again.",
    )
}

fn store_error_response(error: StoreError) -> Response {
    match error {
        StoreError::PersonalAgentNotFound => agent_not_found(),
        StoreError::PersonalAgentEnvelopeNotFound => personal_agent_error(
            StatusCode::NOT_FOUND,
            "envelope_not_found",
            "That Envelope isn't available.",
        ),
        StoreError::PersonalAgentEnvelopeInactive => personal_agent_error(
            StatusCode::CONFLICT,
            "envelope_not_active",
            "Choose an active Envelope and try again.",
        ),
        StoreError::PersonalAgentMessageConflict => personal_agent_error(
            StatusCode::CONFLICT,
            "idempotency_conflict",
            "This message key was already used for different content.",
        ),
        StoreError::PersonalAgentMessageQueueFull => personal_agent_error(
            StatusCode::CONFLICT,
            "message_queue_full",
            "Wait for the current turn to finish, then try again.",
        ),
        StoreError::PersonalAgentModelRequired => personal_agent_error(
            StatusCode::CONFLICT,
            "model_required",
            "Choose a model for this Envelope.",
        ),
        StoreError::PersonalAgentModelUnavailable => personal_agent_error(
            StatusCode::CONFLICT,
            "model_not_available",
            "Choose a model from this Envelope.",
        ),
        StoreError::PersonalAgentConversationTooLarge => personal_agent_error(
            StatusCode::CONFLICT,
            "conversation_too_large",
            "This conversation is too large for another message. Start a new personal agent.",
        ),
        StoreError::InvalidPersonalAgent | StoreError::InvalidPersonalAgentMessage => {
            invalid_request()
        }
        _ => unavailable(),
    }
}

pub(crate) async fn run_dispatcher(
    store: PgStore,
    runner: PersonalAgentTaskRunner<PgStore>,
    config: PersonalAgentConfig,
) {
    let mut active = tokio::task::JoinSet::new();
    let mut active_turns = HashMap::new();
    loop {
        let mut found_turn = false;
        while active.len() < config.max_concurrent_turns {
            let lease_id = format!("personal-agent-turn:{}", Uuid::new_v4());
            let excluded_turn_ids = active_turns.values().copied().collect::<Vec<_>>();
            match store
                .claim_personal_agent_turn(
                    &lease_id,
                    config.lease_seconds,
                    config.recent_turn_limit,
                    &excluded_turn_ids,
                )
                .await
            {
                Ok(Some(dispatch)) => {
                    found_turn = true;
                    let turn_id = dispatch.turn.id;
                    let store = store.clone();
                    let runner = runner.clone();
                    let handle = active.spawn(async move {
                        run_turn(store, runner, lease_id, config, dispatch).await
                    });
                    active_turns.insert(handle.id(), turn_id);
                }
                Ok(None) => break,
                Err(error) => {
                    eprintln!("personal agent claim failed and was rolled back: {error}");
                    break;
                }
            }
        }
        if active.is_empty() {
            tokio::time::sleep(config.poll_interval).await;
        } else if found_turn && active.len() == config.max_concurrent_turns {
            report_completed_turn(active.join_next_with_id().await, &mut active_turns);
        } else {
            tokio::select! {
                completed = active.join_next_with_id() => {
                    report_completed_turn(completed, &mut active_turns);
                },
                () = tokio::time::sleep(config.poll_interval) => {}
            }
        }
    }
}

type CompletedTurn = Result<(tokio::task::Id, Result<(), StoreError>), tokio::task::JoinError>;

fn report_completed_turn(
    result: Option<CompletedTurn>,
    active_turns: &mut HashMap<tokio::task::Id, Uuid>,
) {
    match result {
        Some(Ok((task_id, Ok(())))) => {
            active_turns.remove(&task_id);
        }
        Some(Ok((task_id, Err(error)))) => {
            active_turns.remove(&task_id);
            eprintln!("personal agent turn will be retried after storage failure: {error}");
        }
        Some(Err(error)) => {
            active_turns.remove(&error.id());
            eprintln!("personal agent turn worker stopped unexpectedly: {error}");
        }
        None => {}
    }
}

async fn run_turn(
    store: PgStore,
    runner: PersonalAgentTaskRunner<PgStore>,
    lease_id: String,
    config: PersonalAgentConfig,
    dispatch: PersonalAgentDispatchRecord,
) -> Result<(), StoreError> {
    let task_uid = match dispatch.turn.task_uid {
        Some(task_uid) => task_uid,
        None => {
            let Some(envelope) = dispatch
                .envelope_request
                .as_ref()
                .filter(|envelope| envelope.status == EnvelopeRequestStatus::Provisioned)
            else {
                return store
                    .fail_personal_agent_turn(
                        dispatch.turn.id,
                        &lease_id,
                        "This agent no longer has active access. Choose an active Envelope.",
                    )
                    .await;
            };
            let Some(prompt) = turn_prompt(&dispatch, config.limits.max_prompt_bytes) else {
                return store
                    .fail_personal_agent_turn(
                        dispatch.turn.id,
                        &lease_id,
                        "This conversation is too large for one turn. Start a new personal agent.",
                    )
                    .await;
            };
            match runner
                .submit(
                    dispatch.turn.id,
                    dispatch.owner_user_id.clone(),
                    dispatch.owner_display_email.clone(),
                    &dispatch.agent,
                    &prompt,
                    envelope,
                )
                .await
            {
                Ok(task_uid) => {
                    store
                        .attach_personal_agent_task(dispatch.turn.id, &lease_id, task_uid)
                        .await?;
                    task_uid
                }
                Err(error) => {
                    return handle_task_error(
                        &store,
                        &lease_id,
                        &dispatch,
                        config,
                        error,
                        "This turn could not start with the selected coding agent and Envelope.",
                    )
                    .await;
                }
            }
        }
    };
    loop {
        store
            .renew_personal_agent_turn_lease(dispatch.turn.id, &lease_id, config.lease_seconds)
            .await?;
        match runner.status(task_uid, &dispatch.owner_user_id).await {
            Ok(PersonalAgentTaskState::Pending) => {
                tokio::time::sleep(config.poll_interval).await;
            }
            Ok(PersonalAgentTaskState::Done(reply)) => {
                return match store
                    .complete_personal_agent_turn(
                        dispatch.turn.id,
                        &lease_id,
                        &reply,
                        config.limits.max_reply_bytes,
                    )
                    .await
                {
                    Err(StoreError::PersonalAgentReplyTooLarge) => {
                        store
                            .fail_personal_agent_turn(
                                dispatch.turn.id,
                                &lease_id,
                                "The reply was too large to save.",
                            )
                            .await
                    }
                    Err(StoreError::InvalidPersonalAgentMessage) => {
                        store
                            .fail_personal_agent_turn(
                                dispatch.turn.id,
                                &lease_id,
                                "The reply couldn't be saved.",
                            )
                            .await
                    }
                    result => result,
                };
            }
            Ok(PersonalAgentTaskState::Failed(reason)) => {
                let reason = safe_failure_reason(&reason);
                return store
                    .fail_personal_agent_turn(dispatch.turn.id, &lease_id, reason)
                    .await;
            }
            Err(error) => {
                if keep_monitoring_after_status_error(error) {
                    tokio::time::sleep(config.poll_interval).await;
                } else {
                    return handle_task_error(
                        &store,
                        &lease_id,
                        &dispatch,
                        config,
                        error,
                        "The coding agent did not return a usable reply. Try again.",
                    )
                    .await;
                }
            }
        }
    }
}

fn keep_monitoring_after_status_error(error: PersonalAgentTaskError) -> bool {
    error == PersonalAgentTaskError::Transient
}

async fn handle_task_error(
    store: &PgStore,
    lease_id: &str,
    dispatch: &PersonalAgentDispatchRecord,
    config: PersonalAgentConfig,
    error: PersonalAgentTaskError,
    permanent_reason: &str,
) -> Result<(), StoreError> {
    match error {
        PersonalAgentTaskError::Permanent => {
            store
                .fail_personal_agent_turn(dispatch.turn.id, lease_id, permanent_reason)
                .await
        }
        PersonalAgentTaskError::Transient => {
            let delay_seconds = retry_delay_seconds(
                config.retry_backoff_seconds,
                dispatch.turn.dispatch_attempts,
            );
            store
                .retry_personal_agent_turn(
                    dispatch.turn.id,
                    lease_id,
                    delay_seconds,
                    config.max_transient_attempts,
                )
                .await
        }
    }
}

fn retry_delay_seconds(base_seconds: i64, prior_failures: i32) -> i64 {
    let shift = u32::try_from(prior_failures.max(0))
        .unwrap_or(u32::MAX)
        .min(30);
    base_seconds.saturating_mul(1_i64 << shift).min(3_600)
}

fn safe_failure_reason(reason: &str) -> &str {
    if reason.is_empty() || reason.contains('\0') || reason.chars().count() > 500 {
        "This turn could not be completed. Try again."
    } else {
        reason
    }
}

fn turn_prompt(dispatch: &PersonalAgentDispatchRecord, max_prompt_bytes: usize) -> Option<String> {
    personal_agent_turn_prompt(
        &dispatch.agent.instructions,
        &dispatch.recent_turns,
        &dispatch.turn.messages,
        max_prompt_bytes,
    )
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use axum::body::to_bytes;
    use axum::http::{HeaderValue, StatusCode, header};
    use futures::StreamExt;

    use super::{
        DEFAULT_MAX_INSTRUCTIONS_BYTES, DEFAULT_MAX_MESSAGE_BYTES, DEFAULT_MAX_NAME_CHARACTERS,
        DEFAULT_MAX_PROMPT_BYTES, DEFAULT_MAX_QUEUED_BYTES, DEFAULT_MAX_REPLY_BYTES,
        PERSONAL_AGENT_EVENT_HEARTBEAT_INTERVAL, PERSONAL_AGENTS_API_VERSION, PersonalAgentConfig,
        PersonalAgentDispatchRecord, keep_monitoring_after_status_error,
        personal_agent_last_event_id, personal_agent_sse_event, personal_agent_sse_response,
        retry_delay_seconds, safe_failure_reason, store_error_response, turn_prompt,
    };
    use crate::tasks::{PersonalAgentTaskError, personal_agent_task_definition};
    use steward_admission::{Envelope, EnvelopeSpec};
    use steward_store::{
        PersonalAgentLimits, PersonalAgentMessageRecord, PersonalAgentRecord,
        PersonalAgentTurnRecord, PersonalAgentTurnStatus,
    };

    #[tokio::test]
    async fn personal_agent_sse_disables_edge_buffering() {
        let response = personal_agent_sse_response(futures::stream::empty());
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE),
            Some(&HeaderValue::from_static("text/event-stream"))
        );
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL),
            Some(&HeaderValue::from_static("no-cache"))
        );
        assert_eq!(
            response.headers().get("x-accel-buffering"),
            Some(&HeaderValue::from_static("no"))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn personal_agent_sse_sends_ping_before_edge_idle_timeouts() -> Result<(), String> {
        assert!((15..=30).contains(&PERSONAL_AGENT_EVENT_HEARTBEAT_INTERVAL.as_secs()));
        let response = personal_agent_sse_response(futures::stream::pending::<
            Result<axum::response::sse::Event, Infallible>,
        >());
        let mut body = response.into_body().into_data_stream();
        let read = tokio::spawn(async move { body.next().await });
        tokio::task::yield_now().await;
        tokio::time::advance(PERSONAL_AGENT_EVENT_HEARTBEAT_INTERVAL).await;
        let chunk = read
            .await
            .map_err(|error| format!("join heartbeat read: {error}"))?
            .ok_or_else(|| "heartbeat stream closed".to_owned())?
            .map_err(|error| format!("read heartbeat: {error}"))?;
        assert_eq!(&chunk[..], b": ping\n\n");
        Ok(())
    }

    #[tokio::test]
    async fn personal_agent_sse_frames_durable_sequence_and_payload() -> Result<(), String> {
        let event = personal_agent_sse_event(
            7,
            "turn.updated",
            &serde_json::json!({"apiVersion": PERSONAL_AGENTS_API_VERSION}),
        )
        .map_err(|error| format!("serialize event: {error}"))?;
        let response =
            personal_agent_sse_response(futures::stream::iter([Ok::<_, Infallible>(event)]));
        let body = to_bytes(response.into_body(), 1024)
            .await
            .map_err(|error| format!("read event: {error}"))?;
        let body = std::str::from_utf8(&body).map_err(|error| format!("decode event: {error}"))?;
        assert!(body.contains("id: 7\n"));
        assert!(body.contains("event: turn.updated\n"));
        assert!(body.contains("\"apiVersion\":\"steward.personal-agents/v1\""));
        Ok(())
    }

    #[test]
    fn personal_agent_last_event_id_is_optional_nonnegative_and_strict() {
        let mut headers = axum::http::HeaderMap::new();
        assert_eq!(personal_agent_last_event_id(&headers), Ok(None));
        headers.insert("last-event-id", HeaderValue::from_static("42"));
        assert_eq!(personal_agent_last_event_id(&headers), Ok(Some(42)));
        for invalid in ["-1", "1.5", "not-a-sequence"] {
            headers.insert("last-event-id", HeaderValue::from_static(invalid));
            assert_eq!(personal_agent_last_event_id(&headers), Err(()));
        }
    }
    use steward_types::{
        Budget, CanonicalUserId, Duration, Email, ModelRef, RunnerRequirements, ToolGrant,
    };
    use uuid::Uuid;

    fn limits() -> PersonalAgentLimits {
        PersonalAgentLimits {
            max_name_characters: DEFAULT_MAX_NAME_CHARACTERS,
            max_instructions_bytes: DEFAULT_MAX_INSTRUCTIONS_BYTES,
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
            max_queued_bytes: DEFAULT_MAX_QUEUED_BYTES,
            max_reply_bytes: DEFAULT_MAX_REPLY_BYTES,
            max_prompt_bytes: DEFAULT_MAX_PROMPT_BYTES,
        }
    }

    #[test]
    fn configuration_refuses_unbounded_or_unsafe_dispatch_values() {
        assert!(PersonalAgentConfig::new(0, 30, 1, 8, 3, 2, limits()).is_err());
        assert!(PersonalAgentConfig::new(101, 30, 1, 8, 3, 2, limits()).is_err());
        assert!(PersonalAgentConfig::new(8, 4, 1, 8, 3, 2, limits()).is_err());
        assert!(PersonalAgentConfig::new(8, 30, 16, 8, 3, 2, limits()).is_err());
        assert!(PersonalAgentConfig::new(8, 30, 1, 0, 3, 2, limits()).is_err());
        assert!(PersonalAgentConfig::new(8, 30, 1, 8, 0, 2, limits()).is_err());
        assert!(PersonalAgentConfig::new(8, 30, 1, 8, 3, 0, limits()).is_err());
        assert!(PersonalAgentConfig::new(8, 30, 1, 8, 3, 2, limits()).is_ok());
    }

    #[test]
    fn retries_grow_from_configuration_and_attached_tasks_keep_monitoring() {
        assert_eq!(retry_delay_seconds(2, 0), 2);
        assert_eq!(retry_delay_seconds(2, 1), 4);
        assert_eq!(retry_delay_seconds(2, 2), 8);
        assert_eq!(retry_delay_seconds(2, 30), 3_600);
        assert!(keep_monitoring_after_status_error(
            PersonalAgentTaskError::Transient
        ));
        assert!(!keep_monitoring_after_status_error(
            PersonalAgentTaskError::Permanent
        ));
    }

    #[test]
    fn task_failure_reasons_are_safe_to_persist() {
        assert_eq!(
            safe_failure_reason("Model access ended."),
            "Model access ended."
        );
        assert_eq!(
            safe_failure_reason("bad\0reason"),
            "This turn could not be completed. Try again."
        );
        assert_eq!(
            safe_failure_reason(&"x".repeat(501)),
            "This turn could not be completed. Try again."
        );
    }

    #[tokio::test]
    async fn queue_full_response_tells_the_user_to_wait() -> Result<(), String> {
        let response =
            store_error_response(steward_store::StoreError::PersonalAgentMessageQueueFull);
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = to_bytes(response.into_body(), 4 * 1024)
            .await
            .map_err(|error| error.to_string())?;
        let body: serde_json::Value =
            serde_json::from_slice(&body).map_err(|error| error.to_string())?;
        assert_eq!(body["error"], "message_queue_full");
        assert_eq!(
            body["failureReason"],
            "Wait for the current turn to finish, then try again."
        );
        Ok(())
    }

    #[test]
    fn prompt_keeps_order_and_explicitly_refuses_tools() -> Result<(), String> {
        let turn = PersonalAgentTurnRecord {
            id: Uuid::new_v4(),
            status: PersonalAgentTurnStatus::Running,
            messages: vec![PersonalAgentMessageRecord {
                id: Uuid::new_v4(),
                content: "first".to_owned(),
                created_at: "2026-10-19T00:00:00Z".to_owned(),
            }],
            reply: None,
            task_uid: None,
            failure_reason: None,
            created_at: "2026-10-19T00:00:00Z".to_owned(),
            started_at: Some("2026-10-19T00:00:01Z".to_owned()),
            completed_at: None,
            dispatch_attempts: 1,
        };
        let dispatch = PersonalAgentDispatchRecord {
            agent: PersonalAgentRecord {
                id: Uuid::new_v4(),
                name: "Helper".to_owned(),
                agent_ref: "example-agent@1.0.0".to_owned(),
                instructions: "Be concise.".to_owned(),
                model: ModelRef {
                    provider: "example".to_owned(),
                    model: "model-a".to_owned(),
                },
                envelope_request_id: Uuid::new_v4(),
                created_at: "2026-10-19T00:00:00Z".to_owned(),
                updated_at: "2026-10-19T00:00:00Z".to_owned(),
            },
            owner_user_id: CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?,
            owner_display_email: Email("alice@example.com".to_owned()),
            turn,
            recent_turns: Vec::new(),
            envelope_request: None,
        };
        let prompt = turn_prompt(&dispatch, DEFAULT_MAX_PROMPT_BYTES).ok_or("prompt")?;
        assert!(prompt.contains("Be concise."));
        assert!(prompt.contains("User: first"));
        assert!(prompt.contains("Do not use tools."));
        assert!(prompt.ends_with("out/reply.txt."));
        Ok(())
    }

    #[test]
    fn task_definition_keeps_configured_models_and_removes_every_tool() -> Result<(), String> {
        let envelope = Envelope {
            revision: 1,
            spec: EnvelopeSpec {
                llms: vec![
                    ModelRef {
                        provider: "future-provider".to_owned(),
                        model: "future-model".to_owned(),
                    },
                    ModelRef {
                        provider: "second-provider".to_owned(),
                        model: "second-model".to_owned(),
                    },
                ],
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
        };
        let selected_model = envelope.spec.llms[1].clone();
        let definition = personal_agent_task_definition(
            "future-agent@9.0.0",
            "Answer the user.",
            &envelope,
            &selected_model,
        )
        .map_err(|error| format!("build personal agent definition: {error}"))?;
        assert_eq!(definition.runtime.agent_ref.as_str(), "future-agent@9.0.0");
        let requirements = definition.requires.ok_or("requirements")?;
        assert_eq!(requirements.authority.llms.len(), 1);
        assert_eq!(
            requirements.authority.llms[0].provider.as_str(),
            "second-provider"
        );
        assert_eq!(
            requirements.authority.llms[0].model.as_str(),
            "second-model"
        );
        assert!(requirements.authority.tools.is_empty());
        Ok(())
    }
}
