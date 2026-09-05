//! Vendor-neutral domain types shared by Steward components.

use std::borrow::Cow;

use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kube::{CustomResource, CustomResourceExt};
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};

pub const PENDING_APPROVAL_ANNOTATION: &str = "agents.apelogic.ai/pending-approval";
pub const TASK_EXECUTION_BINDING_ANNOTATION: &str = "agents.apelogic.ai/task-execution-binding";
pub const TASK_EXECUTION_BINDING_SCHEMA_VERSION: &str = "steward/task-execution-binding/v1";
pub const TASK_EXECUTION_BINDING_DIGEST_DOMAIN: &[u8] = b"steward.execution-bindings/v1\0";

pub fn runtime_activated_condition(observed_generation: i64) -> Condition {
    Condition {
        type_: "Activated".to_owned(),
        status: "True".to_owned(),
        observed_generation: Some(observed_generation),
        last_transition_time: Time(k8s_openapi::jiff::Timestamp::now()),
        reason: "PendingApprovalReleased".to_owned(),
        message: "standing delegation TTL starts at hold release".to_owned(),
    }
}

/// Stable identity for one runtime instance.
#[derive(Clone, Debug, Eq, Hash, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RuntimeId(pub String);

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(transparent)]
pub struct Email(pub String);

impl Email {
    pub fn parse(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if valid_email(&value) {
            Ok(Self(value))
        } else {
            Err("email must be a bounded email address".to_owned())
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for Email {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// Version of the stable person-identity contract shared by Steward control-plane surfaces.
pub const CANONICAL_PRINCIPAL_SCHEMA_VERSION: &str = "steward/canonical-principal/v1";

/// Version of the canonical person binding persisted with runtime authority.
pub const CANONICAL_AUTHORITY_BINDING_SCHEMA_VERSION: &str =
    "steward/canonical-authority-binding/v1";
pub const GOOGLE_ORGANIZATION_ISSUER: &str = "https://accounts.google.com";

/// Opaque, immutable identifier allocated by Steward for one person.
/// The value deliberately contains no email, identity-provider subject, or organization name.
#[derive(Clone, Debug, Eq, Hash, JsonSchema, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(transparent)]
pub struct CanonicalUserId(String);

impl CanonicalUserId {
    pub fn parse(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        let suffix = value
            .strip_prefix("usr_")
            .ok_or_else(|| "canonical user ID must use the usr_ prefix".to_owned())?;
        if suffix.len() != 32
            || !suffix
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(
                "canonical user ID must contain 32 lowercase hexadecimal characters".to_owned(),
            );
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for CanonicalUserId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// Minimum stable person binding carried by a live AgentRuntime.
///
/// This deliberately excludes email and external identity-provider claims. In v1, an acting
/// person is always the accountable owner; pure services omit `acting_user_id`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CanonicalAuthorityBinding {
    pub schema_version: String,
    pub owner_user_id: CanonicalUserId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acting_user_id: Option<CanonicalUserId>,
}

impl CanonicalAuthorityBinding {
    pub fn new(
        owner_user_id: CanonicalUserId,
        acting_user_id: Option<CanonicalUserId>,
    ) -> Result<Self, String> {
        if acting_user_id
            .as_ref()
            .is_some_and(|acting_user_id| acting_user_id != &owner_user_id)
        {
            return Err("canonical acting user must equal the accountable owner in v1".to_owned());
        }
        Ok(Self {
            schema_version: CANONICAL_AUTHORITY_BINDING_SCHEMA_VERSION.to_owned(),
            owner_user_id,
            acting_user_id,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CanonicalAuthorityBindingWire {
    schema_version: String,
    owner_user_id: CanonicalUserId,
    #[serde(default)]
    acting_user_id: Option<CanonicalUserId>,
}

impl<'de> Deserialize<'de> for CanonicalAuthorityBinding {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = CanonicalAuthorityBindingWire::deserialize(deserializer)?;
        if wire.schema_version != CANONICAL_AUTHORITY_BINDING_SCHEMA_VERSION {
            return Err(serde::de::Error::custom(
                "unsupported canonical authority binding schema version",
            ));
        }
        Self::new(wire.owner_user_id, wire.acting_user_id).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for CanonicalAuthorityBinding {
    fn schema_name() -> Cow<'static, str> {
        "CanonicalAuthorityBinding".into()
    }

    fn schema_id() -> Cow<'static, str> {
        concat!(module_path!(), "::CanonicalAuthorityBinding").into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let user_id = generator.subschema_for::<CanonicalUserId>();
        json_schema!({
            "type": "object",
            "additionalProperties": false,
            "required": ["schemaVersion", "ownerUserId"],
            "properties": {
                "schemaVersion": {
                    "type": "string",
                    "enum": [CANONICAL_AUTHORITY_BINDING_SCHEMA_VERSION]
                },
                "ownerUserId": user_id.clone(),
                "actingUserId": user_id
            },
            "x-kubernetes-validations": [{
                "rule": "!has(self.actingUserId) || self.actingUserId == self.ownerUserId",
                "message": "canonical acting user must equal the accountable owner in v1"
            }]
        })
    }
}

/// Stable organization boundary used when resolving an external identity.
#[derive(Clone, Debug, Eq, Hash, JsonSchema, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(transparent)]
pub struct OrganizationId(String);

impl OrganizationId {
    pub fn parse(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if value.len() < 3
            || value.len() > 64
            || !value.starts_with("org_")
            || !value.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
            })
        {
            return Err(
                "organization ID must use org_ and lowercase ASCII letters, digits, _ or -"
                    .to_owned(),
            );
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for OrganizationId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// Privacy-bounded person record returned after exact external-identity resolution.
///
/// `display_email` is recognition/contact metadata. It is never a lookup or ownership key.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CanonicalPrincipal {
    pub schema_version: String,
    pub user_id: CanonicalUserId,
    pub organization_id: OrganizationId,
    pub display_email: Email,
}

impl CanonicalPrincipal {
    pub fn new(
        user_id: CanonicalUserId,
        organization_id: OrganizationId,
        display_email: Email,
    ) -> Result<Self, String> {
        if !valid_email(&display_email.0) {
            return Err("display email must be a bounded email address".to_owned());
        }
        Ok(Self {
            schema_version: CANONICAL_PRINCIPAL_SCHEMA_VERSION.to_owned(),
            user_id,
            organization_id,
            display_email,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CanonicalPrincipalWire {
    schema_version: String,
    user_id: CanonicalUserId,
    organization_id: OrganizationId,
    display_email: Email,
}

impl<'de> Deserialize<'de> for CanonicalPrincipal {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = CanonicalPrincipalWire::deserialize(deserializer)?;
        if wire.schema_version != CANONICAL_PRINCIPAL_SCHEMA_VERSION {
            return Err(serde::de::Error::custom(
                "unsupported canonical principal schema version",
            ));
        }
        Self::new(wire.user_id, wire.organization_id, wire.display_email)
            .map_err(serde::de::Error::custom)
    }
}

/// Claims retained after the browser or workload token has been fully validated.
///
/// This type cannot carry a caller-selected Steward user ID or raw assertion.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OrganizationIdentity {
    issuer: String,
    subject: String,
    organization_claim: String,
    organization_id: OrganizationId,
    verified_email: Email,
}

/// Explicitly reviewed identity used only to attach an alternative issuer during migration.
///
/// This type is intentionally distinct from `OrganizationIdentity`: it cannot enter the normal
/// registration path, and every store mutation accepting it requires an audited actor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OrganizationIdentityMigration {
    issuer: String,
    subject: String,
    organization_claim: String,
    organization_id: OrganizationId,
    verified_email: Email,
}

/// Exact trust boundary applied after a supported organization OIDC token is verified.
///
/// The normal registration path is pinned to Google's exact issuer and `hd` claim. A reviewed
/// issuer migration uses the distinct `OrganizationIdentityMigration` type instead.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OrganizationIdentityPolicy {
    issuer: String,
    organization_claim: String,
    organization_id: OrganizationId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OrganizationIdentityPolicyWire {
    issuer: String,
    organization_claim: String,
    organization_id: OrganizationId,
}

impl OrganizationIdentityPolicy {
    pub fn new(
        issuer: impl Into<String>,
        organization_claim: impl Into<String>,
        organization_id: OrganizationId,
    ) -> Result<Self, String> {
        let issuer = issuer.into();
        let organization_claim = organization_claim.into();
        if issuer != GOOGLE_ORGANIZATION_ISSUER {
            return Err("organization identity policy requires the exact Google issuer".to_owned());
        }
        validate_organization_claim(&organization_claim)?;
        Ok(Self {
            issuer,
            organization_claim,
            organization_id,
        })
    }

    pub fn validate(
        &self,
        issuer: &str,
        subject: &str,
        organization_claim: &str,
        email: &str,
        email_verified: bool,
    ) -> Result<OrganizationIdentity, String> {
        if issuer != self.issuer || organization_claim != self.organization_claim {
            return Err("organization identity is outside the exact trusted boundary".to_owned());
        }
        if !email_verified {
            return Err("organization email must be verified".to_owned());
        }
        let verified_email = Email::parse(email.to_owned())?;
        let email_domain = email
            .rsplit_once('@')
            .map(|(_, domain)| domain)
            .ok_or_else(|| "organization email must be bounded".to_owned())?;
        if !email_domain.eq_ignore_ascii_case(&self.organization_claim) {
            return Err("organization email is outside the exact hosted domain".to_owned());
        }
        OrganizationIdentity::new_validated(
            issuer,
            subject,
            organization_claim,
            self.organization_id.clone(),
            verified_email,
        )
    }
}

impl<'de> Deserialize<'de> for OrganizationIdentityPolicy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = OrganizationIdentityPolicyWire::deserialize(deserializer)?;
        Self::new(wire.issuer, wire.organization_claim, wire.organization_id)
            .map_err(serde::de::Error::custom)
    }
}

impl OrganizationIdentity {
    fn new_validated(
        issuer: impl Into<String>,
        subject: impl Into<String>,
        organization_claim: impl Into<String>,
        organization_id: OrganizationId,
        verified_email: Email,
    ) -> Result<Self, String> {
        let issuer = issuer.into();
        let subject = subject.into();
        let organization_claim = organization_claim.into();
        validate_exact_https_issuer(&issuer)?;
        if subject.is_empty()
            || subject.len() > 255
            || subject.bytes().any(|byte| byte.is_ascii_whitespace())
        {
            return Err("organization subject must be a bounded immutable value".to_owned());
        }
        validate_organization_claim(&organization_claim)?;
        if !valid_email(&verified_email.0) {
            return Err("organization email must be verified and bounded".to_owned());
        }
        Ok(Self {
            issuer,
            subject,
            organization_claim,
            organization_id,
            verified_email,
        })
    }

    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    pub fn subject(&self) -> &str {
        &self.subject
    }

    pub fn organization_claim(&self) -> &str {
        &self.organization_claim
    }

    pub fn organization_id(&self) -> &OrganizationId {
        &self.organization_id
    }

    pub fn verified_email(&self) -> &Email {
        &self.verified_email
    }
}

impl OrganizationIdentityMigration {
    pub fn new_reviewed(
        issuer: impl Into<String>,
        subject: impl Into<String>,
        organization_claim: impl Into<String>,
        organization_id: OrganizationId,
        verified_email: Email,
    ) -> Result<Self, String> {
        let identity = OrganizationIdentity::new_validated(
            issuer,
            subject,
            organization_claim,
            organization_id,
            verified_email,
        )?;
        Ok(Self {
            issuer: identity.issuer,
            subject: identity.subject,
            organization_claim: identity.organization_claim,
            organization_id: identity.organization_id,
            verified_email: identity.verified_email,
        })
    }

    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    pub fn subject(&self) -> &str {
        &self.subject
    }

    pub fn organization_claim(&self) -> &str {
        &self.organization_claim
    }

    pub fn organization_id(&self) -> &OrganizationId {
        &self.organization_id
    }

    pub fn verified_email(&self) -> &Email {
        &self.verified_email
    }
}

fn validate_exact_https_issuer(issuer: &str) -> Result<(), String> {
    if issuer.starts_with("https://")
        && !issuer.contains(['?', '#'])
        && !issuer.bytes().any(|byte| byte.is_ascii_whitespace())
    {
        Ok(())
    } else {
        Err("organization issuer must be an exact trusted HTTPS issuer".to_owned())
    }
}

fn validate_organization_claim(organization_claim: &str) -> Result<(), String> {
    if !organization_claim.is_empty()
        && organization_claim.len() <= 253
        && organization_claim.contains('.')
        && organization_claim.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-')
        })
    {
        Ok(())
    } else {
        Err("organization claim must be an exact lowercase domain boundary".to_owned())
    }
}

fn valid_email(value: &str) -> bool {
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && domain.contains('.')
        && value.len() <= 254
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'@' | b'.' | b'_' | b'+' | b'-')
        })
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Principal {
    User {
        acting_user: Email,
    },
    Service {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        acting_user: Option<Email>,
    },
}

impl JsonSchema for Principal {
    fn schema_name() -> Cow<'static, str> {
        "Principal".into()
    }

    fn schema_id() -> Cow<'static, str> {
        concat!(module_path!(), "::Principal").into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let email = generator.subschema_for::<Email>();
        json_schema!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["user", "service"]
                },
                "actingUser": email,
                "name": {
                    "type": "string",
                    "minLength": 1
                }
            },
            "required": ["kind"],
            "additionalProperties": false,
            "x-kubernetes-validations": [{
                "rule": "(self.kind == 'user' && has(self.actingUser) && !has(self.name)) || (self.kind == 'service' && has(self.name))",
                "message": "user principals require actingUser and no name; service principals require name and may carry actingUser"
            }]
        })
    }
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentType {
    pub name: String,
}

/// An immutable server-resolved execution binding persisted with a Task.
///
/// This is deliberately separate from `AgentRuntimeSpec`: portable artifacts and public
/// runtime requests cannot select deployment images, executable paths, native profiles, or
/// execution lifetime.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum TaskExecutionBinding {
    Disposable(DisposableExecutionBinding),
    Resident(ResidentExecutionBinding),
}

impl TaskExecutionBinding {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Disposable(binding) => binding.validate(),
            Self::Resident(binding) => binding.validate(),
        }
    }

    pub const fn task_owns_runtime(&self) -> bool {
        matches!(self, Self::Disposable(_))
    }

    pub const fn disposable(&self) -> Option<&DisposableExecutionBinding> {
        match self {
            Self::Disposable(binding) => Some(binding),
            Self::Resident(_) => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DisposableExecutionBinding {
    pub schema_version: String,
    pub binding_id: String,
    pub binding_digest: String,
    pub agent_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub adapter: String,
    pub image: String,
    pub executable: String,
    pub version_probe: ExecutionVersionProbe,
    pub provider_profiles: ExecutionProviderProfiles,
}

impl DisposableExecutionBinding {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != TASK_EXECUTION_BINDING_SCHEMA_VERSION
            || !valid_sha256_digest(&self.binding_id)
            || !valid_sha256_digest(&self.binding_digest)
            || self.binding_id != self.binding_digest
            || !valid_versioned_binding_reference(&self.agent_ref)
            || self
                .display_name
                .as_deref()
                .is_some_and(|value| !valid_bounded_display_name(value))
            || !valid_binding_slug(&self.adapter)
            || !valid_digest_pinned_image(&self.image)
            || !valid_absolute_executable(&self.executable)
            || self.version_probe.validate().is_err()
            || self.provider_profiles.validate().is_err()
        {
            return Err("disposable execution binding is incomplete or mutable".to_owned());
        }
        Ok(())
    }

    pub fn canonical_content(&self) -> Result<Vec<u8>, String> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Content<'a> {
            agent_ref: &'a str,
            adapter: &'a str,
            image: &'a str,
            executable: &'a str,
            version_probe: &'a ExecutionVersionProbe,
            provider_profiles: &'a ExecutionProviderProfiles,
        }

        serde_json::to_vec(&Content {
            agent_ref: &self.agent_ref,
            adapter: &self.adapter,
            image: &self.image,
            executable: &self.executable,
            version_probe: &self.version_probe,
            provider_profiles: &self.provider_profiles,
        })
        .map_err(|error| format!("execution binding cannot be canonicalized: {error}"))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutionVersionProbe {
    pub arguments: Vec<String>,
    pub expected_stdout: String,
}

impl ExecutionVersionProbe {
    pub fn validate(&self) -> Result<(), String> {
        if self.arguments.is_empty()
            || self.arguments.len() > 8
            || self.arguments.iter().any(|argument| {
                argument.is_empty()
                    || argument.chars().count() > 128
                    || argument.chars().any(char::is_control)
            })
            || !valid_bounded_binding_scalar(&self.expected_stdout)
        {
            return Err("execution version probe is invalid or unbounded".to_owned());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutionProviderProfiles {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<ExecutionProviderProfile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inference: Option<ExecutionProviderProfile>,
}

impl ExecutionProviderProfiles {
    pub fn validate(&self) -> Result<(), String> {
        for profile in [&self.tools, &self.inference].into_iter().flatten() {
            profile.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutionProviderProfile {
    pub id: String,
    pub digest: String,
}

impl ExecutionProviderProfile {
    pub fn validate(&self) -> Result<(), String> {
        if !valid_bounded_binding_scalar(&self.id)
            || self.id.chars().any(char::is_whitespace)
            || !valid_sha256_digest(&self.digest)
        {
            return Err("execution provider profile is incomplete or mutable".to_owned());
        }
        Ok(())
    }
}

/// Exact lease pins required before a Task may use an AgentInstance-owned runtime.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResidentExecutionBinding {
    pub schema_version: String,
    pub binding_id: String,
    pub binding_digest: String,
    pub owner_user_id: CanonicalUserId,
    pub agent_instance_id: String,
    pub agent_instance_revision: i64,
    pub runtime_uid: RuntimeId,
    pub runtime_spec_digest: String,
    pub standing_authority_digest: String,
    pub deployment_binding_digest: String,
    pub freshness_generation: i64,
}

/// Controller-observed facts compared with an immutable resident binding before dispatch.
pub struct ResidentExecutionContext<'a> {
    pub owner_user_id: &'a CanonicalUserId,
    pub agent_instance_id: &'a str,
    pub agent_instance_revision: i64,
    pub runtime_uid: &'a RuntimeId,
    pub runtime_spec_digest: &'a str,
    pub standing_authority_digest: &'a str,
    pub deployment_binding_digest: &'a str,
    pub freshness_generation: i64,
}

impl ResidentExecutionBinding {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != TASK_EXECUTION_BINDING_SCHEMA_VERSION
            || !valid_binding_slug(&self.binding_id)
            || !valid_sha256_digest(&self.binding_digest)
            || !valid_binding_slug(&self.agent_instance_id)
            || self.agent_instance_revision <= 0
            || self.runtime_uid.0.is_empty()
            || self.runtime_uid.0.len() > 255
            || self.runtime_uid.0.chars().any(char::is_control)
            || !valid_sha256_digest(&self.runtime_spec_digest)
            || !valid_sha256_digest(&self.standing_authority_digest)
            || !valid_sha256_digest(&self.deployment_binding_digest)
            || self.freshness_generation <= 0
        {
            return Err("resident execution binding is incomplete or mutable".to_owned());
        }
        Ok(())
    }

    pub fn matches_context(&self, context: &ResidentExecutionContext<'_>) -> bool {
        self.validate().is_ok()
            && &self.owner_user_id == context.owner_user_id
            && self.agent_instance_id == context.agent_instance_id
            && self.agent_instance_revision == context.agent_instance_revision
            && &self.runtime_uid == context.runtime_uid
            && self.runtime_spec_digest == context.runtime_spec_digest
            && self.standing_authority_digest == context.standing_authority_digest
            && self.deployment_binding_digest == context.deployment_binding_digest
            && self.freshness_generation == context.freshness_generation
    }
}

fn valid_binding_slug(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && bytes[0].is_ascii_lowercase()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

fn valid_sha256_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn valid_versioned_binding_reference(value: &str) -> bool {
    let Some((name, version)) = value.rsplit_once('@') else {
        return false;
    };
    !name.is_empty()
        && !version.is_empty()
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && version.as_bytes()[0].is_ascii_digit()
        && version != "latest"
        && value.len() <= 255
        && !name.contains('@')
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'-' | b'_' | b'.' | b'/')
        })
        && version.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'-' | b'_' | b'.' | b'+')
        })
}

fn valid_digest_pinned_image(value: &str) -> bool {
    const SHA256_SUFFIX_BYTES: usize = "@sha256:".len() + 64;
    const OCI_REPOSITORY_MAX_BYTES: usize = 255;
    if value.len() > OCI_REPOSITORY_MAX_BYTES + SHA256_SUFFIX_BYTES {
        return false;
    }
    let Some((repository, digest)) = value.split_once("@sha256:") else {
        return false;
    };
    if repository.len() > OCI_REPOSITORY_MAX_BYTES || repository.contains('@') {
        return false;
    }
    let mut components = repository.split('/');
    let Some(registry) = components.next() else {
        return false;
    };
    valid_oci_registry(registry)
        && components.clone().next().is_some()
        && components.all(valid_oci_path_component)
        && digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_oci_registry(value: &str) -> bool {
    let (host, port) = value
        .split_once(':')
        .map_or((value, None), |(host, port)| (host, Some(port)));
    if host.contains(':')
        || port.is_some_and(|port| {
            port.is_empty()
                || port.len() > 5
                || port.starts_with('0')
                || !port.bytes().all(|byte| byte.is_ascii_digit())
                || port.parse::<u16>().ok().is_none_or(|port| port == 0)
        })
    {
        return false;
    }
    host.len() <= 253
        && host.split('.').all(|label| {
            let bytes = label.as_bytes();
            !bytes.is_empty()
                && bytes.len() <= 63
                && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
                && bytes[bytes.len() - 1].is_ascii_alphanumeric()
                && bytes
                    .iter()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
        })
}

fn valid_oci_path_component(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit() {
        return false;
    }
    let mut index = 1;
    while index < bytes.len() {
        if bytes[index].is_ascii_lowercase() || bytes[index].is_ascii_digit() {
            index += 1;
            continue;
        }
        match bytes[index] {
            b'.' => index += 1,
            b'_' => {
                index += 1;
                if bytes.get(index) == Some(&b'_') {
                    index += 1;
                }
            }
            b'-' => {
                while bytes.get(index) == Some(&b'-') {
                    index += 1;
                }
            }
            _ => return false,
        }
        if bytes
            .get(index)
            .is_none_or(|byte| !byte.is_ascii_lowercase() && !byte.is_ascii_digit())
        {
            return false;
        }
        index += 1;
    }
    true
}

fn valid_absolute_executable(value: &str) -> bool {
    value.starts_with('/')
        && value.len() <= 1024
        && !value.ends_with('/')
        && !value.contains("//")
        && !value
            .split('/')
            .any(|component| matches!(component, "." | ".."))
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'_' | b'.' | b'-'))
}

fn valid_bounded_binding_scalar(value: &str) -> bool {
    !value.is_empty()
        && value.chars().count() <= 255
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn valid_bounded_display_name(value: &str) -> bool {
    !value.is_empty()
        && value.chars().count() <= 200
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelRef {
    pub provider: String,
    pub model: String,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolGrant {
    pub provider: String,
    pub resource: String,
    pub action: String,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Budget {
    pub monthly_limit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub single_run_limit: Option<String>,
    pub currency: String,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(transparent)]
pub struct Duration(pub String);

/// Canonical platform names accepted by the governed runner contract.
#[derive(
    Clone, Copy, Debug, Eq, Hash, JsonSchema, PartialEq, Serialize, Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RunnerPlatform {
    Linux,
    Mac,
    Windows,
}

/// A Kubernetes resource quantity. Admission validates the supported unit family for each
/// resource dimension before it becomes authority.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(transparent)]
pub struct KubernetesQuantity(pub String);

#[derive(
    Clone, Debug, Default, Eq, JsonSchema, PartialEq, Serialize, Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "camelCase")]
pub struct RunnerRequirements {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub platforms: Vec<RunnerPlatform>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<KubernetesQuantity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compute: Option<KubernetesQuantity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<KubernetesQuantity>,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(transparent)]
pub struct BindingRef(pub String);

#[derive(
    CustomResource, Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize, utoipa::ToSchema,
)]
#[kube(
    group = "agents.apelogic.ai",
    version = "v1alpha1",
    kind = "AgentRuntime",
    namespaced,
    status = "AgentRuntimeStatus",
    shortname = "ar",
    schema = "derived"
)]
#[serde(rename_all = "camelCase")]
pub struct AgentRuntimeSpec {
    pub principal: Principal,
    pub owner: Email,
    /// Server-derived stable person authority. Missing means legacy/reconnect-required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_authority: Option<CanonicalAuthorityBinding>,
    pub agent_type: AgentType,
    pub llms: Vec<ModelRef>,
    pub tools: Vec<ToolGrant>,
    pub budget: Budget,
    pub ttl: Duration,
    #[serde(default)]
    pub runner: RunnerRequirements,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bindings: Option<Vec<BindingRef>>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRuntimeStatus {
    pub phase: Phase,
    pub observed_generation: i64,
    pub spec_digest: String,
    pub refs: RuntimeRefs,
    pub conditions: Vec<Condition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spend: Option<SpendSummary>,
}

#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
pub enum Phase {
    Pending,
    Admitted,
    Provisioning,
    Running,
    Suspended,
    Terminating,
    Terminated,
    Failed,
}

#[derive(Clone, Debug, Default, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeRefs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub litellm_key: Option<String>,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpendSummary {
    pub observed_amount: String,
    pub currency: String,
}

/// Durable lifecycle state for one single-shot governed task.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum TaskPhase {
    Submitted,
    Parked,
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum RuntimeOwnership {
    Provisioned,
    Adopted,
}

pub fn agent_runtime_crd() -> CustomResourceDefinition {
    AgentRuntime::crd()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        CANONICAL_AUTHORITY_BINDING_SCHEMA_VERSION, CANONICAL_PRINCIPAL_SCHEMA_VERSION,
        CanonicalAuthorityBinding, CanonicalPrincipal, CanonicalUserId, Email, OrganizationId,
        OrganizationIdentityPolicy, Principal, ResidentExecutionBinding, ResidentExecutionContext,
        RuntimeId, TASK_EXECUTION_BINDING_SCHEMA_VERSION, TaskExecutionBinding, agent_runtime_crd,
        valid_digest_pinned_image,
    };

    fn digest(byte: char) -> String {
        format!("sha256:{}", byte.to_string().repeat(64))
    }

    fn resident_binding() -> Result<ResidentExecutionBinding, String> {
        Ok(ResidentExecutionBinding {
            schema_version: TASK_EXECUTION_BINDING_SCHEMA_VERSION.to_owned(),
            binding_id: "resident-agent-instance-v1".to_owned(),
            binding_digest: digest('a'),
            owner_user_id: CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?,
            agent_instance_id: "agent-instance-01".to_owned(),
            agent_instance_revision: 3,
            runtime_uid: RuntimeId("runtime-uid-01".to_owned()),
            runtime_spec_digest: digest('b'),
            standing_authority_digest: digest('c'),
            deployment_binding_digest: digest('d'),
            freshness_generation: 7,
        })
    }

    #[test]
    fn resident_execution_binding_rejects_every_cross_lease_dimension() -> Result<(), String> {
        let binding = resident_binding()?;
        binding.validate()?;
        let other_owner = CanonicalUserId::parse("usr_abcdef0123456789abcdef0123456789")?;
        let other_runtime = RuntimeId("runtime-uid-02".to_owned());
        let contexts = [
            ResidentExecutionContext {
                owner_user_id: &other_owner,
                agent_instance_id: &binding.agent_instance_id,
                agent_instance_revision: binding.agent_instance_revision,
                runtime_uid: &binding.runtime_uid,
                runtime_spec_digest: &binding.runtime_spec_digest,
                standing_authority_digest: &binding.standing_authority_digest,
                deployment_binding_digest: &binding.deployment_binding_digest,
                freshness_generation: binding.freshness_generation,
            },
            ResidentExecutionContext {
                owner_user_id: &binding.owner_user_id,
                agent_instance_id: "agent-instance-02",
                agent_instance_revision: binding.agent_instance_revision,
                runtime_uid: &binding.runtime_uid,
                runtime_spec_digest: &binding.runtime_spec_digest,
                standing_authority_digest: &binding.standing_authority_digest,
                deployment_binding_digest: &binding.deployment_binding_digest,
                freshness_generation: binding.freshness_generation,
            },
            ResidentExecutionContext {
                owner_user_id: &binding.owner_user_id,
                agent_instance_id: &binding.agent_instance_id,
                agent_instance_revision: 4,
                runtime_uid: &binding.runtime_uid,
                runtime_spec_digest: &binding.runtime_spec_digest,
                standing_authority_digest: &binding.standing_authority_digest,
                deployment_binding_digest: &binding.deployment_binding_digest,
                freshness_generation: binding.freshness_generation,
            },
            ResidentExecutionContext {
                owner_user_id: &binding.owner_user_id,
                agent_instance_id: &binding.agent_instance_id,
                agent_instance_revision: binding.agent_instance_revision,
                runtime_uid: &other_runtime,
                runtime_spec_digest: &binding.runtime_spec_digest,
                standing_authority_digest: &binding.standing_authority_digest,
                deployment_binding_digest: &binding.deployment_binding_digest,
                freshness_generation: binding.freshness_generation,
            },
            ResidentExecutionContext {
                owner_user_id: &binding.owner_user_id,
                agent_instance_id: &binding.agent_instance_id,
                agent_instance_revision: binding.agent_instance_revision,
                runtime_uid: &binding.runtime_uid,
                runtime_spec_digest: &digest('e'),
                standing_authority_digest: &binding.standing_authority_digest,
                deployment_binding_digest: &binding.deployment_binding_digest,
                freshness_generation: binding.freshness_generation,
            },
            ResidentExecutionContext {
                owner_user_id: &binding.owner_user_id,
                agent_instance_id: &binding.agent_instance_id,
                agent_instance_revision: binding.agent_instance_revision,
                runtime_uid: &binding.runtime_uid,
                runtime_spec_digest: &binding.runtime_spec_digest,
                standing_authority_digest: &digest('e'),
                deployment_binding_digest: &binding.deployment_binding_digest,
                freshness_generation: binding.freshness_generation,
            },
            ResidentExecutionContext {
                owner_user_id: &binding.owner_user_id,
                agent_instance_id: &binding.agent_instance_id,
                agent_instance_revision: binding.agent_instance_revision,
                runtime_uid: &binding.runtime_uid,
                runtime_spec_digest: &binding.runtime_spec_digest,
                standing_authority_digest: &binding.standing_authority_digest,
                deployment_binding_digest: &digest('e'),
                freshness_generation: binding.freshness_generation,
            },
            ResidentExecutionContext {
                owner_user_id: &binding.owner_user_id,
                agent_instance_id: &binding.agent_instance_id,
                agent_instance_revision: binding.agent_instance_revision,
                runtime_uid: &binding.runtime_uid,
                runtime_spec_digest: &binding.runtime_spec_digest,
                standing_authority_digest: &binding.standing_authority_digest,
                deployment_binding_digest: &binding.deployment_binding_digest,
                freshness_generation: 8,
            },
        ];
        for context in contexts {
            assert!(
                !binding.matches_context(&context),
                "resident dispatch must reject a context that crosses any immutable lease pin"
            );
        }
        let matching = ResidentExecutionContext {
            owner_user_id: &binding.owner_user_id,
            agent_instance_id: &binding.agent_instance_id,
            agent_instance_revision: binding.agent_instance_revision,
            runtime_uid: &binding.runtime_uid,
            runtime_spec_digest: &binding.runtime_spec_digest,
            standing_authority_digest: &binding.standing_authority_digest,
            deployment_binding_digest: &binding.deployment_binding_digest,
            freshness_generation: binding.freshness_generation,
        };
        assert!(binding.matches_context(&matching));
        assert!(!TaskExecutionBinding::Resident(binding).task_owns_runtime());
        Ok(())
    }

    #[test]
    fn digest_pinned_images_require_bounded_oci_names() {
        let digest = "a".repeat(64);
        assert!(valid_digest_pinned_image(&format!(
            "registry.example.test:5000/team-a/agent@sha256:{digest}"
        )));
        for invalid in [
            format!("registry::5000/agent@sha256:{digest}"),
            format!("registry.example.test:00001/agent@sha256:{digest}"),
            format!("registry.example.test/team-a/-agent@sha256:{digest}"),
            format!("registry.example.test/team-a/agent..v1@sha256:{digest}"),
            format!(
                "registry.example.test/{}/agent@sha256:{digest}",
                "a".repeat(240)
            ),
        ] {
            assert!(
                !valid_digest_pinned_image(&invalid),
                "accepted malformed or unbounded OCI image reference {invalid}"
            );
        }
    }

    #[test]
    fn canonical_principal_uses_an_opaque_immutable_id_not_email() -> Result<(), String> {
        let principal = CanonicalPrincipal::new(
            CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?,
            OrganizationId::parse("org_example")?,
            Email("alice@example.com".to_owned()),
        )?;

        assert_eq!(principal.schema_version, CANONICAL_PRINCIPAL_SCHEMA_VERSION);
        assert_eq!(
            principal.user_id.as_str(),
            "usr_0123456789abcdef0123456789abcdef"
        );
        assert_eq!(principal.organization_id.as_str(), "org_example");
        assert_eq!(principal.display_email.0, "alice@example.com");

        let renamed = CanonicalPrincipal::new(
            principal.user_id.clone(),
            principal.organization_id.clone(),
            Email("alice-renamed@example.com".to_owned()),
        )?;
        assert_eq!(renamed.user_id, principal.user_id);
        assert_ne!(renamed.display_email, principal.display_email);
        Ok(())
    }

    #[test]
    fn canonical_principal_rejects_ambiguous_or_caller_shaped_identifiers() {
        for value in [
            "alice@example.com",
            "usr_0123456789ABCDEF0123456789ABCDEF",
            "usr_0123",
            "0123456789abcdef0123456789abcdef",
        ] {
            assert!(CanonicalUserId::parse(value).is_err(), "accepted {value}");
        }
        for value in ["", "example org", "alice@example.com", "org_/example"] {
            assert!(OrganizationId::parse(value).is_err(), "accepted {value}");
        }
    }

    #[test]
    fn canonical_authority_binding_is_versioned_minimal_and_owner_bound() -> Result<(), String> {
        let owner = CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?;
        let delegated = CanonicalAuthorityBinding::new(owner.clone(), Some(owner.clone()))?;
        assert_eq!(
            serde_json::to_value(&delegated)
                .map_err(|error| format!("failed to serialize canonical authority: {error}"))?,
            json!({
                "schemaVersion": CANONICAL_AUTHORITY_BINDING_SCHEMA_VERSION,
                "ownerUserId": owner.as_str(),
                "actingUserId": owner.as_str()
            })
        );
        let pure_service = CanonicalAuthorityBinding::new(owner.clone(), None)?;
        assert_eq!(
            serde_json::to_value(&pure_service)
                .map_err(|error| format!("failed to serialize service authority: {error}"))?,
            json!({
                "schemaVersion": CANONICAL_AUTHORITY_BINDING_SCHEMA_VERSION,
                "ownerUserId": owner.as_str()
            })
        );

        for invalid in [
            json!({
                "schemaVersion": "steward/canonical-authority-binding/v2",
                "ownerUserId": owner.as_str()
            }),
            json!({
                "schemaVersion": CANONICAL_AUTHORITY_BINDING_SCHEMA_VERSION,
                "ownerUserId": owner.as_str(),
                "actingUserId": "usr_abcdef0123456789abcdef0123456789"
            }),
            json!({
                "schemaVersion": CANONICAL_AUTHORITY_BINDING_SCHEMA_VERSION,
                "ownerUserId": owner.as_str(),
                "email": "alice@example.com"
            }),
            json!({
                "schemaVersion": CANONICAL_AUTHORITY_BINDING_SCHEMA_VERSION,
                "ownerUserId": "alice@example.com"
            }),
        ] {
            assert!(
                serde_json::from_value::<CanonicalAuthorityBinding>(invalid).is_err(),
                "invalid canonical authority binding was accepted"
            );
        }
        let crd = serde_json::to_value(agent_runtime_crd())
            .map_err(|error| format!("failed to inspect AgentRuntime CRD: {error}"))?;
        assert_eq!(
            crd.pointer(
                "/spec/versions/0/schema/openAPIV3Schema/properties/spec/properties/canonicalAuthority/x-kubernetes-validations/0/rule",
            )
            .and_then(serde_json::Value::as_str),
            Some("!has(self.actingUserId) || self.actingUserId == self.ownerUserId"),
            "the Kubernetes schema must enforce the v1 acting/owner invariant"
        );
        Ok(())
    }

    #[test]
    fn canonical_principal_contract_rejects_unknown_fields_and_unverified_email_shapes()
    -> Result<(), String> {
        let unknown = serde_json::from_value::<CanonicalPrincipal>(json!({
            "schemaVersion": "steward/canonical-principal/v1",
            "userId": "usr_0123456789abcdef0123456789abcdef",
            "organizationId": "org_example",
            "displayEmail": "alice@example.com",
            "actingUser": "bob@example.org"
        }));
        assert!(unknown.is_err());

        for invalid in [
            json!({
                "schemaVersion": "steward/canonical-principal/v1",
                "userId": "alice@example.com",
                "organizationId": "org_example",
                "displayEmail": "alice@example.com"
            }),
            json!({
                "schemaVersion": "steward/canonical-principal/v1",
                "userId": "usr_0123456789abcdef0123456789abcdef",
                "organizationId": "example.com",
                "displayEmail": "alice@example.com"
            }),
            json!({
                "schemaVersion": "steward/canonical-principal/v1",
                "userId": "usr_0123456789abcdef0123456789abcdef",
                "organizationId": "org_example",
                "displayEmail": "not-an-email"
            }),
            json!({
                "schemaVersion": "steward/canonical-principal/v2",
                "userId": "usr_0123456789abcdef0123456789abcdef",
                "organizationId": "org_example",
                "displayEmail": "alice@example.com"
            }),
        ] {
            assert!(
                serde_json::from_value::<CanonicalPrincipal>(invalid).is_err(),
                "deserialization bypassed canonical identifier validation"
            );
        }

        assert!(
            CanonicalPrincipal::new(
                CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?,
                OrganizationId::parse("org_example")?,
                Email("not-an-email".to_owned()),
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn organization_identity_is_exact_and_carries_no_caller_selected_user() -> Result<(), String> {
        let identity = OrganizationIdentityPolicy::new(
            "https://accounts.google.com",
            "example.com",
            OrganizationId::parse("org_example")?,
        )?
        .validate(
            "https://accounts.google.com",
            "immutable-subject-001",
            "example.com",
            "alice@example.com",
            true,
        )?;
        let value = serde_json::to_value(identity).map_err(|error| error.to_string())?;
        assert_eq!(value["issuer"], "https://accounts.google.com");
        assert_eq!(value["subject"], "immutable-subject-001");
        assert_eq!(value["organizationClaim"], "example.com");
        assert!(value.get("userId").is_none());
        assert!(value.get("token").is_none());
        Ok(())
    }

    #[test]
    fn organization_identity_rejects_wrong_issuer_and_ambiguous_subject() -> Result<(), String> {
        let organization = OrganizationId::parse("org_example")?;
        let policy = OrganizationIdentityPolicy::new(
            "https://accounts.google.com",
            "example.com",
            organization.clone(),
        )?;
        for (issuer, subject, hosted_domain) in [
            (
                "http://accounts.google.com",
                "immutable-subject-001",
                "example.com",
            ),
            ("https://accounts.google.com", "", "example.com"),
            (
                "https://accounts.google.com",
                "subject with spaces",
                "example.com",
            ),
            (
                "https://accounts.google.com?other=tenant",
                "subject-001",
                "example.com",
            ),
            ("https://accounts.google.com", "subject-001", "other_domain"),
        ] {
            assert!(
                policy
                    .validate(issuer, subject, hosted_domain, "alice@example.com", true,)
                    .is_err(),
                "accepted issuer={issuer} subject={subject} organization={hosted_domain}"
            );
        }
        Ok(())
    }

    #[test]
    fn direct_google_policy_requires_exact_issuer_hosted_domain_and_verified_email()
    -> Result<(), String> {
        assert!(
            OrganizationIdentityPolicy::new(
                "https://login.example.test",
                "example.com",
                OrganizationId::parse("org_example")?,
            )
            .is_err(),
            "an arbitrary HTTPS issuer must not become a registration trust policy"
        );
        let policy = OrganizationIdentityPolicy::new(
            "https://accounts.google.com",
            "example.com",
            OrganizationId::parse("org_example")?,
        )?;
        let identity = policy.validate(
            "https://accounts.google.com",
            "immutable-google-subject",
            "example.com",
            "alice@example.com",
            true,
        )?;
        assert_eq!(identity.issuer(), "https://accounts.google.com");
        assert_eq!(identity.organization_claim(), "example.com");

        for (issuer, hosted_domain, email, verified) in [
            (
                "https://accounts.google.com/",
                "example.com",
                "alice@example.com",
                true,
            ),
            (
                "https://accounts.google.com",
                "other.example",
                "alice@example.com",
                true,
            ),
            (
                "https://accounts.google.com",
                "example.com",
                "alice@other.example",
                true,
            ),
            (
                "https://accounts.google.com",
                "example.com",
                "alice@example.com",
                false,
            ),
        ] {
            assert!(
                policy
                    .validate(
                        issuer,
                        "immutable-google-subject",
                        hosted_domain,
                        email,
                        verified,
                    )
                    .is_err(),
                "accepted mismatched organization identity issuer={issuer} hd={hosted_domain} email={email} verified={verified}"
            );
        }
        Ok(())
    }

    #[test]
    fn principal_wire_shape_is_exclusive_and_structurally_validated() -> Result<(), String> {
        let user = serde_json::to_value(Principal::User {
            acting_user: Email("alice@example.com".to_owned()),
        })
        .map_err(|error| format!("failed to serialize user principal: {error}"))?;
        assert_eq!(
            user,
            json!({"kind": "user", "actingUser": "alice@example.com"})
        );
        assert!(
            serde_json::from_value::<Principal>(
                json!({"kind": "user", "actingUser": "alice@example.com", "name": "service-a"})
            )
            .is_err(),
            "a principal must not carry both user and service identity"
        );

        let delegated_service = serde_json::from_value::<Principal>(json!({
            "kind": "service",
            "name": "steward-run",
            "actingUser": "alice@example.com"
        }))
        .map_err(|error| format!("delegated service principal must deserialize: {error}"))?;
        let delegated_service = serde_json::to_value(delegated_service)
            .map_err(|error| format!("failed to serialize delegated service principal: {error}"))?;
        assert_eq!(
            delegated_service,
            json!({
                "kind": "service",
                "name": "steward-run",
                "actingUser": "alice@example.com"
            })
        );
        let pure_service = serde_json::to_value(Principal::Service {
            name: "scheduled-scanner".to_owned(),
            acting_user: None,
        })
        .map_err(|error| format!("failed to serialize pure service principal: {error}"))?;
        assert_eq!(
            pure_service,
            json!({"kind": "service", "name": "scheduled-scanner"})
        );

        let crd = serde_json::to_value(agent_runtime_crd())
            .map_err(|error| format!("failed to inspect AgentRuntime CRD: {error}"))?;
        let validations = crd
            .pointer(
                "/spec/versions/0/schema/openAPIV3Schema/properties/spec/properties/principal/x-kubernetes-validations",
            )
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| "Principal CRD schema must carry an exclusivity rule".to_owned())?;
        assert_eq!(
            validations.len(),
            1,
            "Principal CRD schema must have one authoritative exclusivity rule"
        );
        let rule = validations[0]
            .get("rule")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "Principal CRD validation must contain a CEL rule".to_owned())?;
        assert!(
            rule.contains("self.kind == 'service' && has(self.name)"),
            "service principals must require a service name"
        );
        assert!(
            !rule.contains("has(self.name) && !has(self.actingUser)"),
            "service principals must allow an optional resolved acting user"
        );
        Ok(())
    }
}
