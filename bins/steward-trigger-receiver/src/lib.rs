use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ring::hmac;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use steward_apiserver::WorkflowReference;
use steward_apiserver::browser_admin::{CapabilityCatalog, ToolAccessClass};
use steward_types::task_output_archive::{TarEntryKind, append_tar_entry, finish_tar_archive};
use steward_types::{CanonicalUserId, ToolGrant};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;

pub mod client;

const SIGNATURE_PREFIX: &str = "sha256=";

#[derive(Clone, Copy, Debug)]
pub struct ReceiverLimits {
    pub max_config_bytes: usize,
    pub max_body_bytes: usize,
    pub max_identifier_bytes: usize,
    pub max_triggers: usize,
    pub max_ignored_accounts: usize,
    pub max_concurrency: usize,
    pub max_cooldown_seconds: u64,
    pub max_recent_deliveries: usize,
    pub max_issue_cooldowns: usize,
    pub max_signing_secret_bytes: usize,
}

impl ReceiverLimits {
    fn validate(&self) -> Result<(), String> {
        let counts = [
            self.max_config_bytes,
            self.max_body_bytes,
            self.max_identifier_bytes,
            self.max_triggers,
            self.max_ignored_accounts,
            self.max_concurrency,
            self.max_recent_deliveries,
            self.max_issue_cooldowns,
            self.max_signing_secret_bytes,
        ];
        if counts.contains(&0) || self.max_cooldown_seconds == 0 {
            return Err("Trigger limits must be greater than zero".to_owned());
        }
        Ok(())
    }
}

pub type ReceiverFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReceiverConfig {
    pub triggers: Vec<TriggerConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TriggerConfig {
    pub id: String,
    pub enabled: bool,
    pub source: String,
    pub workflow: String,
    pub service: String,
    pub owner_user_id: CanonicalUserId,
    pub envelope_digest: String,
    pub signing_secret_file: String,
    #[serde(default)]
    pub filters: TriggerFilters,
    #[serde(default)]
    pub ignored_account_ids: BTreeSet<String>,
    pub concurrency_limit: usize,
    pub cooldown_seconds: u64,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TriggerFilters {
    pub event: Option<String>,
    pub project: Option<String>,
    pub issue_type: Option<String>,
}

impl ReceiverConfig {
    pub fn from_json(value: &str, limits: &ReceiverLimits) -> Result<Self, String> {
        limits.validate()?;
        if value.len() > limits.max_config_bytes {
            return Err("Trigger configuration is too large".to_owned());
        }
        let config: Self = serde_json::from_str(value)
            .map_err(|_| "Trigger configuration is invalid".to_owned())?;
        config.validate(limits)?;
        Ok(config)
    }

    fn validate(&self, limits: &ReceiverLimits) -> Result<(), String> {
        if self.triggers.is_empty() || self.triggers.len() > limits.max_triggers {
            return Err("Trigger configuration has an invalid entry count".to_owned());
        }
        let mut ids = BTreeSet::new();
        for trigger in &self.triggers {
            let exact = [
                &trigger.workflow,
                &trigger.service,
                &trigger.signing_secret_file,
            ];
            if trigger.source != "jira"
                || !valid_slug(&trigger.id, limits.max_identifier_bytes)
                || exact
                    .into_iter()
                    .any(|value| !valid_exact(value, limits.max_identifier_bytes))
                || WorkflowReference::parse(&trigger.workflow).is_err()
                || !valid_envelope_digest(&trigger.envelope_digest)
                || trigger
                    .filters
                    .event
                    .as_ref()
                    .is_some_and(|value| !valid_jira_event(value, limits.max_identifier_bytes))
                || trigger
                    .filters
                    .project
                    .as_ref()
                    .is_some_and(|value| !valid_project_key(value, limits.max_identifier_bytes))
                || trigger
                    .filters
                    .issue_type
                    .as_ref()
                    .is_some_and(|value| !valid_numeric_id(value, limits.max_identifier_bytes))
                || trigger.ignored_account_ids.len() > limits.max_ignored_accounts
                || trigger
                    .ignored_account_ids
                    .iter()
                    .any(|value| !valid_account_id(value, limits.max_identifier_bytes))
                || trigger.concurrency_limit == 0
                || trigger.concurrency_limit > limits.max_concurrency
                || trigger.cooldown_seconds > limits.max_cooldown_seconds
                || !ids.insert(&trigger.id)
            {
                return Err("Trigger configuration contains an invalid entry".to_owned());
            }
        }
        Ok(())
    }
}

fn valid_exact(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn valid_slug(value: &str, max_bytes: usize) -> bool {
    valid_exact(value, max_bytes)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn valid_jira_event(value: &str, max_bytes: usize) -> bool {
    valid_exact(value, max_bytes)
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b':' | b'_')
        })
}

fn valid_project_key(value: &str, max_bytes: usize) -> bool {
    let mut bytes = value.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_uppercase())
        && bytes.all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        && valid_exact(value, max_bytes)
}

fn valid_issue_key(value: &str, max_bytes: usize) -> bool {
    value.rsplit_once('-').is_some_and(|(project, number)| {
        valid_project_key(project, max_bytes)
            && !number.is_empty()
            && number.bytes().all(|byte| byte.is_ascii_digit())
            && valid_exact(value, max_bytes)
    })
}

fn valid_account_id(value: &str, max_bytes: usize) -> bool {
    valid_exact(value, max_bytes)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b':'))
}

fn valid_numeric_id(value: &str, max_bytes: usize) -> bool {
    valid_exact(value, max_bytes) && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn valid_envelope_digest(value: &str) -> bool {
    valid_hash(value, "steward:sha256:")
}

fn valid_hash(value: &str, prefix: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    })
}

pub trait EnvelopeAuthority: Send + Sync + 'static {
    fn active_tools<'a>(
        &'a self,
        owner: &'a CanonicalUserId,
        digest: &'a str,
    ) -> ReceiverFuture<'a, Result<Vec<ToolGrant>, EnvelopeError>>;
}

pub trait CapabilityCatalogReader: Send + Sync + 'static {
    fn current(&self) -> ReceiverFuture<'_, Result<CapabilityCatalog, String>>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnvelopeError {
    Denied,
    Absent,
    Ambiguous,
    Stale,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskInvocation {
    pub workflow: String,
    pub envelope_digest: String,
    pub idempotency_key: String,
    pub input_archive: Vec<u8>,
}

impl TaskInvocation {
    pub fn validate_for(&self, trigger: &TriggerConfig) -> bool {
        self.workflow == trigger.workflow
            && self.envelope_digest == trigger.envelope_digest
            && valid_hash(&self.idempotency_key, "jira-sha256:")
            && valid_event_archive(&self.input_archive)
    }
}

pub trait TaskLifecycle: Send + Sync + 'static {
    fn start<'a>(
        &'a self,
        trigger: &'a TriggerConfig,
        invocation: TaskInvocation,
    ) -> ReceiverFuture<'a, Result<TaskStart, TaskLifecycleError>>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskLifecycleError {
    Retriable,
    Final,
}

pub enum TaskStart {
    Started(ReceiverFuture<'static, Result<(), TaskLifecycleError>>),
    Duplicate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebhookResult {
    Accepted,
    Noop,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WebhookError {
    NotFound,
    Unauthorized,
    Invalid(&'static str),
    Busy,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeliveryState {
    Pending,
    Accepted,
}

struct CooldownReservation {
    delivery_id: String,
    started_at: Instant,
}

#[derive(Default)]
struct TriggerState {
    deliveries: BTreeMap<String, DeliveryState>,
    accepted_order: VecDeque<String>,
    issue_cooldowns: BTreeMap<String, CooldownReservation>,
}

struct LiveTrigger {
    config: TriggerConfig,
    signing_key: hmac::Key,
    state: Mutex<TriggerState>,
    concurrency: Arc<Semaphore>,
}

struct PendingDelivery {
    live: Arc<LiveTrigger>,
    replay_key: String,
    issue_key: String,
    armed: bool,
}

impl PendingDelivery {
    fn new(live: Arc<LiveTrigger>, replay_key: String, issue_key: String) -> Self {
        Self {
            live,
            replay_key,
            issue_key,
            armed: true,
        }
    }

    async fn rollback(&mut self) {
        rollback_pending(
            self.live.clone(),
            self.replay_key.clone(),
            self.issue_key.clone(),
        )
        .await;
        self.armed = false;
    }

    async fn accept(&mut self) {
        let mut state = self.live.state.lock().await;
        state
            .deliveries
            .insert(self.replay_key.clone(), DeliveryState::Accepted);
        state.accepted_order.push_back(self.replay_key.clone());
        self.armed = false;
    }
}

impl Drop for PendingDelivery {
    fn drop(&mut self) {
        if self.armed {
            if let Ok(mut state) = self.live.state.try_lock() {
                rollback_pending_state(&mut state, &self.replay_key, &self.issue_key);
                return;
            }
            let live = self.live.clone();
            let replay_key = self.replay_key.clone();
            let issue_key = self.issue_key.clone();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move { rollback_pending(live, replay_key, issue_key).await });
            }
        }
    }
}

async fn rollback_pending(live: Arc<LiveTrigger>, replay_key: String, issue_key: String) {
    let mut state = live.state.lock().await;
    rollback_pending_state(&mut state, &replay_key, &issue_key);
}

fn rollback_pending_state(state: &mut TriggerState, replay_key: &str, issue_key: &str) {
    if state.deliveries.get(replay_key) == Some(&DeliveryState::Pending) {
        state.deliveries.remove(replay_key);
    }
    if state
        .issue_cooldowns
        .get(issue_key)
        .is_some_and(|reservation| reservation.delivery_id == replay_key)
    {
        state.issue_cooldowns.remove(issue_key);
    }
}

#[derive(Clone)]
pub struct Receiver {
    triggers: Arc<BTreeMap<String, Arc<LiveTrigger>>>,
    catalogs: Arc<dyn CapabilityCatalogReader>,
    envelopes: Arc<dyn EnvelopeAuthority>,
    tasks: Arc<dyn TaskLifecycle>,
    limits: ReceiverLimits,
}

impl Receiver {
    pub async fn new<C: CapabilityCatalogReader, E: EnvelopeAuthority, T: TaskLifecycle>(
        config: ReceiverConfig,
        secrets: BTreeMap<String, Vec<u8>>,
        catalogs: C,
        envelopes: E,
        tasks: T,
        limits: ReceiverLimits,
    ) -> Result<Self, String> {
        limits.validate()?;
        config.validate(&limits)?;
        let triggers = config
            .triggers
            .into_iter()
            .map(|config| {
                let id = config.id.clone();
                let secret = secrets.get(&id);
                if config.enabled
                    && secret.is_none_or(|secret| {
                        secret.is_empty() || secret.len() > limits.max_signing_secret_bytes
                    })
                {
                    return Err("a Trigger signing secret is unavailable".to_owned());
                }
                let live = LiveTrigger {
                    signing_key: hmac::Key::new(
                        hmac::HMAC_SHA256,
                        secret.map(Vec::as_slice).unwrap_or_default(),
                    ),
                    concurrency: Arc::new(Semaphore::new(config.concurrency_limit)),
                    config,
                    state: Mutex::new(TriggerState::default()),
                };
                Ok((id, Arc::new(live)))
            })
            .collect::<Result<BTreeMap<_, _>, String>>()?;
        let receiver = Self {
            triggers: Arc::new(triggers),
            catalogs: Arc::new(catalogs),
            envelopes: Arc::new(envelopes),
            tasks: Arc::new(tasks),
            limits,
        };
        for live in receiver
            .triggers
            .values()
            .filter(|live| live.config.enabled)
        {
            receiver
                .validate_authority(&live.config)
                .await
                .map_err(|_| "a Trigger Envelope is unavailable or unsafe".to_owned())?;
        }
        Ok(receiver)
    }

    pub async fn receive(
        &self,
        trigger_id: &str,
        signature: Option<&str>,
        body: &[u8],
    ) -> Result<WebhookResult, WebhookError> {
        let live = self
            .triggers
            .get(trigger_id)
            .cloned()
            .ok_or(WebhookError::NotFound)?;
        let trigger = &live.config;
        if !trigger.enabled {
            return Err(WebhookError::NotFound);
        }
        if body.len() > self.limits.max_body_bytes {
            return Err(WebhookError::Invalid("The request is too large."));
        }
        if !verify_jira_signature_with_key(&live.signing_key, signature, body) {
            return Err(WebhookError::Unauthorized);
        }
        let event = JiraEvent::parse(body, self.limits.max_identifier_bytes)?;
        if !trigger.filters.matches(&event)
            || trigger
                .ignored_account_ids
                .contains(&event.author_account_id)
        {
            return Ok(WebhookResult::Noop);
        }
        let replay_key = idempotency_key(&trigger.id, body)?;
        let (invocation, issue_key, permit) = {
            let input_archive = event.archive()?;
            let mut state = live.state.lock().await;
            match state.deliveries.get(&replay_key) {
                Some(DeliveryState::Accepted) => return Ok(WebhookResult::Noop),
                Some(DeliveryState::Pending) => return Err(WebhookError::Busy),
                None => {}
            }
            let cooldown = Duration::from_secs(trigger.cooldown_seconds);
            state
                .issue_cooldowns
                .retain(|_, reservation| reservation.started_at.elapsed() < cooldown);
            if state.issue_cooldowns.contains_key(&event.issue_key) {
                return Err(WebhookError::Busy);
            }
            if state.issue_cooldowns.len() == self.limits.max_issue_cooldowns {
                return Err(WebhookError::Busy);
            }
            let permit = live
                .concurrency
                .clone()
                .try_acquire_owned()
                .map_err(|_| WebhookError::Busy)?;
            if state.deliveries.len() == self.limits.max_recent_deliveries {
                let accepted = state.accepted_order.pop_front().ok_or(WebhookError::Busy)?;
                state.deliveries.remove(&accepted);
            }
            state
                .deliveries
                .insert(replay_key.clone(), DeliveryState::Pending);
            state.issue_cooldowns.insert(
                event.issue_key.clone(),
                CooldownReservation {
                    delivery_id: replay_key.clone(),
                    started_at: Instant::now(),
                },
            );
            (
                TaskInvocation {
                    workflow: trigger.workflow.clone(),
                    envelope_digest: trigger.envelope_digest.clone(),
                    idempotency_key: replay_key.clone(),
                    input_archive,
                },
                event.issue_key,
                permit,
            )
        };
        let worker = self.clone();
        let result = tokio::spawn(async move {
            worker
                .start(
                    PendingDelivery::new(live, replay_key, issue_key),
                    invocation,
                    permit,
                )
                .await
        })
        .await;
        match result {
            Ok(result) => result,
            Err(_) => Err(WebhookError::Failed),
        }
    }

    async fn start(
        &self,
        mut pending: PendingDelivery,
        invocation: TaskInvocation,
        permit: OwnedSemaphorePermit,
    ) -> Result<WebhookResult, WebhookError> {
        let trigger = &pending.live.config;
        let started = async {
            self.validate_authority(trigger).await?;
            self.tasks
                .start(trigger, invocation)
                .await
                .map_err(|error| match error {
                    TaskLifecycleError::Retriable => WebhookError::Failed,
                    TaskLifecycleError::Final => {
                        WebhookError::Invalid("The Task request was refused.")
                    }
                })
        }
        .await;
        let start = match started {
            Ok(start) => start,
            Err(error) => {
                pending.rollback().await;
                return Err(error);
            }
        };
        pending.accept().await;
        match start {
            TaskStart::Duplicate => Ok(WebhookResult::Noop),
            TaskStart::Started(completion) => {
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Err(error) = completion.await {
                        eprintln!("accepted Task lifecycle ended with a {error:?} failure");
                    }
                });
                Ok(WebhookResult::Accepted)
            }
        }
    }

    async fn validate_authority(&self, trigger: &TriggerConfig) -> Result<(), WebhookError> {
        let tools = self
            .envelopes
            .active_tools(&trigger.owner_user_id, &trigger.envelope_digest)
            .await
            .map_err(|error| match error {
                EnvelopeError::Denied => WebhookError::Invalid("This Trigger is not read-only."),
                EnvelopeError::Absent | EnvelopeError::Ambiguous | EnvelopeError::Stale => {
                    WebhookError::Invalid("The configured Envelope cannot be used.")
                }
                EnvelopeError::Unavailable => WebhookError::Failed,
            })?;
        let catalog = self
            .catalogs
            .current()
            .await
            .map_err(|_| WebhookError::Failed)?;
        catalog.validate().map_err(|_| WebhookError::Failed)?;
        if tools.iter().any(|grant| {
            catalog
                .tools
                .iter()
                .find(|tool| tool.grants(grant))
                .is_none_or(|tool| tool.access_class != ToolAccessClass::Read)
        }) {
            return Err(WebhookError::Invalid("This Trigger is not read-only."));
        }
        Ok(())
    }
}

pub fn router(receiver: Receiver) -> Router {
    let body_limit = receiver.limits.max_body_bytes;
    Router::new()
        .route("/hooks/jira/{trigger}", post(receive_route))
        .layer(DefaultBodyLimit::max(body_limit))
        .with_state(receiver)
}

async fn receive_route(
    State(receiver): State<Receiver>,
    Path(trigger): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let signature = headers
        .get("x-hub-signature")
        .and_then(|value| value.to_str().ok());
    let (status, message) = match receiver.receive(&trigger, signature, body.as_ref()).await {
        Ok(WebhookResult::Accepted) => (StatusCode::ACCEPTED, "The event was accepted."),
        Ok(WebhookResult::Noop) => (StatusCode::OK, "No run was needed."),
        Err(WebhookError::NotFound) => (StatusCode::NOT_FOUND, "This Trigger is unavailable."),
        Err(WebhookError::Unauthorized) => (
            StatusCode::UNAUTHORIZED,
            "The request signature is invalid.",
        ),
        Err(WebhookError::Invalid(message)) => (StatusCode::BAD_REQUEST, message),
        Err(WebhookError::Busy) => (
            StatusCode::TOO_MANY_REQUESTS,
            "This Trigger cannot start another run yet.",
        ),
        Err(WebhookError::Failed) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "The event could not be processed. Try again later.",
        ),
    };
    (status, message).into_response()
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct JiraEvent {
    event: String,
    issue_key: String,
    project_key: String,
    issue_type: String,
    author_account_id: String,
}

impl JiraEvent {
    fn validate(&self, max_identifier_bytes: usize) -> bool {
        valid_jira_event(&self.event, max_identifier_bytes)
            && valid_issue_key(&self.issue_key, max_identifier_bytes)
            && valid_project_key(&self.project_key, max_identifier_bytes)
            && valid_numeric_id(&self.issue_type, max_identifier_bytes)
            && valid_account_id(&self.author_account_id, max_identifier_bytes)
            && self
                .issue_key
                .rsplit_once('-')
                .is_some_and(|(project, _)| project == self.project_key)
    }

    fn parse(body: &[u8], max_identifier_bytes: usize) -> Result<Self, WebhookError> {
        let value: serde_json::Value = serde_json::from_slice(body)
            .map_err(|_| WebhookError::Invalid("The Jira event is invalid."))?;
        let field = |path: &[&str]| {
            path.iter()
                .try_fold(&value, |value, key| value.get(*key))
                .and_then(serde_json::Value::as_str)
                .filter(|value| valid_exact(value, max_identifier_bytes))
                .map(str::to_owned)
        };
        let event =
            field(&["webhookEvent"]).filter(|value| valid_jira_event(value, max_identifier_bytes));
        let issue_key =
            field(&["issue", "key"]).filter(|value| valid_issue_key(value, max_identifier_bytes));
        let project_key = field(&["issue", "fields", "project", "key"])
            .filter(|value| valid_project_key(value, max_identifier_bytes));
        let issue_type = field(&["issue", "fields", "issuetype", "id"])
            .filter(|value| valid_numeric_id(value, max_identifier_bytes));
        let author_account_id = field(&["user", "accountId"])
            .filter(|value| valid_account_id(value, max_identifier_bytes));
        let incomplete = WebhookError::Invalid("The Jira event is incomplete or invalid.");
        let issue_key = issue_key.ok_or_else(|| incomplete.clone())?;
        let project_key = project_key.ok_or_else(|| incomplete.clone())?;
        if issue_key
            .rsplit_once('-')
            .is_none_or(|(project, _)| project != project_key)
        {
            return Err(incomplete);
        }
        Ok(Self {
            event: event.ok_or_else(|| incomplete.clone())?,
            issue_key,
            project_key,
            issue_type: issue_type.ok_or_else(|| incomplete.clone())?,
            author_account_id: author_account_id.ok_or(incomplete)?,
        })
    }

    fn archive(&self) -> Result<Vec<u8>, WebhookError> {
        let bytes = serde_json::to_vec(self).map_err(|_| WebhookError::Failed)?;
        let mut archive = Vec::new();
        append_tar_entry(&mut archive, "in/event.json", TarEntryKind::File, &bytes);
        finish_tar_archive(&mut archive);
        Ok(archive)
    }
}

fn valid_event_archive(archive: &[u8]) -> bool {
    let Some(header) = archive.get(..512) else {
        return false;
    };
    let Some(size) = std::str::from_utf8(&header[124..136])
        .ok()
        .map(|value| value.trim_matches(['\0', ' ']))
        .and_then(|value| usize::from_str_radix(value, 8).ok())
    else {
        return false;
    };
    archive
        .get(512..512_usize.saturating_add(size))
        .and_then(|bytes| serde_json::from_slice::<JiraEvent>(bytes).ok())
        .filter(|event| event.validate(usize::MAX))
        .and_then(|event| event.archive().ok())
        .is_some_and(|expected| expected == archive)
}

impl TriggerFilters {
    fn matches(&self, event: &JiraEvent) -> bool {
        [
            (&self.event, &event.event),
            (&self.project, &event.project_key),
            (&self.issue_type, &event.issue_type),
        ]
        .into_iter()
        .all(|(filter, value)| filter.as_ref().is_none_or(|filter| filter == value))
    }
}

pub fn verify_jira_signature(secret: &[u8], signature: Option<&str>, body: &[u8]) -> bool {
    verify_jira_signature_with_key(&hmac::Key::new(hmac::HMAC_SHA256, secret), signature, body)
}

fn verify_jira_signature_with_key(key: &hmac::Key, signature: Option<&str>, body: &[u8]) -> bool {
    let Some(signature) = signature.and_then(|value| value.strip_prefix(SIGNATURE_PREFIX)) else {
        return false;
    };
    let mut bytes = [0_u8; 32];
    if signature.len() != bytes.len() * 2 {
        return false;
    }
    for (index, pair) in signature.as_bytes().chunks_exact(2).enumerate() {
        let (Some(high), Some(low)) = (hex_digit(pair[0]), hex_digit(pair[1])) else {
            return false;
        };
        bytes[index] = high * 16 + low;
    }
    hmac::verify(key, body, &bytes).is_ok()
}

fn hex_digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
}

fn idempotency_key(trigger: &str, body: &[u8]) -> Result<String, WebhookError> {
    let value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|_| WebhookError::Invalid("The Jira event is invalid."))?;
    let canonical = serde_json::to_vec(&value).map_err(|_| WebhookError::Failed)?;
    let mut digest = Sha256::new();
    digest.update(b"steward-jira-trigger-delivery/v2\0");
    digest.update(trigger.as_bytes());
    digest.update(b"\0");
    digest.update(canonical);
    Ok(format!("jira-sha256:{:x}", digest.finalize()))
}

#[cfg(test)]
mod tests {
    use std::collections::btree_map::Entry;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::{WebhookError as E, WebhookResult as W, *};
    use axum::body::Body;
    use axum::http::Request;
    use steward_apiserver::browser_admin::CapabilityTool;
    use tokio::sync::Notify;
    use tower::ServiceExt;

    const OWNER: &str = "usr_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const DIGEST: &str =
        "steward:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[derive(Clone)]
    struct Envelopes(Arc<StdMutex<Result<Vec<ToolGrant>, EnvelopeError>>>);

    impl EnvelopeAuthority for Envelopes {
        fn active_tools<'a>(
            &'a self,
            owner: &'a CanonicalUserId,
            digest: &'a str,
        ) -> ReceiverFuture<'a, Result<Vec<ToolGrant>, EnvelopeError>> {
            Box::pin(async move {
                if owner.as_str() != OWNER || digest != DIGEST {
                    return Err(EnvelopeError::Denied);
                }
                self.0
                    .lock()
                    .map_err(|_| EnvelopeError::Unavailable)?
                    .clone()
            })
        }
    }

    #[derive(Clone)]
    struct Catalogs(Arc<StdMutex<Result<CapabilityCatalog, String>>>);

    impl CapabilityCatalogReader for Catalogs {
        fn current(&self) -> ReceiverFuture<'_, Result<CapabilityCatalog, String>> {
            Box::pin(async move {
                self.0
                    .lock()
                    .map_err(|_| "Capability catalog fixture is unavailable".to_owned())?
                    .clone()
            })
        }
    }

    #[derive(Clone, Default)]
    struct Tasks {
        calls: Arc<StdMutex<Vec<TaskInvocation>>>,
        seen: Arc<StdMutex<BTreeMap<String, TaskInvocation>>>,
        blocks: bool,
        entered: Option<Arc<Notify>>,
        gate: Option<Arc<Notify>>,
        recorded: Option<Arc<Notify>>,
        response_gate: Option<Arc<Notify>>,
        fail_start: Arc<AtomicBool>,
        panic_start: Arc<AtomicBool>,
    }

    impl TaskLifecycle for Tasks {
        fn start<'a>(
            &'a self,
            trigger: &'a super::TriggerConfig,
            invocation: TaskInvocation,
        ) -> ReceiverFuture<'a, Result<TaskStart, TaskLifecycleError>> {
            Box::pin(async move {
                if !invocation.validate_for(trigger) {
                    return Err(TaskLifecycleError::Final);
                }
                let fail = self.fail_start.load(Ordering::SeqCst);
                if let Some(entered) = &self.entered {
                    entered.notify_one();
                }
                if let Some(gate) = &self.gate {
                    gate.notified().await;
                }
                if fail {
                    return Err(TaskLifecycleError::Retriable);
                }
                if self.panic_start.load(Ordering::SeqCst) {
                    std::panic::resume_unwind(Box::new("task lifecycle fixture panicked"));
                }
                self.calls
                    .lock()
                    .map_err(|_| TaskLifecycleError::Retriable)?
                    .push(invocation.clone());
                let duplicate = {
                    let mut seen = self
                        .seen
                        .lock()
                        .map_err(|_| TaskLifecycleError::Retriable)?;
                    match seen.entry(invocation.idempotency_key.clone()) {
                        Entry::Occupied(entry) if entry.get() == &invocation => true,
                        Entry::Occupied(_) => return Err(TaskLifecycleError::Final),
                        Entry::Vacant(entry) => {
                            _ = entry.insert(invocation);
                            false
                        }
                    }
                };
                if let Some(recorded) = &self.recorded {
                    recorded.notify_one();
                }
                if let Some(response_gate) = &self.response_gate {
                    response_gate.notified().await;
                }
                if duplicate {
                    return Ok(TaskStart::Duplicate);
                }
                let blocks = self.blocks;
                Ok(TaskStart::Started(Box::pin(async move {
                    if blocks {
                        std::future::pending::<()>().await;
                    }
                    Ok(())
                })))
            })
        }
    }

    fn signature(secret: &[u8], body: &[u8]) -> String {
        let bytes = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, secret), body);
        let mut value = String::from("sha256=");
        for byte in bytes.as_ref() {
            value.push_str(&format!("{byte:02x}"));
        }
        value
    }

    fn config_json(ignored: &str, concurrency: usize, cooldown: u64) -> String {
        format!(
            r#"{{"triggers":[{{"id":"jira-read","enabled":true,"source":"jira","workflow":"triage@1","service":"event-worker","ownerUserId":"{OWNER}","envelopeDigest":"{DIGEST}","signingSecretFile":"/var/run/secrets/jira","concurrencyLimit":2,"cooldownSeconds":60}}]}}"#
        )
        .replace("\"concurrencyLimit\":2", &format!("\"concurrencyLimit\":{concurrency}"))
        .replace("\"cooldownSeconds\":60", &format!("\"cooldownSeconds\":{cooldown}"))
        .replace("\"signingSecretFile\"", &format!("\"ignoredAccountIds\":{ignored},\"signingSecretFile\""))
    }

    fn body(author: &str) -> Vec<u8> {
        body_for(author, "PROJ-123")
    }

    fn body_for(author: &str, issue: &str) -> Vec<u8> {
        body_fields("jira:issue_updated", author, issue, "PROJ", "10001")
    }

    fn body_fields(
        event: &str,
        author: &str,
        issue: &str,
        project: &str,
        issue_type: &str,
    ) -> Vec<u8> {
        format!(r#"{{"webhookEvent":"{event}","issue":{{"key":"{issue}","fields":{{"project":{{"key":"{project}"}},"issuetype":{{"id":"{issue_type}","name":"free text"}}}}}},"user":{{"accountId":"{author}"}},"ignored":"event text"}}"#).into_bytes()
    }

    fn catalog(class: ToolAccessClass) -> CapabilityCatalog {
        CapabilityCatalog {
            schema_version: "steward.capability-catalog/v2".to_owned(),
            models: Vec::new(),
            tools: vec![CapabilityTool {
                provider: "jira".to_owned(),
                resource: "issue".to_owned(),
                action: "read".to_owned(),
                access_class: class,
                toolsets: Vec::new(),
            }],
            catalogs: Vec::new(),
        }
    }

    fn catalogs(class: ToolAccessClass) -> Catalogs {
        Catalogs(Arc::new(StdMutex::new(Ok(catalog(class)))))
    }

    fn grant() -> ToolGrant {
        ToolGrant {
            provider: "jira".to_owned(),
            resource: "issue".to_owned(),
            action: "read".to_owned(),
        }
    }

    fn envelopes() -> Envelopes {
        Envelopes(Arc::new(StdMutex::new(Ok(vec![grant()]))))
    }

    fn limits() -> ReceiverLimits {
        ReceiverLimits {
            max_config_bytes: 64 * 1024,
            max_body_bytes: 8 * 1024,
            max_identifier_bytes: 128,
            max_triggers: 4,
            max_ignored_accounts: 4,
            max_concurrency: 4,
            max_cooldown_seconds: 300,
            max_recent_deliveries: 8,
            max_issue_cooldowns: 2,
            max_signing_secret_bytes: 64,
        }
    }

    async fn make_receiver(
        config: String,
        envelopes: Envelopes,
        tasks: Tasks,
    ) -> Result<Receiver, String> {
        make_receiver_with_limits(config, envelopes, tasks, limits()).await
    }

    async fn make_receiver_with_limits(
        config: String,
        envelopes: Envelopes,
        tasks: Tasks,
        limits: ReceiverLimits,
    ) -> Result<Receiver, String> {
        make_receiver_with_catalogs(
            config,
            catalogs(ToolAccessClass::Read),
            envelopes,
            tasks,
            limits,
        )
        .await
    }

    async fn make_receiver_with_catalogs(
        config: String,
        catalogs: Catalogs,
        envelopes: Envelopes,
        tasks: Tasks,
        limits: ReceiverLimits,
    ) -> Result<Receiver, String> {
        Receiver::new(
            ReceiverConfig::from_json(&config, &limits)?,
            BTreeMap::from([("jira-read".to_owned(), b"test-signing-secret".to_vec())]),
            catalogs,
            envelopes,
            tasks,
            limits,
        )
        .await
    }

    async fn send(receiver: &Receiver, delivery: &str, author: &str) -> Result<W, E> {
        send_issue(receiver, delivery, author, "PROJ-123").await
    }

    async fn send_issue(
        receiver: &Receiver,
        _delivery: &str,
        author: &str,
        issue: &str,
    ) -> Result<W, E> {
        let event = body_for(author, issue);
        receiver
            .receive(
                "jira-read",
                Some(&signature(b"test-signing-secret", &event)),
                &event,
            )
            .await
    }

    async fn expect(receiver: &Receiver, delivery: &str, author: &str, value: Result<W, E>) {
        assert_eq!(send(receiver, delivery, author).await, value);
    }

    #[test]
    fn deployment_limit_unknown_to_the_code_controls_trigger_validation() {
        let mut configured = limits();
        configured.max_concurrency = 7;
        configured.max_cooldown_seconds = 137;
        assert!(ReceiverConfig::from_json(&config_json("[]", 7, 137), &configured).is_ok());
        assert!(ReceiverConfig::from_json(&config_json("[]", 8, 137), &configured).is_err());
        assert!(ReceiverConfig::from_json(&config_json("[]", 7, 138), &configured).is_err());
    }

    #[tokio::test]
    async fn request_refusals_are_fail_closed() -> Result<(), String> {
        let valid = config_json("[\"ignored\"]", 2, 60);
        for bad in [
            valid.replace("\"jira\"", "\"other\""),
            valid.replace("\"triage@1\"", "\"triage\""),
            valid.replace("\"triage@1\"", "\"Triage@1\""),
            valid.replace("\"triage@1\"", "\"tri age@1\""),
            valid.replace("\"triage@1\"", "\"triage@x@1\""),
            valid.replace("\"triage@1\"", "\"triage@9223372036854775808\""),
            valid.replace("\"id\":\"jira-read\"", "\"id\":\"jira/read\""),
            valid.replace(
                "\"signingSecretFile\"",
                "\"filters\":{\"project\":\" PROJ\"},\"signingSecretFile\"",
            ),
            valid.replace(
                "\"signingSecretFile\"",
                "\"filters\":{\"project\":\"PROJ/KEY\"},\"signingSecretFile\"",
            ),
            valid.replace("[\"ignored\"]", "[\"alice_user\"]"),
        ] {
            assert!(ReceiverConfig::from_json(&bad, &limits()).is_err());
        }
        let underscore_filter = valid.replace(
            "\"signingSecretFile\"",
            "\"filters\":{\"project\":\"PROJ_KEY\"},\"signingSecretFile\"",
        );
        assert!(ReceiverConfig::from_json(&underscore_filter, &limits()).is_ok());
        let tools = envelopes();
        let tasks = Tasks::default();
        let r = make_receiver(valid.clone(), tools.clone(), tasks.clone()).await?;
        let event = body("alice");
        let signed = signature(b"test-signing-secret", &event);
        assert_eq!(
            r.receive("missing", Some(&signed), &event).await,
            Err(E::NotFound)
        );
        assert_eq!(
            r.receive("jira-read", None, &event).await,
            Err(E::Unauthorized)
        );
        assert_eq!(
            r.receive("jira-read", Some("sha256=bad"), &event).await,
            Err(E::Unauthorized)
        );
        for signature in [None, Some("sha256=bad")] {
            assert_eq!(
                r.receive("jira-read", signature, b"not-json").await,
                Err(E::Unauthorized)
            );
        }
        assert_eq!(
            r.receive("jira-read", Some(&signed), &event).await,
            Ok(W::Accepted)
        );
        assert!(matches!(
            r.receive("jira-read", None, &vec![b'x'; limits().max_body_bytes + 1])
                .await,
            Err(E::Invalid(_))
        ));
        assert_eq!(
            r.receive("jira-read", Some(&signed), &event).await,
            Ok(W::Noop)
        );
        let malformed = b"{}";
        assert!(matches!(
            r.receive(
                "jira-read",
                Some(&signature(b"test-signing-secret", malformed)),
                malformed
            )
            .await,
            Err(E::Invalid(_))
        ));
        let mut changed = event.clone();
        changed.push(b' ');
        assert_eq!(
            r.receive("jira-read", Some(&signed), &changed).await,
            Err(E::Unauthorized)
        );
        let disabled = valid.replace("\"enabled\":true", "\"enabled\":false");
        let r = make_receiver(disabled, tools, tasks).await?;
        expect(&r, "x", "alice", Err(E::NotFound)).await;
        let filtered = valid.replace(
            "\"signingSecretFile\"",
            "\"filters\":{\"event\":\"other\"},\"signingSecretFile\"",
        );
        let r = make_receiver(filtered, envelopes(), Tasks::default()).await?;
        expect(&r, "x", "alice", Ok(W::Noop)).await;

        let underscore = body_fields(
            "jira:issue_updated",
            "alice",
            "PROJ_KEY-123",
            "PROJ_KEY",
            "10001",
        );
        let r = make_receiver(config_json("[]", 1, 0), envelopes(), Tasks::default()).await?;
        assert_eq!(
            r.receive(
                "jira-read",
                Some(&signature(b"test-signing-secret", &underscore)),
                &underscore,
            )
            .await,
            Ok(W::Accepted)
        );

        for event in [
            body_fields("Jira event", "alice", "PROJ-123", "PROJ", "10001"),
            body_fields("jira:issue_updated", "alice", "proj-123", "PROJ", "10001"),
            body_fields("jira:issue_updated", "alice", "PROJ-123", "proj", "10001"),
            body_fields("jira:issue_updated", "alice", "PROJ-123", "OTHER", "10001"),
            body_fields(
                "jira:issue_updated",
                "alice",
                "PROJ/KEY-123",
                "PROJ/KEY",
                "10001",
            ),
            body_fields("jira:issue_updated", "alice", "PROJ-123", "PROJ", "Task"),
            body_fields(
                "jira:issue_updated",
                "alice_user",
                "PROJ-123",
                "PROJ",
                "10001",
            ),
            body_fields(
                "jira:issue_updated",
                "alice@example.com",
                "PROJ-123",
                "PROJ",
                "10001",
            ),
        ] {
            assert!(matches!(
                r.receive(
                    "jira-read",
                    Some(&signature(b"test-signing-secret", &event)),
                    &event,
                )
                .await,
                Err(E::Invalid(_))
            ));
        }
        Ok(())
    }

    #[tokio::test]
    async fn replay_cooldown_concurrency_and_authority_are_enforced() -> Result<(), String> {
        let tools = envelopes();
        let tasks = Tasks::default();
        let r = make_receiver(
            config_json("[\"ignored\"]", 2, 60),
            tools.clone(),
            tasks.clone(),
        )
        .await?;
        expect(&r, "one", "alice", Ok(W::Accepted)).await;
        expect(&r, "one", "alice", Ok(W::Noop)).await;
        expect(&r, "fresh", "alice", Ok(W::Noop)).await;
        expect(&r, "two", "bob", Err(E::Busy)).await;
        expect(&r, "three", "ignored", Ok(W::Noop)).await;
        assert_eq!(
            tasks.calls.lock().map_err(|_| "calls lock failed")?.len(),
            1
        );
        {
            let calls = tasks.calls.lock().map_err(|_| "calls lock failed")?;
            for free_text in [b"event text".as_slice(), b"free text".as_slice()] {
                assert!(
                    !calls[0]
                        .input_archive
                        .windows(free_text.len())
                        .any(|value| value == free_text)
                );
            }
        }
        let recheck = make_receiver(config_json("[]", 2, 0), tools.clone(), tasks).await?;
        *tools.0.lock().map_err(|_| "tools lock failed")? = Ok(vec![ToolGrant {
            action: "write".to_owned(),
            ..grant()
        }]);
        assert!(matches!(
            send(&recheck, "four", "bob").await,
            Err(E::Invalid(_))
        ));

        let unsafe_tools = envelopes();
        for class in [ToolAccessClass::Write, ToolAccessClass::Destructive] {
            assert!(
                Receiver::new(
                    ReceiverConfig::from_json(&config_json("[]", 1, 0), &limits())?,
                    BTreeMap::from([("jira-read".to_owned(), b"secret".to_vec())]),
                    catalogs(class),
                    unsafe_tools.clone(),
                    Tasks::default(),
                    limits(),
                )
                .await
                .is_err()
            );
        }
        let unknown = Envelopes(Arc::new(StdMutex::new(Ok(vec![ToolGrant {
            action: "unknown".to_owned(),
            ..grant()
        }]))));
        assert!(
            make_receiver(config_json("[]", 1, 0), unknown, Tasks::default())
                .await
                .is_err()
        );
        let tasks = Tasks {
            blocks: true,
            ..Tasks::default()
        };
        let r = make_receiver(config_json("[]", 1, 0), envelopes(), tasks.clone()).await?;
        expect(&r, "one", "alice", Ok(W::Accepted)).await;
        expect(&r, "two", "bob", Err(E::Busy)).await;

        let exact = Tasks::default();
        let first = make_receiver(config_json("[]", 1, 0), envelopes(), exact.clone()).await?;
        let retry = make_receiver(config_json("[]", 1, 0), envelopes(), exact.clone()).await?;
        expect(&first, "same", "alice", Ok(W::Accepted)).await;
        expect(&retry, "same", "alice", Ok(W::Noop)).await;
        assert_eq!(
            exact.calls.lock().map_err(|_| "calls lock failed")?.len(),
            2
        );
        let config = ReceiverConfig::from_json(&config_json("[]", 1, 0), &limits())?;
        let mut conflict = exact
            .calls
            .lock()
            .map_err(|_| "calls lock failed")?
            .first()
            .ok_or("Task invocation fixture is unavailable")?
            .clone();
        conflict.input_archive = JiraEvent::parse(&body("bob"), limits().max_identifier_bytes)
            .map_err(|_| "Jira event fixture is invalid")?
            .archive()
            .map_err(|_| "Jira archive fixture is invalid")?;
        assert!(matches!(
            exact.start(&config.triggers[0], conflict).await,
            Err(TaskLifecycleError::Final)
        ));
        assert_ne!(
            idempotency_key("one", br#"{"value":"same"}"#).map_err(|_| "key failed")?,
            idempotency_key("two", br#"{"value":"same"}"#).map_err(|_| "key failed")?
        );

        let trigger = ReceiverConfig::from_json(&config_json("[]", 1, 0), &limits())?
            .triggers
            .into_iter()
            .next()
            .ok_or("Trigger fixture is unavailable")?;
        for (event, issue_type) in [("Jira event", "10001"), ("jira:issue_updated", "Task")] {
            let input_archive = JiraEvent {
                event: event.to_owned(),
                issue_key: "PROJ-123".to_owned(),
                project_key: "PROJ".to_owned(),
                issue_type: issue_type.to_owned(),
                author_account_id: "alice".to_owned(),
            }
            .archive()
            .map_err(|_| "Jira archive fixture is invalid")?;
            assert!(
                !TaskInvocation {
                    workflow: trigger.workflow.clone(),
                    envelope_digest: trigger.envelope_digest.clone(),
                    idempotency_key: format!("jira-sha256:{}", "a".repeat(64)),
                    input_archive,
                }
                .validate_for(&trigger)
            );
        }

        let config = ReceiverConfig::from_json(&config_json("[]", 2, 0), &limits())?;
        let mut second = config.triggers[0].clone();
        second.id = "jira-two".to_owned();
        let r = Receiver::new(
            ReceiverConfig {
                triggers: vec![config.triggers[0].clone(), second],
            },
            BTreeMap::from([
                ("jira-read".to_owned(), b"test-signing-secret".to_vec()),
                ("jira-two".to_owned(), b"test-signing-secret".to_vec()),
            ]),
            catalogs(ToolAccessClass::Read),
            envelopes(),
            Tasks::default(),
            limits(),
        )
        .await?;
        let event = body("alice");
        let signed = signature(b"test-signing-secret", &event);
        assert_eq!(
            r.receive("jira-read", Some(&signed), &event).await,
            Ok(W::Accepted)
        );
        assert_eq!(
            r.receive("jira-two", Some(&signed), &event).await,
            Ok(W::Accepted)
        );
        Ok(())
    }

    #[test]
    fn pending_delivery_drop_without_runtime_does_not_panic() -> Result<(), String> {
        let config = ReceiverConfig::from_json(&config_json("[]", 1, 0), &limits())?
            .triggers
            .into_iter()
            .next()
            .ok_or("Trigger fixture is unavailable")?;
        let live = Arc::new(LiveTrigger {
            signing_key: hmac::Key::new(hmac::HMAC_SHA256, b"test-signing-secret"),
            concurrency: Arc::new(Semaphore::new(1)),
            config,
            state: Mutex::new(TriggerState::default()),
        });
        drop(PendingDelivery::new(
            live,
            format!("jira-sha256:{}", "a".repeat(64)),
            "PROJ-123".to_owned(),
        ));
        Ok(())
    }

    #[tokio::test]
    async fn panicking_task_start_releases_pending_delivery_and_cooldown() -> Result<(), String> {
        let tasks = Tasks {
            panic_start: Arc::new(AtomicBool::new(true)),
            ..Tasks::default()
        };
        let receiver = make_receiver(config_json("[]", 1, 60), envelopes(), tasks.clone()).await?;
        assert_eq!(send(&receiver, "first", "alice").await, Err(E::Failed));
        let live = receiver
            .triggers
            .get("jira-read")
            .ok_or("Trigger fixture is unavailable")?;
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let state = live.state.lock().await;
                if state.deliveries.is_empty() && state.issue_cooldowns.is_empty() {
                    break;
                }
                drop(state);
                tokio::task::yield_now().await;
            }
        })
        .await
        .map_err(|_| "panicking Task start did not release receiver state")?;
        tasks.panic_start.store(false, Ordering::SeqCst);
        assert_eq!(send(&receiver, "retry", "alice").await, Ok(W::Accepted));
        Ok(())
    }

    #[tokio::test]
    async fn replay_identity_uses_the_parsed_json_value() -> Result<(), String> {
        let tasks = Tasks::default();
        let receiver = make_receiver(config_json("[]", 2, 0), envelopes(), tasks.clone()).await?;
        let first = body("alice");
        let second = br#"{
          "ignored": "event text",
          "user": { "accountId": "alice" },
          "issue": {
            "fields": {
              "issuetype": { "name": "free text", "id": "10001" },
              "project": { "key": "PROJ" }
            },
            "key": "PROJ-123"
          },
          "webhookEvent": "jira:issue_updated"
        }"#;
        assert_eq!(
            receiver
                .receive(
                    "jira-read",
                    Some(&signature(b"test-signing-secret", &first)),
                    &first,
                )
                .await,
            Ok(W::Accepted)
        );
        assert_eq!(
            receiver
                .receive(
                    "jira-read",
                    Some(&signature(b"test-signing-secret", second)),
                    second,
                )
                .await,
            Ok(W::Noop)
        );
        let different = std::str::from_utf8(second)
            .map_err(|_| "event fixture is not UTF-8")?
            .replace("event text", "different event text");
        assert_eq!(
            receiver
                .receive(
                    "jira-read",
                    Some(&signature(b"test-signing-secret", different.as_bytes())),
                    different.as_bytes(),
                )
                .await,
            Ok(W::Accepted)
        );
        assert_eq!(
            tasks.calls.lock().map_err(|_| "calls lock failed")?.len(),
            2
        );
        // Pin the numeric range represented exactly by serde_json's default Number model.
        assert_ne!(
            idempotency_key("jira-read", br#"{"value":18446744073709551615}"#),
            idempotency_key("jira-read", br#"{"value":18446744073709551614}"#)
        );
        assert_ne!(
            idempotency_key("jira-read", br#"{"value":0.1234567890123456}"#),
            idempotency_key("jira-read", br#"{"value":0.1234567890123457}"#)
        );
        Ok(())
    }

    #[tokio::test]
    async fn pending_failure_is_retriable_and_cooldowns_are_bounded() -> Result<(), String> {
        let entered = Arc::new(Notify::new());
        let gate = Arc::new(Notify::new());
        let tasks = Tasks {
            entered: Some(entered.clone()),
            gate: Some(gate.clone()),
            fail_start: Arc::new(AtomicBool::new(true)),
            ..Tasks::default()
        };
        let r = make_receiver(config_json("[]", 2, 0), envelopes(), tasks.clone()).await?;
        let request = r.clone();
        let first = tokio::spawn(async move { send(&request, "same", "alice").await });
        entered.notified().await;
        expect(&r, "same", "alice", Err(E::Busy)).await;
        tasks.fail_start.store(false, Ordering::SeqCst);
        gate.notify_one();
        assert_eq!(
            first.await.map_err(|_| "request task failed")?,
            Err(E::Failed)
        );
        gate.notify_one();
        expect(&r, "same", "alice", Ok(W::Accepted)).await;

        let recorded = Arc::new(Notify::new());
        let response_gate = Arc::new(Notify::new());
        let r = make_receiver(
            config_json("[]", 1, 0),
            envelopes(),
            Tasks {
                blocks: true,
                recorded: Some(recorded.clone()),
                response_gate: Some(response_gate.clone()),
                ..Tasks::default()
            },
        )
        .await?;
        let cancelled_receiver = r.clone();
        let cancelled =
            tokio::spawn(async move { send(&cancelled_receiver, "cancel", "alice").await });
        recorded.notified().await;
        cancelled.abort();
        assert!(cancelled.await.is_err());
        expect(&r, "fresh", "alice", Err(E::Busy)).await;
        response_gate.notify_one();
        for _ in 0..10 {
            if r.triggers
                .get("jira-read")
                .ok_or("Trigger fixture is unavailable")?
                .state
                .lock()
                .await
                .deliveries
                .values()
                .all(|status| *status == DeliveryState::Accepted)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            r.triggers
                .get("jira-read")
                .ok_or("Trigger fixture is unavailable")?
                .state
                .lock()
                .await
                .deliveries
                .values()
                .all(|status| *status == DeliveryState::Accepted)
        );
        expect(&r, "retry", "alice", Ok(W::Noop)).await;
        expect(&r, "other", "bob", Err(E::Busy)).await;

        let r = make_receiver(config_json("[]", 4, 60), envelopes(), Tasks::default()).await?;
        assert_eq!(
            send_issue(&r, "one", "alice", "PROJ-1").await,
            Ok(W::Accepted)
        );
        assert_eq!(
            send_issue(&r, "two", "alice", "PROJ-2").await,
            Ok(W::Accepted)
        );
        assert_eq!(
            send_issue(&r, "three", "alice", "PROJ-3").await,
            Err(E::Busy)
        );
        Ok(())
    }

    #[tokio::test]
    async fn failed_delivery_preserves_a_newer_cooldown_reservation() -> Result<(), String> {
        let entered = Arc::new(Notify::new());
        let gate = Arc::new(Notify::new());
        let tasks = Tasks {
            entered: Some(entered.clone()),
            gate: Some(gate.clone()),
            fail_start: Arc::new(AtomicBool::new(true)),
            ..Tasks::default()
        };
        let r = make_receiver(config_json("[]", 2, 1), envelopes(), tasks.clone()).await?;
        let first_receiver = r.clone();
        let first =
            tokio::spawn(
                async move { send_issue(&first_receiver, "first", "alice", "PROJ-1").await },
            );
        entered.notified().await;
        let live = r
            .triggers
            .get("jira-read")
            .ok_or("Trigger fixture is unavailable")?;
        live.state
            .lock()
            .await
            .issue_cooldowns
            .get_mut("PROJ-1")
            .ok_or("cooldown reservation is unavailable")?
            .started_at = Instant::now()
            .checked_sub(Duration::from_secs(2))
            .ok_or("test clock cannot be adjusted")?;

        tasks.fail_start.store(false, Ordering::SeqCst);
        let second_receiver = r.clone();
        let second =
            tokio::spawn(
                async move { send_issue(&second_receiver, "second", "bob", "PROJ-1").await },
            );
        entered.notified().await;
        gate.notify_waiters();
        assert_eq!(
            first.await.map_err(|_| "first request task failed")?,
            Err(E::Failed)
        );
        assert_eq!(
            second.await.map_err(|_| "second request task failed")?,
            Ok(W::Accepted)
        );
        let expected = idempotency_key("jira-read", &body_for("bob", "PROJ-1"))
            .map_err(|_| "idempotency key fixture is invalid")?;
        assert_eq!(
            live.state
                .lock()
                .await
                .issue_cooldowns
                .get("PROJ-1")
                .map(|reservation| reservation.delivery_id.as_str()),
            Some(expected.as_str())
        );
        Ok(())
    }

    #[tokio::test]
    async fn delivery_cache_evicts_only_accepted_entries() -> Result<(), String> {
        let mut small = limits();
        small.max_recent_deliveries = 2;
        small.max_issue_cooldowns = 3;
        let tasks = Tasks::default();
        let r =
            make_receiver_with_limits(config_json("[]", 4, 0), envelopes(), tasks.clone(), small)
                .await?;
        for (delivery, issue) in [("z", "PROJ-1"), ("a", "PROJ-2"), ("m", "PROJ-3")] {
            assert_eq!(
                send_issue(&r, delivery, "alice", issue).await,
                Ok(W::Accepted)
            );
        }
        assert_eq!(send_issue(&r, "z", "alice", "PROJ-1").await, Ok(W::Noop));
        assert_eq!(
            tasks.calls.lock().map_err(|_| "calls lock failed")?.len(),
            4
        );
        assert_eq!(send_issue(&r, "m", "alice", "PROJ-3").await, Ok(W::Noop));
        assert_eq!(
            tasks.calls.lock().map_err(|_| "calls lock failed")?.len(),
            4
        );

        let entered = Arc::new(Notify::new());
        let gate = Arc::new(Notify::new());
        let tasks = Tasks {
            entered: Some(entered.clone()),
            gate: Some(gate.clone()),
            ..Tasks::default()
        };
        let r =
            make_receiver_with_limits(config_json("[]", 3, 0), envelopes(), tasks, small).await?;
        let first_receiver = r.clone();
        let first =
            tokio::spawn(
                async move { send_issue(&first_receiver, "one", "alice", "PROJ-1").await },
            );
        entered.notified().await;
        let second_receiver = r.clone();
        let second =
            tokio::spawn(
                async move { send_issue(&second_receiver, "two", "alice", "PROJ-2").await },
            );
        entered.notified().await;
        assert_eq!(
            send_issue(&r, "three", "alice", "PROJ-3").await,
            Err(E::Busy)
        );
        gate.notify_waiters();
        assert_eq!(
            first.await.map_err(|_| "first request task failed")?,
            Ok(W::Accepted)
        );
        assert_eq!(
            second.await.map_err(|_| "second request task failed")?,
            Ok(W::Accepted)
        );
        Ok(())
    }

    #[tokio::test]
    async fn envelope_failures_preserve_retry_behavior() -> Result<(), String> {
        for error in [
            EnvelopeError::Absent,
            EnvelopeError::Ambiguous,
            EnvelopeError::Stale,
            EnvelopeError::Unavailable,
        ] {
            let envelopes = Envelopes(Arc::new(StdMutex::new(Err(error))));
            assert!(
                make_receiver(config_json("[]", 1, 0), envelopes, Tasks::default())
                    .await
                    .is_err()
            );
        }
        let mutable_envelopes = envelopes();
        let r = make_receiver(
            config_json("[]", 1, 0),
            mutable_envelopes.clone(),
            Tasks::default(),
        )
        .await?;
        for (delivery, error) in [
            ("absent", EnvelopeError::Absent),
            ("ambiguous", EnvelopeError::Ambiguous),
            ("stale", EnvelopeError::Stale),
        ] {
            *mutable_envelopes
                .0
                .lock()
                .map_err(|_| "Envelope fixture lock")? = Err(error);
            assert!(matches!(
                send(&r, delivery, "alice").await,
                Err(E::Invalid(_))
            ));
        }
        *mutable_envelopes
            .0
            .lock()
            .map_err(|_| "Envelope fixture lock")? = Err(EnvelopeError::Unavailable);
        expect(&r, "retry", "alice", Err(E::Failed)).await;
        *mutable_envelopes
            .0
            .lock()
            .map_err(|_| "Envelope fixture lock")? = Ok(vec![grant()]);
        expect(&r, "retry", "alice", Ok(W::Accepted)).await;

        let catalogs = catalogs(ToolAccessClass::Read);
        let r = make_receiver_with_catalogs(
            config_json("[]", 1, 0),
            catalogs.clone(),
            envelopes(),
            Tasks::default(),
            limits(),
        )
        .await?;
        *catalogs.0.lock().map_err(|_| "catalog lock failed")? =
            Ok(catalog(ToolAccessClass::Write));
        expect(
            &r,
            "catalog",
            "alice",
            Err(E::Invalid("This Trigger is not read-only.")),
        )
        .await;
        *catalogs.0.lock().map_err(|_| "catalog lock failed")? =
            Err("catalog unavailable".to_owned());
        expect(&r, "catalog", "alice", Err(E::Failed)).await;
        *catalogs.0.lock().map_err(|_| "catalog lock failed")? = Ok(catalog(ToolAccessClass::Read));
        expect(&r, "catalog", "alice", Ok(W::Accepted)).await;
        Ok(())
    }

    #[tokio::test]
    async fn router_enforces_body_limit_before_routing() -> Result<(), String> {
        let r = make_receiver(config_json("[]", 1, 0), envelopes(), Tasks::default()).await?;
        let body = body("alice");
        let app = router(r);
        let response = app
            .clone()
            .oneshot(
                Request::post("/hooks/jira/jira-read")
                    .header("x-hub-signature", signature(b"test-signing-secret", &body))
                    .header("x-atlassian-webhook-identifier", "one")
                    .body(Body::from(body))
                    .map_err(|_| "request is invalid")?,
            )
            .await
            .map_err(|_| "route failed")?;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let response = app
            .oneshot(
                Request::post("/hooks/jira/jira-read")
                    .body(Body::from(vec![0_u8; limits().max_body_bytes + 1]))
                    .map_err(|_| "request is invalid")?,
            )
            .await
            .map_err(|_| "route failed")?;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        Ok(())
    }
}
