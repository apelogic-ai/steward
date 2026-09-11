//! Frozen wire types for direct Git package Task invocation.
//!
//! These types are additive to the frozen `steward.m1/v1` catalog contract. They keep
//! source references and authority selection explicit while leaving source retrieval,
//! authorization, and Envelope lookup to their owning services.

use std::collections::BTreeSet;

use serde::{Deserialize, Deserializer, Serialize};

pub const DIRECT_TASK_CONTRACT_VERSION: &str = "steward.task/v2";
pub const DIRECT_TASK_DEFINITION_SCHEMA: &str = "steward.task-definition/v2";
pub const INSTRUCTION_SKILL_SCHEMA: &str = "steward.instruction-skill/v1";
pub const SOURCE_PROVENANCE_CONTRACT_VERSION: &str = "steward.source-provenance/v1";
pub const PACKAGE_CLOSURE_CONTRACT_VERSION: &str = "steward.package-closure/v1";
pub const TASK_BINDING_EVIDENCE_SCHEMA: &str = "steward.task/source-authority-evidence/v1";
pub const SOURCE_PROVENANCE_JWT_CLAIM: &str = "source_provenance";
pub const EXECUTION_STDOUT_ARCHIVE_PATH: &str = ".steward/diagnostics/stdout.log";
pub const EXECUTION_STDERR_ARCHIVE_PATH: &str = ".steward/diagnostics/stderr.log";
pub const MAX_EXECUTION_STREAM_BYTES: u64 = 4 * 1024 * 1024;
pub const MAX_EXECUTION_TRANSCRIPT_BYTES: u64 = 2 * MAX_EXECUTION_STREAM_BYTES;
pub const MAX_PACKAGE_FILES: usize = 128;
pub const MAX_PACKAGE_FILE_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_PACKAGE_CLOSURE_BYTES: u64 = 16 * 1024 * 1024;

macro_rules! validated_string {
    ($name:ident, $validator:ident) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn parse(value: impl Into<String>) -> Result<Self, String> {
                let value = value.into();
                if $validator(&value) {
                    Ok(Self(value))
                } else {
                    Err(concat!(stringify!($name), " is invalid").to_owned())
                }
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
            }
        }
    };
}

validated_string!(RepositoryUrl, valid_repository_url);
validated_string!(RelativePath, valid_relative_path);
validated_string!(ExactGitCommit, valid_exact_git_commit);
validated_string!(EnvelopeDigest, valid_envelope_digest);
validated_string!(ContentDigest, valid_content_digest);
validated_string!(StableProviderId, valid_stable_provider_id);
validated_string!(Uuid, valid_uuid);
validated_string!(Slug, valid_slug);
validated_string!(AgentRef, valid_agent_ref);
validated_string!(BoundedText, valid_bounded_text);
validated_string!(BoundedRef, valid_bounded_ref);
validated_string!(Decimal, valid_decimal);
validated_string!(Duration, valid_duration);
validated_string!(ResourceQuantity, valid_resource_quantity);

fn valid_repository_url(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("https://") else {
        return false;
    };
    if value.len() > 512
        || value.contains(['?', '#', '@', '\\'])
        || !value.ends_with(".git")
        || rest.ends_with("/.git")
    {
        return false;
    }
    let Some((host, path)) = rest.split_once('/') else {
        return false;
    };
    !host.is_empty()
        && host.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-')
        })
        && host.contains('.')
        && path
            .strip_suffix(".git")
            .is_some_and(|path| valid_repository_path(path) && path.contains('/'))
}

fn valid_repository_path(value: &str) -> bool {
    !value.is_empty()
        && value.split('/').all(|component| {
            !component.is_empty()
                && !matches!(component, "." | "..")
                && component
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        })
}

fn valid_relative_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && !value.starts_with('/')
        && !value.ends_with('/')
        && !value.contains('\\')
        && value.split('/').all(|component| {
            !component.is_empty()
                && !matches!(component, "." | "..")
                && component
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        })
}

fn valid_exact_git_commit(value: &str) -> bool {
    value.strip_prefix("git:sha1:").is_some_and(|hex| {
        hex.len() == 40
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn valid_envelope_digest(value: &str) -> bool {
    valid_typed_sha256(value, "steward:sha256:")
}

fn valid_content_digest(value: &str) -> bool {
    valid_typed_sha256(value, "steward:sha256:")
}

fn valid_typed_sha256(value: &str, prefix: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn valid_stable_provider_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 20 && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn valid_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && [8, 13, 18, 23]
            .into_iter()
            .all(|index| bytes.get(index) == Some(&b'-'))
        && bytes.iter().enumerate().all(|(index, byte)| {
            [8, 13, 18, 23].contains(&index)
                || byte.is_ascii_digit()
                || (b'a'..=b'f').contains(byte)
        })
}

fn valid_slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn valid_agent_ref(value: &str) -> bool {
    let Some((name, version)) = value.split_once('@') else {
        return false;
    };
    value.len() <= 255
        && !version.contains('@')
        && !name.is_empty()
        && name
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
        && version
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_digit())
        && version.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'.' | b'_' | b'-' | b'+')
        })
}

fn valid_bounded_text(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn valid_bounded_ref(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 2048
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn valid_decimal(value: &str) -> bool {
    let mut parts = value.split('.');
    let Some(whole) = parts.next() else {
        return false;
    };
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || whole.len() > 1 && whole.starts_with('0')
    {
        return false;
    }
    match (parts.next(), parts.next()) {
        (None, None) => true,
        (Some(fraction), None) => {
            !fraction.is_empty()
                && fraction.len() <= 6
                && fraction.bytes().all(|byte| byte.is_ascii_digit())
        }
        _ => false,
    }
}

fn valid_duration(value: &str) -> bool {
    let Some(unit) = value.chars().last() else {
        return false;
    };
    matches!(unit, 's' | 'm' | 'h')
        && value[..value.len() - 1]
            .parse::<u64>()
            .is_ok_and(|amount| amount > 0)
}

fn valid_resource_quantity(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
}

fn require_version(actual: &str, expected: &str, kind: &str) -> Result<(), String> {
    if actual == expected {
        Ok(())
    } else {
        Err(format!("unsupported {kind} version"))
    }
}

fn require_unique_paths<'a>(
    paths: impl IntoIterator<Item = &'a RelativePath>,
) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for path in paths {
        if !seen.insert(path.as_str()) {
            return Err(format!("duplicate package path {}", path.as_str()));
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DirectTaskSubmission {
    pub contract_version: String,
    pub invocation_path: RelativePath,
}

impl DirectTaskSubmission {
    pub fn validate(&self) -> Result<(), String> {
        require_version(
            &self.contract_version,
            DIRECT_TASK_CONTRACT_VERSION,
            "direct Task contract",
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InvocationManifest {
    pub contract_version: String,
    pub package: PackageReference,
    pub envelope: EnvelopeDigest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<DiagnosticsRequest>,
}

impl InvocationManifest {
    pub fn validate_for_invoking_repository(
        &self,
        invoking_repository: &RepositoryUrl,
    ) -> Result<(), String> {
        require_version(
            &self.contract_version,
            DIRECT_TASK_CONTRACT_VERSION,
            "invocation contract",
        )?;
        if matches!(self.package.commit, PackageCommit::Trigger)
            && self.package.repository != *invoking_repository
        {
            return Err(
                "git:trigger is valid only for a package in the invoking repository".to_owned(),
            );
        }
        Ok(())
    }

    pub fn effective_diagnostics(&self) -> DiagnosticsRequest {
        self.diagnostics.unwrap_or_default()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PackageReference {
    pub repository: RepositoryUrl,
    pub commit: PackageCommit,
    pub path: RelativePath,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PackageCommit {
    Exact(ExactGitCommit),
    Trigger,
}

impl Serialize for PackageCommit {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Exact(commit) => serializer.serialize_str(commit.as_str()),
            Self::Trigger => serializer.serialize_str("git:trigger"),
        }
    }
}

impl<'de> Deserialize<'de> for PackageCommit {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value == "git:trigger" {
            Ok(Self::Trigger)
        } else {
            ExactGitCommit::parse(value)
                .map(Self::Exact)
                .map_err(serde::de::Error::custom)
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionLogMode {
    #[default]
    Off,
    Full,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DiagnosticsRequest {
    #[serde(default)]
    pub execution_log: ExecutionLogMode,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DirectTaskStatusResponse {
    pub contract_version: String,
    pub task_uid: Uuid,
    pub runtime_uid: Option<BoundedText>,
    pub phase: DirectTaskPhase,
    pub runtime_ownership: DirectRuntimeOwnership,
    pub finalized: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<BoundedText>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deltas: Vec<DirectAdmissionDelta>,
    pub diagnostics: DiagnosticsRequest,
}

impl DirectTaskStatusResponse {
    pub fn validate(&self) -> Result<(), String> {
        require_version(
            &self.contract_version,
            DIRECT_TASK_CONTRACT_VERSION,
            "direct Task status",
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DirectTaskPhase {
    Submitted,
    Parked,
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DirectRuntimeOwnership {
    Provisioned,
    Adopted,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "dimension",
    deny_unknown_fields
)]
pub enum DirectAdmissionDelta {
    Budget {
        requested: Decimal,
        ceiling: Decimal,
        currency: Currency,
    },
    SingleRunBudget {
        requested: Option<Decimal>,
        ceiling: Decimal,
        currency: Currency,
    },
    Ttl {
        requested: Duration,
        ceiling: Duration,
    },
    Models {
        requested: Vec<ModelRequirement>,
        ceiling: Vec<ModelRequirement>,
    },
    Tools {
        requested: Vec<ToolRequirement>,
        ceiling: Vec<ToolRequirement>,
    },
    RunnerPlatforms {
        requested: Vec<RunnerPlatform>,
        ceiling: Vec<RunnerPlatform>,
    },
    RunnerMemory {
        requested: ResourceQuantity,
        ceiling: Option<ResourceQuantity>,
    },
    RunnerCompute {
        requested: ResourceQuantity,
        ceiling: Option<ResourceQuantity>,
    },
    RunnerStorage {
        requested: ResourceQuantity,
        ceiling: Option<ResourceQuantity>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DirectTaskDefinition {
    pub schema_version: String,
    pub name: Slug,
    pub version: u64,
    pub runtime: RuntimeSelection,
    pub prompt: RelativePath,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<RelativePath>,
    pub outputs: Vec<DeclaredOutput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires: Option<DirectRequirements>,
}

impl DirectTaskDefinition {
    pub fn validate(&self) -> Result<(), String> {
        require_version(
            &self.schema_version,
            DIRECT_TASK_DEFINITION_SCHEMA,
            "TaskDefinition schema",
        )?;
        if self.version == 0 {
            return Err("TaskDefinition version must be positive".to_owned());
        }
        if self.outputs.is_empty() {
            return Err("TaskDefinition must declare at least one output".to_owned());
        }
        if self.outputs.iter().any(|output| {
            output.path.as_str() != "out" && !output.path.as_str().starts_with("out/")
        }) {
            return Err("declared outputs must remain beneath the out directory".to_owned());
        }
        require_unique_paths(self.skills.iter())?;
        require_unique_paths(self.outputs.iter().map(|output| &output.path))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeSelection {
    pub agent_ref: AgentRef,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputKind {
    Directory,
    File,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeclaredOutput {
    pub path: RelativePath,
    pub kind: OutputKind,
    pub required: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InstructionSkill {
    pub schema_version: String,
    pub name: Slug,
    pub description: BoundedText,
    #[serde(default)]
    pub kind: SkillKind,
    pub instructions: RelativePath,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assets: Vec<RelativePath>,
}

impl InstructionSkill {
    pub fn validate(&self) -> Result<(), String> {
        require_version(
            &self.schema_version,
            INSTRUCTION_SKILL_SCHEMA,
            "instruction skill schema",
        )?;
        require_unique_paths(self.assets.iter())
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillKind {
    #[default]
    InstructionOnly,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DirectRequirements {
    pub execution: ExecutionRequirements,
    pub authority: AuthorityRequirements,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutionRequirements {
    pub capabilities: Vec<ExecutionCapability>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionCapability {
    Shell,
    Python3,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthorityRequirements {
    pub llms: Vec<ModelRequirement>,
    pub tools: Vec<ToolRequirement>,
    pub budget: BudgetRequirement,
    pub ttl: Duration,
    pub runner: RunnerRequirement,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelRequirement {
    pub provider: Slug,
    pub model: BoundedText,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolRequirement {
    pub provider: Slug,
    pub resource: BoundedText,
    pub action: BoundedText,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BudgetRequirement {
    pub monthly_limit: Decimal,
    pub single_run_limit: Decimal,
    pub currency: Currency,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Currency(String);

impl<'de> Deserialize<'de> for Currency {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value.len() == 3 && value.bytes().all(|byte| byte.is_ascii_uppercase()) {
            Ok(Self(value))
        } else {
            Err(serde::de::Error::custom(
                "currency must be three uppercase ASCII letters",
            ))
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunnerRequirement {
    pub platforms: Vec<RunnerPlatform>,
    pub memory: ResourceQuantity,
    pub compute: ResourceQuantity,
    pub storage: ResourceQuantity,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerPlatform {
    Linux,
    Mac,
    Windows,
}

impl DirectRequirements {
    pub fn validate(&self) -> Result<(), String> {
        require_unique_values(&self.execution.capabilities, "execution capability")?;
        require_unique_values(&self.authority.llms, "model requirement")?;
        require_unique_values(&self.authority.tools, "tool requirement")?;
        require_unique_values(&self.authority.runner.platforms, "runner platform")
    }
}

fn require_unique_values<T: Ord + std::fmt::Debug>(values: &[T], kind: &str) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for value in values {
        if !seen.insert(value) {
            return Err(format!("duplicate {kind}: {value:?}"));
        }
    }
    Ok(())
}

impl Ord for ModelRequirement {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (&self.provider, &self.model).cmp(&(&other.provider, &other.model))
    }
}

impl PartialOrd for ModelRequirement {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ToolRequirement {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (&self.provider, &self.resource, &self.action).cmp(&(
            &other.provider,
            &other.resource,
            &other.action,
        ))
    }
}

impl PartialOrd for ToolRequirement {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceProvenance {
    pub contract_version: String,
    pub provider: SourceProvider,
    pub repository: TriggerRepository,
    pub triggered_sha: ExactGitCommit,
    pub run: WorkflowRun,
    pub event: BoundedText,
    #[serde(rename = "ref")]
    pub git_ref: BoundedRef,
    pub actor_id: StableProviderId,
    pub actor: BoundedText,
    pub caller_workflow: WorkflowIdentity,
    pub reusable_workflow: WorkflowIdentity,
}

impl SourceProvenance {
    pub fn validate(&self) -> Result<(), String> {
        require_version(
            &self.contract_version,
            SOURCE_PROVENANCE_CONTRACT_VERSION,
            "source provenance contract",
        )?;
        if self.run.attempt == 0 {
            return Err("workflow run attempt must be positive".to_owned());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceProvider {
    Github,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TriggerRepository {
    pub id: StableProviderId,
    pub owner_id: StableProviderId,
    pub name: BoundedText,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowRun {
    pub id: StableProviderId,
    pub attempt: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowIdentity {
    #[serde(rename = "ref")]
    pub workflow_ref: BoundedRef,
    pub sha: ExactGitCommit,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClosureEntryKind {
    TaskDefinition,
    Prompt,
    InstructionSkill,
    Instructions,
    Asset,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClosureEntry {
    pub kind: ClosureEntryKind,
    pub path: RelativePath,
    pub digest: ContentDigest,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PackageClosure {
    pub contract_version: String,
    pub entry_point: RelativePath,
    pub entries: Vec<ClosureEntry>,
}

impl PackageClosure {
    pub fn validate(&self) -> Result<(), String> {
        require_version(
            &self.contract_version,
            PACKAGE_CLOSURE_CONTRACT_VERSION,
            "package closure contract",
        )?;
        if self.entries.is_empty() || self.entries.len() > MAX_PACKAGE_FILES {
            return Err("package closure file count is outside the allowed bounds".to_owned());
        }
        if self
            .entries
            .windows(2)
            .any(|pair| pair[0].path >= pair[1].path)
        {
            return Err(
                "package closure entries must have unique paths in ascending order".to_owned(),
            );
        }
        let package_root = self
            .entry_point
            .as_str()
            .rsplit_once('/')
            .map_or("", |(directory, _)| directory);
        if self.entries.iter().any(|entry| {
            !package_root.is_empty()
                && entry.path.as_str() != package_root
                && !entry.path.as_str().starts_with(&format!("{package_root}/"))
        }) {
            return Err("package closure entries must remain beneath the package root".to_owned());
        }
        if self
            .entries
            .iter()
            .filter(|entry| {
                entry.kind == ClosureEntryKind::TaskDefinition && entry.path == self.entry_point
            })
            .count()
            != 1
        {
            return Err(
                "package closure must contain its TaskDefinition entry point once".to_owned(),
            );
        }
        let mut total = 0_u64;
        for entry in &self.entries {
            if entry.size_bytes > MAX_PACKAGE_FILE_BYTES {
                return Err(format!(
                    "package file {} exceeds the size limit",
                    entry.path.as_str()
                ));
            }
            total = total
                .checked_add(entry.size_bytes)
                .ok_or_else(|| "package closure size overflow".to_owned())?;
        }
        if total > MAX_PACKAGE_CLOSURE_BYTES {
            return Err("package closure exceeds the total size limit".to_owned());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResolvedSource {
    pub repository: RepositoryUrl,
    pub repository_id: StableProviderId,
    pub repository_owner_id: StableProviderId,
    pub commit: ExactGitCommit,
    pub path: RelativePath,
    pub content_digest: ContentDigest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnvelopeEvidence {
    pub uid: Uuid,
    pub revision: u64,
    pub digest: EnvelopeDigest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DirectTaskBindingEvidence {
    pub schema_version: String,
    pub task_uid: Uuid,
    pub source_provenance: SourceProvenance,
    pub invocation: ResolvedSource,
    pub package: ResolvedSource,
    pub closure: PackageClosure,
    pub closure_digest: ContentDigest,
    pub envelope: EnvelopeEvidence,
    pub effective_requirements: DirectRequirements,
    pub diagnostics: DiagnosticsRequest,
}

impl DirectTaskBindingEvidence {
    pub fn validate(&self) -> Result<(), String> {
        require_version(
            &self.schema_version,
            TASK_BINDING_EVIDENCE_SCHEMA,
            "Task source-authority evidence schema",
        )?;
        self.source_provenance.validate()?;
        self.closure.validate()?;
        self.effective_requirements.validate()
    }
}

/// Return deterministic canonical JSON bytes for a contract value.
///
/// Contract numbers are non-negative integers and authority decimals remain strings. Turning the
/// value into `serde_json::Value` first therefore gives the RFC 8785 member ordering required by
/// the direct-package digest profile without an additional canonicalization dependency.
pub fn canonical_json_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    let value = serde_json::to_value(value)
        .map_err(|error| format!("contract value cannot be canonicalized: {error}"))?;
    serde_json::to_vec(&value)
        .map_err(|error| format!("canonical contract value cannot be encoded: {error}"))
}
