//! Browser-session personal agents and their one-turn-at-a-time dispatcher.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use steward_store::{
    CreatePersonalAgentRequest as StoreCreatePersonalAgentRequest, EnvelopeRequestStatus,
    PersonalAgentDispatchRecord, PersonalAgentMessageRecord, PersonalAgentRecord,
    PersonalAgentReplyRecord, PersonalAgentTurnRecord, PersonalAgentTurnStatus, PgStore,
    StoreError,
};
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
const MAX_PROMPT_BYTES: usize = 32 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PersonalAgentConfig {
    recent_turn_limit: i64,
    lease_seconds: i64,
    poll_interval: Duration,
}

impl PersonalAgentConfig {
    pub fn new(
        recent_turn_limit: i64,
        lease_seconds: i64,
        poll_interval_seconds: u64,
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
        Ok(Self {
            recent_turn_limit,
            lease_seconds,
            poll_interval: Duration::from_secs(poll_interval_seconds),
        })
    }
}

#[derive(Clone)]
pub(crate) struct PersonalAgentApiState {
    store: PgStore,
    agent_refs: Arc<BTreeSet<String>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CreatePersonalAgentBody {
    name: String,
    agent_ref: String,
    instructions: String,
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

pub fn protected_router(
    store: PgStore,
    agent_refs: impl IntoIterator<Item = String>,
    browser_auth: BrowserAuthService,
) -> Router {
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
        .with_state(PersonalAgentApiState {
            store,
            agent_refs: Arc::new(agent_refs.into_iter().collect()),
        });
    protect_browser_routes(routes, browser_auth)
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
        .create_personal_agent(StoreCreatePersonalAgentRequest {
            id: Uuid::new_v4(),
            owner_user_id: &session.principal.canonical_user_id,
            name: &body.name,
            agent_ref: &body.agent_ref,
            instructions: &body.instructions,
            envelope_request_id: body.envelope_request_id,
        })
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
        StoreError::PersonalAgentNotFound | StoreError::PersonalAgentEnvelopeNotFound => {
            agent_not_found()
        }
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
    let worker = format!("personal-agent-dispatcher:{}", Uuid::new_v4());
    loop {
        match store
            .claim_personal_agent_turn(&worker, config.lease_seconds, config.recent_turn_limit)
            .await
        {
            Ok(Some(dispatch)) => run_turn(&store, &runner, &worker, config, dispatch).await,
            Ok(None) | Err(_) => tokio::time::sleep(config.poll_interval).await,
        }
    }
}

async fn run_turn(
    store: &PgStore,
    runner: &PersonalAgentTaskRunner<PgStore>,
    worker: &str,
    config: PersonalAgentConfig,
    dispatch: PersonalAgentDispatchRecord,
) {
    let Some(envelope) = dispatch
        .envelope_request
        .as_ref()
        .filter(|envelope| envelope.status == EnvelopeRequestStatus::Provisioned)
    else {
        let _ = store
            .fail_personal_agent_turn(
                dispatch.turn.id,
                worker,
                "This agent no longer has active access. Choose an active Envelope.",
            )
            .await;
        return;
    };
    let Some(prompt) = turn_prompt(&dispatch) else {
        let _ = store
            .fail_personal_agent_turn(
                dispatch.turn.id,
                worker,
                "This conversation is too large for one turn. Start a new personal agent.",
            )
            .await;
        return;
    };
    let task_uid = match dispatch.turn.task_uid {
        Some(task_uid) => task_uid,
        None => match runner
            .submit(
                dispatch.turn.id,
                dispatch.owner_user_id.clone(),
                dispatch.owner_display_email.clone(),
                &dispatch.agent.agent_ref,
                &prompt,
                envelope,
            )
            .await
        {
            Ok(task_uid) => {
                if store
                    .attach_personal_agent_task(dispatch.turn.id, worker, task_uid)
                    .await
                    .is_err()
                {
                    return;
                }
                task_uid
            }
            Err(PersonalAgentTaskError::Rejected) => {
                let _ = store
                    .fail_personal_agent_turn(
                        dispatch.turn.id,
                        worker,
                        "This turn could not start with the selected coding agent and Envelope.",
                    )
                    .await;
                return;
            }
            Err(PersonalAgentTaskError::Unavailable) => return,
        },
    };
    loop {
        if store
            .renew_personal_agent_turn_lease(dispatch.turn.id, worker, config.lease_seconds)
            .await
            .is_err()
        {
            return;
        }
        match runner.status(task_uid, &dispatch.owner_user_id).await {
            Ok(PersonalAgentTaskState::Pending) => {
                tokio::time::sleep(config.poll_interval).await;
            }
            Ok(PersonalAgentTaskState::Done(reply)) => {
                let _ = store
                    .complete_personal_agent_turn(dispatch.turn.id, worker, &reply)
                    .await;
                return;
            }
            Ok(PersonalAgentTaskState::Failed(reason)) => {
                let _ = store
                    .fail_personal_agent_turn(dispatch.turn.id, worker, &reason)
                    .await;
                return;
            }
            Err(PersonalAgentTaskError::Rejected) => {
                let _ = store
                    .fail_personal_agent_turn(
                        dispatch.turn.id,
                        worker,
                        "The coding agent did not return a usable reply. Try again.",
                    )
                    .await;
                return;
            }
            Err(PersonalAgentTaskError::Unavailable) => return,
        }
    }
}

fn turn_prompt(dispatch: &PersonalAgentDispatchRecord) -> Option<String> {
    let mut history = dispatch.recent_turns.as_slice();
    loop {
        let mut prompt = String::from("Follow these personal agent instructions:\n");
        prompt.push_str(&dispatch.agent.instructions);
        prompt.push_str("\n\nRecent conversation:\n");
        for turn in history {
            for message in &turn.messages {
                prompt.push_str("User: ");
                prompt.push_str(&message.content);
                prompt.push('\n');
            }
            if let Some(reply) = &turn.reply {
                prompt.push_str("Agent: ");
                prompt.push_str(&reply.content);
                prompt.push('\n');
            }
        }
        prompt.push_str("\nNew messages:\n");
        for message in &dispatch.turn.messages {
            prompt.push_str("User: ");
            prompt.push_str(&message.content);
            prompt.push('\n');
        }
        prompt.push_str(
            "\nAnswer the new messages. Do not use tools. Write only the final reply to out/reply.txt.",
        );
        if prompt.len() <= MAX_PROMPT_BYTES {
            return Some(prompt);
        }
        history = history.get(1..)?;
    }
}

#[cfg(test)]
mod tests {
    use super::{PersonalAgentConfig, PersonalAgentDispatchRecord, turn_prompt};
    use crate::tasks::personal_agent_task_definition;
    use steward_admission::{Envelope, EnvelopeSpec};
    use steward_store::{
        PersonalAgentMessageRecord, PersonalAgentRecord, PersonalAgentTurnRecord,
        PersonalAgentTurnStatus,
    };
    use steward_types::{
        Budget, CanonicalUserId, Duration, Email, ModelRef, RunnerRequirements, ToolGrant,
    };
    use uuid::Uuid;

    #[test]
    fn configuration_refuses_unbounded_or_unsafe_dispatch_values() {
        assert!(PersonalAgentConfig::new(0, 30, 1).is_err());
        assert!(PersonalAgentConfig::new(101, 30, 1).is_err());
        assert!(PersonalAgentConfig::new(8, 4, 1).is_err());
        assert!(PersonalAgentConfig::new(8, 30, 16).is_err());
        assert!(PersonalAgentConfig::new(8, 30, 1).is_ok());
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
        };
        let dispatch = PersonalAgentDispatchRecord {
            agent: PersonalAgentRecord {
                id: Uuid::new_v4(),
                name: "Helper".to_owned(),
                agent_ref: "example-agent@1.0.0".to_owned(),
                instructions: "Be concise.".to_owned(),
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
        let prompt = turn_prompt(&dispatch).ok_or("prompt")?;
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
                llms: vec![ModelRef {
                    provider: "future-provider".to_owned(),
                    model: "future-model".to_owned(),
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
        };
        let definition =
            personal_agent_task_definition("future-agent@9.0.0", "Answer the user.", &envelope)
                .map_err(|error| format!("build personal agent definition: {error}"))?;
        assert_eq!(definition.runtime.agent_ref.as_str(), "future-agent@9.0.0");
        let requirements = definition.requires.ok_or("requirements")?;
        assert_eq!(requirements.authority.llms.len(), 1);
        assert_eq!(
            requirements.authority.llms[0].provider.as_str(),
            "future-provider"
        );
        assert_eq!(
            requirements.authority.llms[0].model.as_str(),
            "future-model"
        );
        assert!(requirements.authority.tools.is_empty());
        Ok(())
    }
}
