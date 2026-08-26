use std::collections::BTreeMap;
use std::fs;
use std::future::Future;
use std::path::Path as FilePath;
use std::pin::Pin;

use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use jsonwebtoken::jwk::{Jwk, JwkSet, KeyAlgorithm, PublicKeyUse};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
#[cfg(test)]
use k8s_openapi::api::authentication::v1::TokenReviewStatus;
use k8s_openapi::api::authentication::v1::{TokenReview, UserInfo};
use kube::api::{Api, PostParams};
use kube::{Client, ResourceExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use steward_admission::{AdmissionDecision, AdmissionDelta, Envelope, evaluate_with_grants};
use steward_ports::MAX_TASK_INPUT_ARCHIVE_BYTES;
use steward_store::{
    EnvelopeRequestRecord, ParkRejection, PgStore, StoreError, TaskRecord, TaskReservationRequest,
    WorkflowRevisionRecord,
};
use steward_types::{
    AgentRuntime, AgentRuntimeSpec, Budget, CanonicalAuthorityBinding, CanonicalUserId, Duration,
    Email, ModelRef, PENDING_APPROVAL_ANNOTATION, Principal, RuntimeOwnership, TaskPhase,
    ToolGrant,
};
use uuid::Uuid;

use crate::WorkflowReference;
use crate::{
    AdmissionLedger, ApiError, BoxFuture, DecisionChannel, KubernetesTokenReviewAudience,
    RuntimeCreateError, RuntimeRepository, authenticated_token_review_user,
    file_decision_reference, spec_digest, token_review_request,
};

const SERVICE_PRINCIPAL_ANNOTATION: &str = "agents.apelogic.ai/service-principal";
const SERVICE_GROUP_PREFIX: &str = "agents.apelogic.ai/service-principal:";
const ACTING_USER_GROUP_PREFIX: &str = "agents.apelogic.ai/acting-user:";
const TASK_OWNER_GROUP_PREFIX: &str = "agents.apelogic.ai/task-owner:";
const CANONICAL_USER_GROUP_PREFIX: &str = "agents.apelogic.ai/canonical-user:";
const VERSIONED_WORKFLOW_NAMESPACE: &str = "steward-workflows";
const IDENTITY_TASK_CONTRACT: &str = "steward-task-v2";
const MAX_IDENTITY_TASK_TOKEN_BYTES: usize = 16 * 1024;
const MAX_IDENTITY_JWKS_BYTES: usize = 128 * 1024;
const MAX_IDENTITY_TASK_TOKEN_AGE_SECONDS: u64 = 300;
const IDENTITY_CLOCK_SKEW_SECONDS: u64 = 60;

fn versioned_workflow_reference(
    workflow: &str,
    coding_agent_runtime: Option<&str>,
) -> Result<Option<WorkflowReference>, ApiError> {
    if !workflow.contains('@') {
        return Ok(None);
    }
    let reference = WorkflowReference::parse(workflow).map_err(|_| {
        ApiError::Admission("workflow must be an exact lowercase name@positive-version".to_owned())
    })?;
    if coding_agent_runtime.is_some() {
        return Err(ApiError::Admission(
            "codingAgentRuntime is server-selected for versioned Workflows".to_owned(),
        ));
    }
    Ok(Some(reference))
}

struct VersionedTaskPlan {
    workflow: WorkflowRevisionRecord,
    envelope: EnvelopeRequestRecord,
    spec: AgentRuntimeSpec,
    command: Vec<String>,
}

fn resolve_versioned_task_plan(
    identity: &TaskIdentity,
    workflow: WorkflowRevisionRecord,
    envelopes: Vec<EnvelopeRequestRecord>,
) -> Result<VersionedTaskPlan, ApiError> {
    let [envelope] = envelopes.as_slice() else {
        return if envelopes.is_empty() {
            Err(ApiError::MissingEnvelope)
        } else {
            Err(ApiError::Conflict(
                "multiple active provisioned User Envelopes are ambiguous".to_owned(),
            ))
        };
    };
    if envelope.owner_user_id != identity.canonical_user_id {
        return Err(ApiError::PrincipalMismatch);
    }
    if envelope.status != steward_store::EnvelopeRequestStatus::Provisioned
        || envelope.envelope_instance_id.is_none()
        || envelope.envelope_digest.is_none()
    {
        return Err(ApiError::MissingEnvelope);
    }
    let approved = envelope
        .approved_envelope
        .as_ref()
        .ok_or(ApiError::MissingEnvelope)?;
    let [model] = approved.spec.llms.as_slice() else {
        return Err(ApiError::Admission(
            "versioned Codex Workflows require exactly one approved model".to_owned(),
        ));
    };
    let codex_model = format!("{}/{}", model.provider, model.model);
    let canonical_authority = CanonicalAuthorityBinding::new(
        identity.canonical_user_id.clone(),
        identity
            .acting_user
            .as_ref()
            .map(|_| identity.canonical_user_id.clone()),
    )
    .map_err(ApiError::Admission)?;
    let spec = AgentRuntimeSpec {
        principal: Principal::Service {
            name: identity.service.clone(),
            acting_user: identity.acting_user.clone(),
        },
        owner: identity.owner.clone(),
        canonical_authority: Some(canonical_authority),
        agent_type: steward_types::AgentType {
            name: workflow.agent.clone(),
        },
        llms: approved.spec.llms.clone(),
        tools: approved.spec.tools.clone(),
        budget: approved.spec.budget.clone(),
        ttl: approved.spec.ttl.clone(),
        runner: approved.spec.runner.clone(),
        bindings: None,
    };
    let command = vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        concat!(
            "set -eu; umask 077; ",
            "test \"$(codex --version)\" = \"codex-cli 0.117.0\"; ",
            "test \"$#\" -eq 2; ",
            "export CODEX_HOME=/sandbox/steward-codex; ",
            "mkdir -p \"$CODEX_HOME\" \"$STEWARD_OUTPUT_DIR/out\"; ",
            "test ! -e out; ln -s \"$STEWARD_OUTPUT_DIR/out\" out; ",
            "printf '%s\\n' ",
            "'model_provider = \"litellm\"' ",
            "'approval_policy = \"never\"' ",
            "'web_search = \"disabled\"' ",
            "'[model_providers.litellm]' ",
            "'name = \"LiteLLM\"' ",
            "'base_url = \"http://litellm-litellm.litellm.svc.cluster.local:4000/v1\"' ",
            "'env_key = \"OPENAI_API_KEY\"' ",
            "'wire_api = \"responses\"' ",
            "'requires_openai_auth = false' ",
            "> \"$CODEX_HOME/config.toml\"; ",
            "OPENAI_API_KEY=openshell-token-grant-placeholder ",
            "codex exec --ephemeral --skip-git-repo-check ",
            "--sandbox danger-full-access --model \"$2\" ",
            "--output-last-message \"$STEWARD_OUTPUT_DIR/result.txt\" -- \"$1\""
        )
        .to_owned(),
        "steward-workflow".to_owned(),
        workflow.prompt.clone(),
        codex_model,
    ];
    Ok(VersionedTaskPlan {
        workflow,
        envelope: envelope.clone(),
        spec,
        command,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskAuthenticationError {
    InvalidCredentials,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskIdentity {
    pub service: String,
    pub acting_user: Option<Email>,
    pub owner: Email,
    pub canonical_user_id: CanonicalUserId,
}

pub trait TaskIdentityResolver: Clone + Send + Sync + 'static {
    fn resolve<'a>(
        &'a self,
        assertion: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<TaskIdentity, TaskAuthenticationError>> + Send + 'a>>;
}

#[derive(Clone)]
pub struct KubernetesTaskIdentityResolver {
    client: Client,
    audience: KubernetesTokenReviewAudience,
    canonical_identities: PgStore,
}

impl KubernetesTaskIdentityResolver {
    pub fn new(
        client: Client,
        audience: KubernetesTokenReviewAudience,
        canonical_identities: PgStore,
    ) -> Self {
        Self {
            client,
            audience,
            canonical_identities,
        }
    }
}

impl TaskIdentityResolver for KubernetesTaskIdentityResolver {
    fn resolve<'a>(
        &'a self,
        assertion: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<TaskIdentity, TaskAuthenticationError>> + Send + 'a>>
    {
        Box::pin(async move {
            let review = token_review_request(assertion, &self.audience);
            let reviewed = Api::<TokenReview>::all(self.client.clone())
                .create(&PostParams::default(), &review)
                .await
                .map_err(|_| TaskAuthenticationError::Unavailable)?;
            let user = authenticated_token_review_user(reviewed.status, self.audience.as_str())
                .ok_or(TaskAuthenticationError::InvalidCredentials)?;
            let identity = task_identity_from_kubernetes_user(&user)?;
            self.canonical_identities
                .resolve_canonical_principal(&identity.canonical_user_id, &identity.owner)
                .await
                .map_err(|error| match error {
                    StoreError::Database(_) => TaskAuthenticationError::Unavailable,
                    _ => TaskAuthenticationError::InvalidCredentials,
                })?;
            Ok(identity)
        })
    }
}

/// Verifies the short-lived, ES256 task credential issued by the deployed Identity service.
///
/// This is deliberately a distinct resolver from Kubernetes TokenReview. A GitHub Actions
/// runner cannot present a Kubernetes service-account token, and an Identity credential must
/// never be sent to the Kubernetes TokenReview API. Deployments select exactly one resolver.
#[derive(Clone)]
pub struct IdentityTaskIdentityResolver {
    jwks: JwkSet,
    issuer: String,
    audience: String,
    canonical_identities: PgStore,
}

#[derive(Clone)]
pub enum ConfiguredTaskIdentityResolver {
    Kubernetes(KubernetesTaskIdentityResolver),
    Identity(IdentityTaskIdentityResolver),
}

impl ConfiguredTaskIdentityResolver {
    pub fn kubernetes(
        client: Client,
        audience: KubernetesTokenReviewAudience,
        canonical_identities: PgStore,
    ) -> Self {
        Self::Kubernetes(KubernetesTaskIdentityResolver::new(
            client,
            audience,
            canonical_identities,
        ))
    }

    pub fn identity_from_jwks_file(
        issuer: String,
        audience: String,
        jwks_file: &FilePath,
        canonical_identities: PgStore,
    ) -> Result<Self, TaskAuthenticationError> {
        Ok(Self::Identity(
            IdentityTaskIdentityResolver::from_jwks_file(
                issuer,
                audience,
                jwks_file,
                canonical_identities,
            )?,
        ))
    }

    /// Resolve the ratified Identity claims into the authenticated username and groups without
    /// performing task admission. This is used only by the route-scoped administrator
    /// authenticator for the Steward-run service-envelope bootstrap path.
    ///
    /// Kubernetes service-account credentials still use TokenReview. A caller cannot select
    /// this path: deployments select the configured task identity verifier, which validates the
    /// exact Identity issuer, audience, signature, expiry and identity contract first.
    pub(crate) fn authenticated_user(
        &self,
        assertion: &str,
    ) -> Result<UserInfo, TaskAuthenticationError> {
        match self {
            Self::Kubernetes(_) => Err(TaskAuthenticationError::InvalidCredentials),
            Self::Identity(resolver) => resolver.authenticated_user(assertion),
        }
    }
}

impl TaskIdentityResolver for ConfiguredTaskIdentityResolver {
    fn resolve<'a>(
        &'a self,
        assertion: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<TaskIdentity, TaskAuthenticationError>> + Send + 'a>>
    {
        match self {
            Self::Kubernetes(resolver) => resolver.resolve(assertion),
            Self::Identity(resolver) => resolver.resolve(assertion),
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(untagged)]
enum IdentityTaskAudience {
    Single(String),
    Multiple(Vec<String>),
}

#[derive(Clone, Deserialize)]
struct IdentityTaskClaims {
    iss: String,
    sub: String,
    aud: IdentityTaskAudience,
    exp: u64,
    iat: u64,
    nbf: u64,
    jti: String,
    email: String,
    email_verified: bool,
    groups: Vec<String>,
    identity_contract: String,
}

impl IdentityTaskIdentityResolver {
    pub fn from_jwks_file(
        issuer: String,
        audience: String,
        jwks_file: &FilePath,
        canonical_identities: PgStore,
    ) -> Result<Self, TaskAuthenticationError> {
        if !valid_identity_issuer(&issuer) || !bounded_non_whitespace(&audience, 256) {
            return Err(TaskAuthenticationError::InvalidCredentials);
        }
        let jwks =
            fs::read_to_string(jwks_file).map_err(|_| TaskAuthenticationError::Unavailable)?;
        if jwks.len() > MAX_IDENTITY_JWKS_BYTES {
            return Err(TaskAuthenticationError::InvalidCredentials);
        }
        let jwks =
            serde_json::from_str(&jwks).map_err(|_| TaskAuthenticationError::InvalidCredentials)?;
        validate_identity_task_jwks(&jwks)?;
        Ok(Self {
            jwks,
            issuer,
            audience,
            canonical_identities,
        })
    }
}

impl IdentityTaskIdentityResolver {
    pub(crate) fn authenticated_user(
        &self,
        assertion: &str,
    ) -> Result<UserInfo, TaskAuthenticationError> {
        let claims =
            verify_identity_task_token(assertion, &self.jwks, &self.issuer, &self.audience)?;
        Ok(UserInfo {
            username: Some(claims.email),
            groups: Some(claims.groups),
            ..UserInfo::default()
        })
    }
}

impl TaskIdentityResolver for IdentityTaskIdentityResolver {
    fn resolve<'a>(
        &'a self,
        assertion: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<TaskIdentity, TaskAuthenticationError>> + Send + 'a>>
    {
        Box::pin(async move {
            let claims =
                verify_identity_task_token(assertion, &self.jwks, &self.issuer, &self.audience)?;
            let identity = task_identity_from_identity_claims(claims)?;
            self.canonical_identities
                .resolve_canonical_principal(&identity.canonical_user_id, &identity.owner)
                .await
                .map_err(|error| match error {
                    StoreError::Database(_) => TaskAuthenticationError::Unavailable,
                    _ => TaskAuthenticationError::InvalidCredentials,
                })?;
            Ok(identity)
        })
    }
}

fn verify_identity_task_token(
    assertion: &str,
    jwks: &JwkSet,
    issuer: &str,
    audience: &str,
) -> Result<IdentityTaskClaims, TaskAuthenticationError> {
    if assertion.is_empty() || assertion.len() > MAX_IDENTITY_TASK_TOKEN_BYTES {
        return Err(TaskAuthenticationError::InvalidCredentials);
    }
    let header =
        decode_header(assertion).map_err(|_| TaskAuthenticationError::InvalidCredentials)?;
    if header.alg != Algorithm::ES256
        || header.jku.is_some()
        || header.jwk.is_some()
        || header.x5u.is_some()
        || header.x5c.is_some()
        || header.x5t.is_some()
        || header.x5t_s256.is_some()
        || header.crit.is_some()
    {
        return Err(TaskAuthenticationError::InvalidCredentials);
    }
    let kid = header
        .kid
        .ok_or(TaskAuthenticationError::InvalidCredentials)?;
    if !bounded_non_whitespace(&kid, 128) || !kid.is_ascii() {
        return Err(TaskAuthenticationError::InvalidCredentials);
    }
    let key = select_identity_task_key(jwks, &kid)?;
    let key =
        DecodingKey::from_jwk(key).map_err(|_| TaskAuthenticationError::InvalidCredentials)?;
    let mut validation = Validation::new(Algorithm::ES256);
    validation.required_spec_claims.clear();
    validation.validate_exp = false;
    validation.validate_aud = false;
    validation.validate_nbf = false;
    let claims = decode::<IdentityTaskClaims>(assertion, &key, &validation)
        .map_err(|_| TaskAuthenticationError::InvalidCredentials)?
        .claims;
    validate_identity_task_claims(&claims, issuer, audience)?;
    Ok(claims)
}

fn validate_identity_task_jwks(jwks: &JwkSet) -> Result<(), TaskAuthenticationError> {
    if jwks.keys.is_empty() || jwks.keys.len() > 16 {
        return Err(TaskAuthenticationError::InvalidCredentials);
    }
    let mut kids = std::collections::HashSet::new();
    let mut identity_task_keys = 0_usize;
    for key in &jwks.keys {
        let kid = key
            .common
            .key_id
            .as_deref()
            .ok_or(TaskAuthenticationError::InvalidCredentials)?;
        if !bounded_non_whitespace(kid, 128) || !kid.is_ascii() || !kids.insert(kid) {
            return Err(TaskAuthenticationError::InvalidCredentials);
        }
        // Identity's public JWKS can also publish keys for its separate workload
        // exchange contract. Only ES256 signing keys participate in the
        // steward-task-v2 trust domain; non-ES256 keys remain unselectable.
        if key.common.key_algorithm == Some(KeyAlgorithm::ES256) {
            if key.common.public_key_use != Some(PublicKeyUse::Signature)
                || DecodingKey::from_jwk(key).is_err()
            {
                return Err(TaskAuthenticationError::InvalidCredentials);
            }
            identity_task_keys += 1;
        }
    }
    (identity_task_keys > 0)
        .then_some(())
        .ok_or(TaskAuthenticationError::InvalidCredentials)
}

fn select_identity_task_key<'a>(
    jwks: &'a JwkSet,
    kid: &str,
) -> Result<&'a Jwk, TaskAuthenticationError> {
    let mut matching = jwks.keys.iter().filter(|key| {
        key.common.key_id.as_deref() == Some(kid)
            && key.common.key_algorithm == Some(KeyAlgorithm::ES256)
            && key.common.public_key_use == Some(PublicKeyUse::Signature)
    });
    let key = matching
        .next()
        .ok_or(TaskAuthenticationError::InvalidCredentials)?;
    if matching.next().is_some() {
        return Err(TaskAuthenticationError::InvalidCredentials);
    }
    Ok(key)
}

fn validate_identity_task_claims(
    claims: &IdentityTaskClaims,
    issuer: &str,
    audience: &str,
) -> Result<(), TaskAuthenticationError> {
    let now = jsonwebtoken::get_current_timestamp();
    let audience_matches = match &claims.aud {
        IdentityTaskAudience::Single(value) => value == audience,
        IdentityTaskAudience::Multiple(values) => {
            values.len() == 1 && values.first() == Some(&audience.to_owned())
        }
    };
    if claims.iss != issuer
        || !audience_matches
        || claims.identity_contract != IDENTITY_TASK_CONTRACT
        || !claims.email_verified
        || !valid_email(&claims.email)
        || !bounded_non_whitespace(&claims.sub, 255)
        || !bounded_non_whitespace(&claims.jti, 128)
        || claims.exp.saturating_add(IDENTITY_CLOCK_SKEW_SECONDS) <= now
        || claims.iat > now.saturating_add(IDENTITY_CLOCK_SKEW_SECONDS)
        || now
            > claims
                .iat
                .saturating_add(MAX_IDENTITY_TASK_TOKEN_AGE_SECONDS)
        || claims.nbf > now.saturating_add(IDENTITY_CLOCK_SKEW_SECONDS)
        || claims.groups.len() > 16
    {
        return Err(TaskAuthenticationError::InvalidCredentials);
    }
    Ok(())
}

fn task_identity_from_identity_claims(
    claims: IdentityTaskClaims,
) -> Result<TaskIdentity, TaskAuthenticationError> {
    let user = UserInfo {
        username: Some(claims.email),
        groups: Some(claims.groups),
        ..UserInfo::default()
    };
    task_identity_from_kubernetes_user(&user)
}

fn valid_identity_issuer(value: &str) -> bool {
    value.starts_with("https://") && value.len() <= 2_048 && !value.chars().any(char::is_whitespace)
}

fn bounded_non_whitespace(value: &str, maximum: usize) -> bool {
    !value.is_empty() && value.len() <= maximum && !value.chars().any(char::is_whitespace)
}

#[cfg(test)]
pub(crate) fn task_identity_from_token_review(
    status: Option<TokenReviewStatus>,
    requested_audience: &str,
) -> Result<TaskIdentity, TaskAuthenticationError> {
    let user = authenticated_token_review_user(status, requested_audience)
        .ok_or(TaskAuthenticationError::InvalidCredentials)?;
    task_identity_from_kubernetes_user(&user)
}

fn task_identity_from_kubernetes_user(
    user: &UserInfo,
) -> Result<TaskIdentity, TaskAuthenticationError> {
    let username = user
        .username
        .as_deref()
        .filter(|username| !username.is_empty())
        .ok_or(TaskAuthenticationError::InvalidCredentials)?;
    let groups = user.groups.as_deref().unwrap_or_default();
    let services = group_values(groups, SERVICE_GROUP_PREFIX);
    let acting_users = group_values(groups, ACTING_USER_GROUP_PREFIX);
    let owners = group_values(groups, TASK_OWNER_GROUP_PREFIX);
    let canonical_users = group_values(groups, CANONICAL_USER_GROUP_PREFIX);
    let [service] = services.as_slice() else {
        return Err(TaskAuthenticationError::InvalidCredentials);
    };
    let [canonical_user] = canonical_users.as_slice() else {
        return Err(TaskAuthenticationError::InvalidCredentials);
    };
    let canonical_user_id = CanonicalUserId::parse(canonical_user.clone())
        .map_err(|_| TaskAuthenticationError::InvalidCredentials)?;
    if service.is_empty() {
        return Err(TaskAuthenticationError::InvalidCredentials);
    }
    let acting_user = match acting_users.as_slice() {
        [] => None,
        [acting_user]
            if valid_email(username) && valid_email(acting_user) && username == acting_user =>
        {
            Some(Email(acting_user.clone()))
        }
        _ => return Err(TaskAuthenticationError::InvalidCredentials),
    };
    let owner = if let Some(acting_user) = &acting_user {
        if !owners.is_empty() {
            return Err(TaskAuthenticationError::InvalidCredentials);
        }
        acting_user.clone()
    } else {
        let [owner] = owners.as_slice() else {
            return Err(TaskAuthenticationError::InvalidCredentials);
        };
        if !valid_email(owner) {
            return Err(TaskAuthenticationError::InvalidCredentials);
        }
        Email(owner.clone())
    };
    Ok(TaskIdentity {
        service: service.clone(),
        acting_user,
        owner,
        canonical_user_id,
    })
}

fn group_values(groups: &[String], prefix: &str) -> Vec<String> {
    groups
        .iter()
        .filter_map(|group| group.strip_prefix(prefix))
        .map(str::to_owned)
        .collect()
}

fn valid_email(value: &str) -> bool {
    let mut parts = value.split('@');
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(local), Some(domain), None)
            if !local.is_empty() && domain.contains('.') && !value.contains(char::is_whitespace)
    )
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskWorkflow {
    pub name: String,
    pub namespace: String,
    pub coding_agent_runtime: String,
    pub llms: Vec<ModelRef>,
    pub tools: Vec<ToolGrant>,
    pub budget: Budget,
    pub ttl: Duration,
    pub command: Vec<String>,
}

pub trait TaskWorkflowCatalog: Clone + Send + Sync + 'static {
    fn workflow(&self, name: &str) -> Option<TaskWorkflow>;
}

#[derive(Clone, Default)]
pub struct StaticTaskWorkflowCatalog {
    workflows: BTreeMap<String, TaskWorkflow>,
}

impl StaticTaskWorkflowCatalog {
    pub fn new(workflows: impl IntoIterator<Item = TaskWorkflow>) -> Self {
        Self {
            workflows: workflows
                .into_iter()
                .map(|workflow| (workflow.name.clone(), workflow))
                .collect(),
        }
    }

    pub fn from_json(value: &str) -> Result<Self, String> {
        let workflows = serde_json::from_str::<Vec<TaskWorkflow>>(value)
            .map_err(|error| format!("task workflow catalog is invalid: {error}"))?;
        if workflows.iter().any(|workflow| {
            workflow.name.is_empty()
                || workflow.namespace.is_empty()
                || workflow.coding_agent_runtime.is_empty()
                || workflow.command.is_empty()
                || workflow.command.iter().any(String::is_empty)
        }) {
            return Err(
                "task workflows require non-empty identity, namespace, runtime, and command fields"
                    .to_owned(),
            );
        }
        // Versioned Workflows are stored separately and may be the only task
        // submission path.  An empty legacy catalog is therefore valid: no
        // legacy workflow can resolve, while a versioned `name@version`
        // reference remains governed by the persisted Workflow repository.
        Ok(Self::new(workflows))
    }
}

impl TaskWorkflowCatalog for StaticTaskWorkflowCatalog {
    fn workflow(&self, name: &str) -> Option<TaskWorkflow> {
        self.workflows.get(name).cloned()
    }
}

pub trait TaskSubmissionLedger: Clone + Send + Sync + 'static {
    fn workflow_revision<'a>(
        &'a self,
        _name: &'a str,
        _version: i64,
    ) -> BoxFuture<'a, Result<Option<WorkflowRevisionRecord>, StoreError>> {
        Box::pin(async { Ok(None) })
    }

    fn active_provisioned_user_envelopes<'a>(
        &'a self,
        _owner_user_id: &'a CanonicalUserId,
    ) -> BoxFuture<'a, Result<Vec<EnvelopeRequestRecord>, StoreError>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn reserve_task<'a>(
        &'a self,
        request: TaskReservationRequest<'a>,
    ) -> BoxFuture<'a, Result<steward_store::TaskReservation, StoreError>>;

    fn bind_task_runtime<'a>(
        &'a self,
        task_uid: Uuid,
        runtime_uid: &'a str,
        phase: TaskPhase,
    ) -> BoxFuture<'a, Result<TaskRecord, StoreError>>;

    fn put_task_inputs<'a>(
        &'a self,
        task_uid: Uuid,
        submitter_service: &'a str,
        owner_user_id: &'a str,
        archive: &'a [u8],
    ) -> BoxFuture<'a, Result<TaskRecord, StoreError>>;

    fn request_task_execution<'a>(
        &'a self,
        task_uid: Uuid,
        submitter_service: &'a str,
        owner_user_id: &'a str,
    ) -> BoxFuture<'a, Result<TaskRecord, StoreError>>;

    fn task_for_submitter<'a>(
        &'a self,
        task_uid: Uuid,
        submitter_service: &'a str,
        owner_user_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<TaskRecord>, StoreError>>;

    fn request_task_finalization<'a>(
        &'a self,
        task_uid: Uuid,
        submitter_service: &'a str,
        owner_user_id: &'a str,
    ) -> BoxFuture<'a, Result<TaskRecord, StoreError>>;
}

impl TaskSubmissionLedger for PgStore {
    fn workflow_revision<'a>(
        &'a self,
        name: &'a str,
        version: i64,
    ) -> BoxFuture<'a, Result<Option<WorkflowRevisionRecord>, StoreError>> {
        Box::pin(async move { PgStore::workflow_revision(self, name, version).await })
    }

    fn active_provisioned_user_envelopes<'a>(
        &'a self,
        owner_user_id: &'a CanonicalUserId,
    ) -> BoxFuture<'a, Result<Vec<EnvelopeRequestRecord>, StoreError>> {
        Box::pin(async move {
            PgStore::envelope_requests(self, owner_user_id)
                .await
                .map(|records| {
                    records
                        .into_iter()
                        .filter(|record| {
                            record.status == steward_store::EnvelopeRequestStatus::Provisioned
                        })
                        .collect()
                })
        })
    }

    fn reserve_task<'a>(
        &'a self,
        request: TaskReservationRequest<'a>,
    ) -> BoxFuture<'a, Result<steward_store::TaskReservation, StoreError>> {
        Box::pin(async move { PgStore::reserve_task(self, &request).await })
    }

    fn bind_task_runtime<'a>(
        &'a self,
        task_uid: Uuid,
        runtime_uid: &'a str,
        phase: TaskPhase,
    ) -> BoxFuture<'a, Result<TaskRecord, StoreError>> {
        Box::pin(
            async move { PgStore::bind_task_runtime(self, task_uid, runtime_uid, phase).await },
        )
    }

    fn put_task_inputs<'a>(
        &'a self,
        task_uid: Uuid,
        submitter_service: &'a str,
        owner_user_id: &'a str,
        archive: &'a [u8],
    ) -> BoxFuture<'a, Result<TaskRecord, StoreError>> {
        Box::pin(async move {
            PgStore::put_task_inputs(self, task_uid, submitter_service, owner_user_id, archive)
                .await
        })
    }

    fn request_task_execution<'a>(
        &'a self,
        task_uid: Uuid,
        submitter_service: &'a str,
        owner_user_id: &'a str,
    ) -> BoxFuture<'a, Result<TaskRecord, StoreError>> {
        Box::pin(async move {
            PgStore::request_task_execution(self, task_uid, submitter_service, owner_user_id).await
        })
    }

    fn task_for_submitter<'a>(
        &'a self,
        task_uid: Uuid,
        submitter_service: &'a str,
        owner_user_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<TaskRecord>, StoreError>> {
        Box::pin(async move {
            PgStore::task_for_submitter(self, task_uid, submitter_service, owner_user_id).await
        })
    }

    fn request_task_finalization<'a>(
        &'a self,
        task_uid: Uuid,
        submitter_service: &'a str,
        owner_user_id: &'a str,
    ) -> BoxFuture<'a, Result<TaskRecord, StoreError>> {
        Box::pin(async move {
            PgStore::request_task_finalization(self, task_uid, submitter_service, owner_user_id)
                .await
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskSubmissionRequest {
    pub workflow: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coding_agent_runtime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_runtime_uid: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TaskStatusResponse {
    #[schema(value_type = String, format = "uuid")]
    pub task_uid: Uuid,
    #[schema(value_type = Option<String>)]
    pub runtime_uid: Option<String>,
    pub phase: TaskPhase,
    pub runtime_ownership: RuntimeOwnership,
    pub finalized: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schema(value_type = Vec<TaskAdmissionDelta>)]
    pub deltas: Vec<AdmissionDelta>,
}

/// Machine-readable shape of an admission delta returned in Task status.
#[derive(Clone, Debug, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", tag = "dimension")]
pub enum TaskAdmissionDelta {
    Budget {
        requested: String,
        ceiling: String,
        currency: String,
    },
    Ttl {
        requested: String,
        ceiling: String,
    },
    Models {
        requested: Vec<ModelRef>,
        ceiling: Vec<ModelRef>,
    },
    Tools {
        requested: Vec<ToolGrant>,
        ceiling: Vec<ToolGrant>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TaskErrorResponse {
    pub error: String,
}

#[derive(utoipa::ToSchema)]
#[schema(
    value_type = String,
    format = Binary,
    description = "Opaque tar archive, limited to 67,108,864 raw bytes"
)]
pub struct TaskArchive(pub Vec<u8>);

#[derive(Clone)]
struct TaskApiState<R, L, D, I, W> {
    runtimes: R,
    ledger: L,
    decisions: D,
    identities: I,
    workflows: W,
}

pub fn task_router<R, L, D, I, W>(
    runtimes: R,
    ledger: L,
    decisions: D,
    identities: I,
    workflows: W,
) -> Router
where
    R: RuntimeRepository,
    L: AdmissionLedger + TaskSubmissionLedger,
    D: DecisionChannel + Clone,
    I: TaskIdentityResolver,
    W: TaskWorkflowCatalog,
{
    Router::new()
        .route("/v1/tasks", post(submit_task::<R, L, D, I, W>))
        .route(
            "/v1/tasks/{task_uid}/inputs",
            put(put_task_inputs::<R, L, D, I, W>),
        )
        .route(
            "/v1/tasks/{task_uid}/execute",
            post(execute_task::<R, L, D, I, W>),
        )
        .route(
            "/v1/tasks/{task_uid}/outputs",
            get(get_task_outputs::<R, L, D, I, W>),
        )
        .route(
            "/v1/tasks/{task_uid}",
            get(get_task::<R, L, D, I, W>).delete(delete_task::<R, L, D, I, W>),
        )
        .layer(DefaultBodyLimit::max(MAX_TASK_INPUT_ARCHIVE_BYTES))
        .with_state(TaskApiState {
            runtimes,
            ledger,
            decisions,
            identities,
            workflows,
        })
}

async fn get_task_outputs<R, L, D, I, W>(
    State(state): State<TaskApiState<R, L, D, I, W>>,
    Path(task_uid): Path<Uuid>,
    headers: HeaderMap,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger + TaskSubmissionLedger,
    D: DecisionChannel + Clone,
    I: TaskIdentityResolver,
    W: TaskWorkflowCatalog,
{
    let identity = match resolve_task_identity(&state.identities, &headers).await {
        Ok(identity) => identity,
        Err(error) => return error.into_response(),
    };
    let record = match state
        .ledger
        .task_for_submitter(
            task_uid,
            &identity.service,
            identity.canonical_user_id.as_str(),
        )
        .await
    {
        Ok(Some(record)) => record,
        Ok(None) => return ApiError::Store(StoreError::TaskNotFound).into_response(),
        Err(error) => return ApiError::Store(error).into_response(),
    };
    if record.phase != TaskPhase::Succeeded {
        return ApiError::TaskOutputNotReady.into_response();
    }
    match record.output_archive {
        Some(archive) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/x-tar")],
            archive,
        )
            .into_response(),
        None => ApiError::TaskOutputNotReady.into_response(),
    }
}

async fn delete_task<R, L, D, I, W>(
    State(state): State<TaskApiState<R, L, D, I, W>>,
    Path(task_uid): Path<Uuid>,
    headers: HeaderMap,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger + TaskSubmissionLedger,
    D: DecisionChannel + Clone,
    I: TaskIdentityResolver,
    W: TaskWorkflowCatalog,
{
    let identity = match resolve_task_identity(&state.identities, &headers).await {
        Ok(identity) => identity,
        Err(error) => return error.into_response(),
    };
    match state
        .ledger
        .request_task_finalization(
            task_uid,
            &identity.service,
            identity.canonical_user_id.as_str(),
        )
        .await
    {
        Ok(record) => match status_response(record, Vec::new()) {
            Ok(response) => (StatusCode::ACCEPTED, Json(response)).into_response(),
            Err(error) => error.into_response(),
        },
        Err(error) => ApiError::Store(error).into_response(),
    }
}

async fn execute_task<R, L, D, I, W>(
    State(state): State<TaskApiState<R, L, D, I, W>>,
    Path(task_uid): Path<Uuid>,
    headers: HeaderMap,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger + TaskSubmissionLedger,
    D: DecisionChannel + Clone,
    I: TaskIdentityResolver,
    W: TaskWorkflowCatalog,
{
    let identity = match resolve_task_identity(&state.identities, &headers).await {
        Ok(identity) => identity,
        Err(error) => return error.into_response(),
    };
    match state
        .ledger
        .request_task_execution(
            task_uid,
            &identity.service,
            identity.canonical_user_id.as_str(),
        )
        .await
    {
        Ok(record) => match status_response(record, Vec::new()) {
            Ok(response) => (StatusCode::ACCEPTED, Json(response)).into_response(),
            Err(error) => error.into_response(),
        },
        Err(error) => ApiError::Store(error).into_response(),
    }
}

async fn get_task<R, L, D, I, W>(
    State(state): State<TaskApiState<R, L, D, I, W>>,
    Path(task_uid): Path<Uuid>,
    headers: HeaderMap,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger + TaskSubmissionLedger,
    D: DecisionChannel + Clone,
    I: TaskIdentityResolver,
    W: TaskWorkflowCatalog,
{
    let identity = match resolve_task_identity(&state.identities, &headers).await {
        Ok(identity) => identity,
        Err(error) => return error.into_response(),
    };
    let record = match state
        .ledger
        .task_for_submitter(
            task_uid,
            &identity.service,
            identity.canonical_user_id.as_str(),
        )
        .await
    {
        Ok(Some(record)) => record,
        Ok(None) => return ApiError::Store(StoreError::TaskNotFound).into_response(),
        Err(error) => return ApiError::Store(error).into_response(),
    };
    match status_response(record, Vec::new()) {
        Ok(response) => (StatusCode::OK, Json(response)).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn put_task_inputs<R, L, D, I, W>(
    State(state): State<TaskApiState<R, L, D, I, W>>,
    Path(task_uid): Path<Uuid>,
    headers: HeaderMap,
    archive: Result<Bytes, BytesRejection>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger + TaskSubmissionLedger,
    D: DecisionChannel + Clone,
    I: TaskIdentityResolver,
    W: TaskWorkflowCatalog,
{
    let archive = match archive {
        Ok(archive) => archive,
        Err(rejection) => {
            let status = rejection.status();
            let error = if status == StatusCode::PAYLOAD_TOO_LARGE {
                "Task input archive exceeds the 64 MiB limit"
            } else {
                "Task input archive body could not be read"
            };
            return (status, Json(serde_json::json!({"error": error}))).into_response();
        }
    };
    if headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        != Some("application/x-tar")
    {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Json(serde_json::json!({"error": "Content-Type must be application/x-tar"})),
        )
            .into_response();
    }
    let identity = match resolve_task_identity(&state.identities, &headers).await {
        Ok(identity) => identity,
        Err(error) => return error.into_response(),
    };
    match state
        .ledger
        .put_task_inputs(
            task_uid,
            &identity.service,
            identity.canonical_user_id.as_str(),
            &archive,
        )
        .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => ApiError::Store(error).into_response(),
    }
}

async fn submit_task<R, L, D, I, W>(
    State(state): State<TaskApiState<R, L, D, I, W>>,
    headers: HeaderMap,
    Json(request): Json<TaskSubmissionRequest>,
) -> Response
where
    R: RuntimeRepository,
    L: AdmissionLedger + TaskSubmissionLedger,
    D: DecisionChannel + Clone,
    I: TaskIdentityResolver,
    W: TaskWorkflowCatalog,
{
    match submit_task_inner(&state, &headers, &request).await {
        Ok((status, response)) => (status, Json(response)).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn submit_task_inner<R, L, D, I, W>(
    state: &TaskApiState<R, L, D, I, W>,
    headers: &HeaderMap,
    request: &TaskSubmissionRequest,
) -> Result<(StatusCode, TaskStatusResponse), ApiError>
where
    R: RuntimeRepository,
    L: AdmissionLedger + TaskSubmissionLedger,
    D: DecisionChannel + Clone,
    I: TaskIdentityResolver,
    W: TaskWorkflowCatalog,
{
    let idempotency_key = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::Admission("Idempotency-Key is required".to_owned()))?;
    let identity = resolve_task_identity(&state.identities, headers).await?;
    if let Some(reference) =
        versioned_workflow_reference(&request.workflow, request.coding_agent_runtime.as_deref())?
    {
        return submit_versioned_task(state, idempotency_key, identity, reference, request).await;
    }
    let workflow = state
        .workflows
        .workflow(&request.workflow)
        .ok_or(ApiError::TaskWorkflowNotFound)?;
    let coding_agent_runtime = request.coding_agent_runtime.as_deref().ok_or_else(|| {
        ApiError::Admission("legacy workflows require codingAgentRuntime".to_owned())
    })?;
    if coding_agent_runtime != workflow.coding_agent_runtime {
        return Err(ApiError::Admission(
            "codingAgentRuntime is not selected by the workflow".to_owned(),
        ));
    }
    let spec = AgentRuntimeSpec {
        principal: Principal::Service {
            name: identity.service.clone(),
            acting_user: identity.acting_user.clone(),
        },
        owner: identity.owner.clone(),
        canonical_authority: Some(
            CanonicalAuthorityBinding::new(
                identity.canonical_user_id.clone(),
                identity
                    .acting_user
                    .as_ref()
                    .map(|_| identity.canonical_user_id.clone()),
            )
            .map_err(ApiError::Admission)?,
        ),
        agent_type: steward_types::AgentType {
            name: workflow.coding_agent_runtime.clone(),
        },
        llms: workflow.llms.clone(),
        tools: workflow.tools.clone(),
        budget: workflow.budget.clone(),
        ttl: workflow.ttl.clone(),
        runner: steward_types::RunnerRequirements::default(),
        bindings: None,
    };
    let envelope = state
        .ledger
        .latest_service_envelope(&identity.service)
        .await
        .map_err(ApiError::Store)?
        .ok_or(ApiError::MissingEnvelope)?;
    let decision = evaluate_with_grants(&spec, &envelope, &[])
        .map_err(|error| ApiError::Admission(format!("{error:?}")))?;
    if let Some(runtime_uid) = request.agent_runtime_uid.as_deref() {
        if !matches!(decision, AdmissionDecision::Admit) {
            return Err(ApiError::Admission(
                "adopted runtime is outside the current service envelope".to_owned(),
            ));
        }
        let runtime = state
            .runtimes
            .get_by_uid(runtime_uid)
            .await
            .map_err(ApiError::Runtime)?;
        let runtime_namespace = runtime
            .namespace()
            .ok_or_else(|| ApiError::Runtime("adopted runtime has no namespace".to_owned()))?;
        if runtime_namespace != workflow.namespace
            || runtime.spec != spec
            || runtime
                .annotations()
                .contains_key(PENDING_APPROVAL_ANNOTATION)
        {
            return Err(ApiError::Conflict(
                "adopted runtime does not match the resolved workflow and principal".to_owned(),
            ));
        }
        let runtime_name = runtime.name_any();
        let reservation = state
            .ledger
            .reserve_task(TaskReservationRequest {
                idempotency_key,
                submitter_service: &identity.service,
                acting_user: identity.acting_user.as_ref().map(|email| email.0.as_str()),
                acting_user_id: identity
                    .acting_user
                    .as_ref()
                    .map(|_| identity.canonical_user_id.as_str()),
                owner: &identity.owner.0,
                owner_user_id: identity.canonical_user_id.as_str(),
                workflow: &workflow.name,
                workflow_name: None,
                workflow_version: None,
                workflow_digest: None,
                user_envelope_instance_id: None,
                user_envelope_revision: None,
                user_envelope_digest: None,
                coding_agent_runtime: &workflow.coding_agent_runtime,
                runtime_namespace: &runtime_namespace,
                runtime_name: &runtime_name,
                runtime_ownership: RuntimeOwnership::Adopted,
                runtime_spec: &spec,
                agent_command: &workflow.command,
                envelope_revision: envelope.revision,
            })
            .await
            .map_err(ApiError::Store)?;
        let record = if reservation.record.runtime_uid.is_none() {
            state
                .ledger
                .bind_task_runtime(
                    reservation.record.task_uid,
                    runtime_uid,
                    TaskPhase::Submitted,
                )
                .await
                .map_err(ApiError::Store)?
        } else {
            reservation.record
        };
        return task_response(record, Vec::new());
    }
    let runtime_name = stable_task_runtime_name(
        &identity.service,
        identity.canonical_user_id.as_str(),
        idempotency_key,
    );
    let reservation = state
        .ledger
        .reserve_task(TaskReservationRequest {
            idempotency_key,
            submitter_service: &identity.service,
            acting_user: identity.acting_user.as_ref().map(|email| email.0.as_str()),
            acting_user_id: identity
                .acting_user
                .as_ref()
                .map(|_| identity.canonical_user_id.as_str()),
            owner: &identity.owner.0,
            owner_user_id: identity.canonical_user_id.as_str(),
            workflow: &workflow.name,
            workflow_name: None,
            workflow_version: None,
            workflow_digest: None,
            user_envelope_instance_id: None,
            user_envelope_revision: None,
            user_envelope_digest: None,
            coding_agent_runtime: &workflow.coding_agent_runtime,
            runtime_namespace: &workflow.namespace,
            runtime_name: &runtime_name,
            runtime_ownership: RuntimeOwnership::Provisioned,
            runtime_spec: &spec,
            agent_command: &workflow.command,
            envelope_revision: envelope.revision,
        })
        .await
        .map_err(ApiError::Store)?;
    if !reservation.inserted && reservation.record.runtime_uid.is_some() {
        return task_response(reservation.record, Vec::new());
    }

    if matches!(&decision, AdmissionDecision::Admit) {
        return task_response(reservation.record, Vec::new());
    }

    let (runtime, phase, deltas) = create_task_runtime(
        &state.runtimes,
        &state.ledger,
        &state.decisions,
        TaskRuntimePlan {
            namespace: &workflow.namespace,
            name: &runtime_name,
            service: &identity.service,
            proposed_spec: &spec,
            envelope: &envelope,
            decision,
        },
    )
    .await?;
    let runtime_uid = runtime
        .metadata
        .uid
        .as_deref()
        .ok_or(ApiError::MissingRuntimeUid)?;
    let record = state
        .ledger
        .bind_task_runtime(reservation.record.task_uid, runtime_uid, phase)
        .await
        .map_err(ApiError::Store)?;
    task_response(record, deltas)
}

async fn submit_versioned_task<R, L, D, I, W>(
    state: &TaskApiState<R, L, D, I, W>,
    idempotency_key: &str,
    identity: TaskIdentity,
    reference: WorkflowReference,
    request: &TaskSubmissionRequest,
) -> Result<(StatusCode, TaskStatusResponse), ApiError>
where
    R: RuntimeRepository,
    L: AdmissionLedger + TaskSubmissionLedger,
    D: DecisionChannel + Clone,
    I: TaskIdentityResolver,
    W: TaskWorkflowCatalog,
{
    if request.agent_runtime_uid.is_some() {
        return Err(ApiError::Admission(
            "versioned Workflows use a server-owned runtime path".to_owned(),
        ));
    }
    let workflow = state
        .ledger
        .workflow_revision(&reference.name, reference.version)
        .await
        .map_err(ApiError::Store)?
        .ok_or(ApiError::TaskWorkflowNotFound)?;
    let envelopes = state
        .ledger
        .active_provisioned_user_envelopes(&identity.canonical_user_id)
        .await
        .map_err(ApiError::Store)?;
    let plan = resolve_versioned_task_plan(&identity, workflow, envelopes)?;
    let user_envelope = plan
        .envelope
        .approved_envelope
        .as_ref()
        .ok_or(ApiError::MissingEnvelope)?;
    if !matches!(
        evaluate_with_grants(&plan.spec, user_envelope, &[])
            .map_err(|error| ApiError::Admission(format!("{error:?}")))?,
        AdmissionDecision::Admit
    ) {
        return Err(ApiError::Admission(
            "Workflow runtime exceeds its pinned User Envelope".to_owned(),
        ));
    }
    let service_envelope = state
        .ledger
        .latest_service_envelope(&identity.service)
        .await
        .map_err(ApiError::Store)?
        .ok_or(ApiError::MissingEnvelope)?;
    let decision = evaluate_with_grants(&plan.spec, &service_envelope, &[])
        .map_err(|error| ApiError::Admission(format!("{error:?}")))?;
    let workflow_reference = format!("{}@{}", plan.workflow.name, plan.workflow.version);
    let runtime_name = stable_task_runtime_name(
        &identity.service,
        identity.canonical_user_id.as_str(),
        idempotency_key,
    );
    let envelope_instance_id = plan
        .envelope
        .envelope_instance_id
        .as_deref()
        .ok_or(ApiError::MissingEnvelope)?;
    let envelope_digest = plan
        .envelope
        .envelope_digest
        .as_deref()
        .ok_or(ApiError::MissingEnvelope)?;
    let reservation = state
        .ledger
        .reserve_task(TaskReservationRequest {
            idempotency_key,
            submitter_service: &identity.service,
            acting_user: identity.acting_user.as_ref().map(|email| email.0.as_str()),
            acting_user_id: identity
                .acting_user
                .as_ref()
                .map(|_| identity.canonical_user_id.as_str()),
            owner: &identity.owner.0,
            owner_user_id: identity.canonical_user_id.as_str(),
            workflow: &workflow_reference,
            workflow_name: Some(&plan.workflow.name),
            workflow_version: Some(plan.workflow.version),
            workflow_digest: Some(&plan.workflow.content_digest),
            user_envelope_instance_id: Some(envelope_instance_id),
            user_envelope_revision: Some(user_envelope.revision),
            user_envelope_digest: Some(envelope_digest),
            coding_agent_runtime: &plan.workflow.agent,
            runtime_namespace: VERSIONED_WORKFLOW_NAMESPACE,
            runtime_name: &runtime_name,
            runtime_ownership: RuntimeOwnership::Provisioned,
            runtime_spec: &plan.spec,
            agent_command: &plan.command,
            envelope_revision: service_envelope.revision,
        })
        .await
        .map_err(ApiError::Store)?;
    if !reservation.inserted && reservation.record.runtime_uid.is_some() {
        return task_response(reservation.record, Vec::new());
    }
    if matches!(&decision, AdmissionDecision::Admit) {
        return task_response(reservation.record, Vec::new());
    }
    let (runtime, phase, deltas) = create_task_runtime(
        &state.runtimes,
        &state.ledger,
        &state.decisions,
        TaskRuntimePlan {
            namespace: VERSIONED_WORKFLOW_NAMESPACE,
            name: &runtime_name,
            service: &identity.service,
            proposed_spec: &plan.spec,
            envelope: &service_envelope,
            decision,
        },
    )
    .await?;
    let runtime_uid = runtime
        .metadata
        .uid
        .as_deref()
        .ok_or(ApiError::MissingRuntimeUid)?;
    let record = state
        .ledger
        .bind_task_runtime(reservation.record.task_uid, runtime_uid, phase)
        .await
        .map_err(ApiError::Store)?;
    task_response(record, deltas)
}

async fn resolve_task_identity<I: TaskIdentityResolver>(
    identities: &I,
    headers: &HeaderMap,
) -> Result<TaskIdentity, ApiError> {
    let assertion = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| !value.is_empty())
        .ok_or(ApiError::TaskAuthentication)?;
    identities
        .resolve(assertion)
        .await
        .map_err(|error| match error {
            TaskAuthenticationError::InvalidCredentials => ApiError::TaskAuthentication,
            TaskAuthenticationError::Unavailable => ApiError::TaskAuthenticationUnavailable,
        })
}

struct TaskRuntimePlan<'a> {
    namespace: &'a str,
    name: &'a str,
    service: &'a str,
    proposed_spec: &'a AgentRuntimeSpec,
    envelope: &'a Envelope,
    decision: AdmissionDecision,
}

async fn create_task_runtime<R, L, D>(
    runtimes: &R,
    ledger: &L,
    decisions: &D,
    plan: TaskRuntimePlan<'_>,
) -> Result<(AgentRuntime, TaskPhase, Vec<AdmissionDelta>), ApiError>
where
    R: RuntimeRepository,
    L: AdmissionLedger,
    D: DecisionChannel,
{
    let mut runtime = AgentRuntime::new(plan.name, plan.proposed_spec.clone());
    runtime.metadata.namespace = Some(plan.namespace.to_owned());
    runtime.metadata.annotations = Some(BTreeMap::from([(
        SERVICE_PRINCIPAL_ANNOTATION.to_owned(),
        plan.service.to_owned(),
    )]));
    let AdmissionDecision::Reject { deltas } = plan.decision else {
        let created = create_or_get_matching_runtime(runtimes, plan.namespace, &runtime).await?;
        return Ok((created, TaskPhase::Submitted, Vec::new()));
    };
    let proposed_digest = spec_digest(plan.proposed_spec)?;
    runtime.spec.llms.clear();
    runtime.spec.tools.clear();
    runtime.spec.budget.monthly_limit = "0".to_owned();
    runtime.spec.budget.currency = plan.envelope.spec.budget.currency.clone();
    runtime.spec.ttl = plan.envelope.spec.ttl.clone();
    runtime.metadata.annotations.get_or_insert_default().insert(
        PENDING_APPROVAL_ANNOTATION.to_owned(),
        proposed_digest.clone(),
    );
    let created = create_or_get_matching_runtime(runtimes, plan.namespace, &runtime).await?;
    let runtime_uid = created
        .metadata
        .uid
        .as_deref()
        .ok_or(ApiError::MissingRuntimeUid)?;
    let base_digest = spec_digest(&created.spec)?;
    let parked = ledger
        .park_rejection(ParkRejection {
            runtime_uid,
            runtime_namespace: plan.namespace,
            runtime_name: plan.name,
            spec_digest: &proposed_digest,
            base_spec_digest: &base_digest,
            base_pending_approval_digest: Some(&proposed_digest),
            base_spec: &created.spec,
            envelope_revision: plan.envelope.revision,
            deltas: &deltas,
            proposed_spec: plan.proposed_spec,
            actor: plan.service,
            member_role: plan.service,
        })
        .await
        .map_err(ApiError::Store)?;
    if parked.decision_key.is_none() || parked.evidence_url.is_none() {
        file_decision_reference(ledger, decisions, parked.approval_id).await?;
    }
    Ok((created, TaskPhase::Parked, deltas))
}

async fn create_or_get_matching_runtime<R: RuntimeRepository>(
    runtimes: &R,
    namespace: &str,
    runtime: &AgentRuntime,
) -> Result<AgentRuntime, ApiError> {
    match runtimes.create_as_authority(namespace, runtime).await {
        Ok(created) => Ok(created),
        Err(RuntimeCreateError::Kubernetes { status: 409, .. }) => {
            let existing = runtimes
                .get(
                    namespace,
                    &runtime.metadata.name.clone().unwrap_or_default(),
                )
                .await
                .map_err(ApiError::Runtime)?;
            if existing.spec != runtime.spec || existing.annotations() != runtime.annotations() {
                return Err(ApiError::Conflict(
                    "task runtime name is bound to unrelated desired state".to_owned(),
                ));
            }
            Ok(existing)
        }
        Err(error) => Err(ApiError::RuntimeCreate(error)),
    }
}

fn task_response(
    record: TaskRecord,
    deltas: Vec<AdmissionDelta>,
) -> Result<(StatusCode, TaskStatusResponse), ApiError> {
    let status = if record.runtime_uid.is_none() || record.phase == TaskPhase::Parked {
        StatusCode::ACCEPTED
    } else if record.finalized {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((status, status_response(record, deltas)?))
}

fn status_response(
    record: TaskRecord,
    deltas: Vec<AdmissionDelta>,
) -> Result<TaskStatusResponse, ApiError> {
    Ok(TaskStatusResponse {
        task_uid: record.task_uid,
        runtime_uid: record.runtime_uid,
        phase: record.phase,
        runtime_ownership: record.runtime_ownership,
        finalized: record.finalized,
        failure_reason: record.failure_reason,
        deltas,
    })
}

fn stable_task_runtime_name(
    service: &str,
    canonical_owner_user_id: &str,
    idempotency_key: &str,
) -> String {
    let digest = Sha256::digest(
        format!("{service}\0{canonical_owner_user_id}\0{idempotency_key}").as_bytes(),
    );
    let suffix = digest[..10]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("task-{suffix}")
}

#[cfg(test)]
mod workflow_request_tests {
    use super::{
        StaticTaskWorkflowCatalog, TaskWorkflowCatalog, resolve_versioned_task_plan,
        versioned_workflow_reference,
    };
    use crate::{ApiError, TaskIdentity};
    use steward_admission::{Envelope, EnvelopeSpec};
    use steward_store::{EnvelopeRequestRecord, EnvelopeRequestStatus, WorkflowRevisionRecord};
    use steward_types::{
        Budget, CanonicalUserId, Duration, Email, ModelRef, RunnerRequirements, ToolGrant,
    };
    use uuid::Uuid;

    #[test]
    fn versioned_workflow_rejects_caller_selected_coding_runtime() {
        assert!(
            versioned_workflow_reference("repository-review@1", Some("base")).is_err(),
            "versioned Workflow requests must not let the caller select the coding runtime"
        );
    }

    #[test]
    fn malformed_versioned_workflow_does_not_fall_back_to_legacy_catalog() {
        for workflow in [
            "repository-review@latest",
            "repository-review@0",
            "repository-review@",
            "repository-review@1@2",
        ] {
            assert!(
                versioned_workflow_reference(workflow, None).is_err(),
                "malformed versioned reference {workflow:?} must not reach the legacy path"
            );
        }
    }

    #[test]
    fn empty_legacy_catalog_allows_versioned_only_deployments() -> Result<(), String> {
        let catalog = StaticTaskWorkflowCatalog::from_json("[]").map_err(|error| {
            format!("a versioned-Workflow deployment may omit legacy workflows: {error}")
        })?;
        assert!(catalog.workflow("legacy-smoke").is_none());
        Ok(())
    }

    fn identity(user_id: &str) -> Result<TaskIdentity, String> {
        Ok(TaskIdentity {
            service: "steward-run".to_owned(),
            acting_user: Some(Email("alice@example.com".to_owned())),
            owner: Email("alice@example.com".to_owned()),
            canonical_user_id: CanonicalUserId::parse(user_id)?,
        })
    }

    fn workflow() -> WorkflowRevisionRecord {
        WorkflowRevisionRecord {
            name: "repository-review".to_owned(),
            version: 1,
            display_name: "Repository review".to_owned(),
            agent: "codex@0.117.0".to_owned(),
            prompt: "Review the repository state.".to_owned(),
            content_digest: "workflow-digest".to_owned(),
            published_by: "usr_abcdef0123456789abcdef0123456789".to_owned(),
            published_at: "2026-08-24T00:00:00.000000Z".to_owned(),
        }
    }

    fn provisioned_envelope(owner_user_id: &str) -> Result<EnvelopeRequestRecord, String> {
        let envelope = Envelope {
            revision: 7,
            spec: EnvelopeSpec {
                llms: vec![ModelRef {
                    provider: "openai".to_owned(),
                    model: "gpt-5.4".to_owned(),
                }],
                tools: vec![ToolGrant {
                    provider: "github".to_owned(),
                    resource: "repository".to_owned(),
                    action: "get_file_contents".to_owned(),
                }],
                budget: Budget {
                    monthly_limit: "10.00".to_owned(),
                    single_run_limit: Some("1.00".to_owned()),
                    currency: "USD".to_owned(),
                },
                ttl: Duration("15m".to_owned()),
                runner: RunnerRequirements::default(),
            },
        };
        Ok(EnvelopeRequestRecord {
            id: Uuid::new_v4(),
            owner_user_id: CanonicalUserId::parse(owner_user_id)?,
            template_id: "developer".to_owned(),
            template_revision: 2,
            requested_envelope: envelope.clone(),
            approved_envelope: Some(envelope),
            status: EnvelopeRequestStatus::Provisioned,
            approval_id: None,
            envelope_instance_id: Some("env_instance_01".to_owned()),
            envelope_digest: Some("envelope-digest".to_owned()),
            reason: None,
            created_at: "2026-08-24T00:00:00.000000Z".to_owned(),
            status_at: "2026-08-24T00:00:01.000000Z".to_owned(),
        })
    }

    #[test]
    fn versioned_workflow_cannot_use_another_users_envelope() -> Result<(), String> {
        let result = resolve_versioned_task_plan(
            &identity("usr_0123456789abcdef0123456789abcdef")?,
            workflow(),
            vec![provisioned_envelope(
                "usr_abcdef0123456789abcdef0123456789",
            )?],
        );
        assert!(
            matches!(result, Err(ApiError::PrincipalMismatch)),
            "a versioned Task must reject an Envelope owned by another canonical user"
        );
        Ok(())
    }

    #[test]
    fn zero_or_ambiguous_provisioned_user_envelopes_fail_closed() -> Result<(), String> {
        let task_identity = identity("usr_0123456789abcdef0123456789abcdef")?;
        assert!(matches!(
            resolve_versioned_task_plan(&task_identity, workflow(), Vec::new()),
            Err(ApiError::MissingEnvelope)
        ));
        assert!(matches!(
            resolve_versioned_task_plan(
                &task_identity,
                workflow(),
                vec![
                    provisioned_envelope("usr_0123456789abcdef0123456789abcdef")?,
                    provisioned_envelope("usr_0123456789abcdef0123456789abcdef")?,
                ],
            ),
            Err(ApiError::Conflict(_))
        ));
        Ok(())
    }

    #[test]
    fn versioned_task_combines_workflow_agent_and_prompt_with_user_envelope_authority()
    -> Result<(), String> {
        let plan = resolve_versioned_task_plan(
            &identity("usr_0123456789abcdef0123456789abcdef")?,
            workflow(),
            vec![provisioned_envelope(
                "usr_0123456789abcdef0123456789abcdef",
            )?],
        )
        .map_err(|error| format!("one exact owned provisioned Envelope was rejected: {error:?}"))?;
        assert_eq!(plan.workflow.name, "repository-review");
        assert_eq!(plan.workflow.version, 1);
        assert_eq!(plan.spec.agent_type.name, "codex@0.117.0");
        assert_eq!(plan.spec.llms[0].model, "gpt-5.4");
        assert_eq!(plan.spec.tools[0].provider, "github");
        assert_eq!(plan.spec.budget.monthly_limit, "10.00");
        assert_eq!(plan.spec.budget.single_run_limit.as_deref(), Some("1.00"));
        assert_eq!(plan.spec.ttl.0, "15m");
        assert_eq!(plan.envelope.template_revision, 2);
        assert_eq!(
            plan.envelope.envelope_instance_id.as_deref(),
            Some("env_instance_01")
        );
        assert_eq!(
            plan.command.get(4).map(String::as_str),
            Some("Review the repository state."),
            "the immutable Workflow prompt must be a separate argument to the server-owned command"
        );
        assert!(
            plan.command
                .iter()
                .any(|argument| argument.contains("codex-cli 0.117.0")),
            "the server-owned command must fail closed unless the sandbox exposes the exact approved Codex version"
        );
        let shell_command = &plan.command[2];
        assert!(
            shell_command.contains("OPENAI_API_KEY=openshell-token-grant-placeholder"),
            "Codex must present a non-secret placeholder for OpenShell's runtime token grant"
        );
        assert!(
            shell_command.contains("litellm-litellm.litellm.svc.cluster.local:4000/v1"),
            "Codex must send inference to the OpenShell-governed LiteLLM endpoint"
        );
        assert!(
            shell_command.contains("--model \"$2\""),
            "Codex must use the model selected by the approved envelope"
        );
        assert!(
            shell_command.contains("ln -s \"$STEWARD_OUTPUT_DIR/out\" out"),
            "the declared out binding must resolve to Steward's collected output root"
        );
        assert!(
            shell_command.contains("--ephemeral")
                && shell_command.contains("--sandbox danger-full-access"),
            "Codex must leave persistence and sandboxing to the disposable OpenShell runtime"
        );
        assert_eq!(
            plan.command.get(5).map(String::as_str),
            Some("openai/gpt-5.4"),
            "the approved provider and model must be passed as an opaque shell argument"
        );
        assert!(
            plan.command
                .iter()
                .all(|argument| !argument.contains("example-org")),
            "repository mechanics do not belong to the Workflow execution command"
        );
        Ok(())
    }
}

#[cfg(test)]
mod identity_task_authentication_tests {
    use super::{
        IdentityTaskClaims, TaskAuthenticationError, task_identity_from_identity_claims,
        validate_identity_task_jwks, verify_identity_task_token,
    };
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use jsonwebtoken::jwk::JwkSet;
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
    use p256::SecretKey;
    use p256::elliptic_curve::sec1::ToEncodedPoint;
    use p256::pkcs8::EncodePrivateKey;
    use rand_core::OsRng;
    use serde::Serialize;

    const ISSUER: &str = "https://identity.localhost:18444";
    const AUDIENCE: &str = "steward-task-api";
    const KID: &str = "identity-task-current";

    #[derive(Serialize)]
    struct Claims<'a> {
        iss: &'a str,
        sub: &'a str,
        aud: Vec<&'a str>,
        exp: u64,
        iat: u64,
        nbf: u64,
        jti: &'a str,
        email: &'a str,
        email_verified: bool,
        groups: Vec<&'a str>,
        identity_contract: &'a str,
    }

    fn key_material() -> Result<(EncodingKey, JwkSet), String> {
        let private = SecretKey::random(&mut OsRng);
        let der = private
            .to_pkcs8_der()
            .map_err(|error| format!("encode P-256 test key: {error}"))?;
        let point = private.public_key().to_encoded_point(false);
        let x = point.x().ok_or("P-256 public key missing x")?;
        let y = point.y().ok_or("P-256 public key missing y")?;
        let jwks = serde_json::json!({
            "keys": [{
                "kty": "EC", "use": "sig", "alg": "ES256", "kid": KID,
                "crv": "P-256", "x": URL_SAFE_NO_PAD.encode(x), "y": URL_SAFE_NO_PAD.encode(y)
            }]
        });
        Ok((
            EncodingKey::from_ec_der(der.as_bytes()),
            serde_json::from_value(jwks).map_err(|error| format!("parse test JWKS: {error}"))?,
        ))
    }

    fn token(
        key: &EncodingKey,
        issuer: &str,
        audience: &str,
        contract: &str,
    ) -> Result<String, String> {
        let now = jsonwebtoken::get_current_timestamp();
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some(KID.to_owned());
        encode(
            &header,
            &Claims {
                iss: issuer,
                sub: "github-actions:actor:16106037",
                aud: vec![audience],
                exp: now + 60,
                iat: now,
                nbf: now.saturating_sub(1),
                jti: "identity-task-test-jti",
                email: "leo@apelogic.ai",
                email_verified: true,
                groups: vec![
                    "agents.apelogic.ai/acting-user:leo@apelogic.ai",
                    "agents.apelogic.ai/canonical-user:usr_528fc0fed6cf400abb93a3f327d9a809",
                    "agents.apelogic.ai/service-principal:steward-run",
                ],
                identity_contract: contract,
            },
            key,
        )
        .map_err(|error| format!("sign Identity task token: {error}"))
    }

    #[test]
    fn identity_task_token_verifies_exact_claims_and_maps_existing_group_contract()
    -> Result<(), String> {
        let (key, jwks) = key_material()?;
        validate_identity_task_jwks(&jwks)
            .map_err(|error| format!("valid JWKS rejected: {error:?}"))?;
        let assertion = token(&key, ISSUER, AUDIENCE, "steward-task-v2")?;
        let claims: IdentityTaskClaims =
            verify_identity_task_token(&assertion, &jwks, ISSUER, AUDIENCE)
                .map_err(|error| format!("valid Identity task credential rejected: {error:?}"))?;
        let identity = task_identity_from_identity_claims(claims)
            .map_err(|error| format!("valid ratified Identity groups rejected: {error:?}"))?;
        assert_eq!(identity.service, "steward-run");
        assert_eq!(identity.owner.0, "leo@apelogic.ai");
        assert_eq!(
            identity.canonical_user_id.as_str(),
            "usr_528fc0fed6cf400abb93a3f327d9a809"
        );
        Ok(())
    }

    #[test]
    fn identity_task_jwks_accepts_unselectable_workload_keys() -> Result<(), String> {
        let (key, jwks) = key_material()?;
        let mut mixed = serde_json::to_value(jwks)
            .map_err(|error| format!("serialize task JWKS fixture: {error}"))?;
        mixed["keys"]
            .as_array_mut()
            .ok_or("task JWKS fixture keys is not an array")?
            .push(serde_json::json!({
                "kty": "RSA", "use": "sig", "alg": "RS256", "kid": "identity-workload-current",
                "n": "not-used-for-task-authentication", "e": "AQAB"
            }));
        let mixed = serde_json::from_value(mixed)
            .map_err(|error| format!("parse mixed Identity JWKS fixture: {error}"))?;

        validate_identity_task_jwks(&mixed)
            .map_err(|error| format!("mixed Identity JWKS rejected: {error:?}"))?;
        let assertion = token(&key, ISSUER, AUDIENCE, "steward-task-v2")?;
        verify_identity_task_token(&assertion, &mixed, ISSUER, AUDIENCE)
            .map_err(|error| format!("ES256 task token rejected from mixed JWKS: {error:?}"))?;
        Ok(())
    }

    #[test]
    fn identity_task_token_rejects_wrong_issuer_audience_or_contract() -> Result<(), String> {
        let (key, jwks) = key_material()?;
        for (issuer, audience, contract) in [
            (
                "https://other.identity.invalid",
                AUDIENCE,
                "steward-task-v2",
            ),
            (ISSUER, "other-audience", "steward-task-v2"),
            (ISSUER, AUDIENCE, "steward-task-v1"),
        ] {
            let assertion = token(&key, issuer, audience, contract)?;
            assert!(
                matches!(
                    verify_identity_task_token(&assertion, &jwks, ISSUER, AUDIENCE),
                    Err(TaskAuthenticationError::InvalidCredentials)
                ),
                "unratified Identity task credential must fail closed"
            );
        }
        Ok(())
    }
}
