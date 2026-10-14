//! Shared admission boundary for every desired-state writer.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};
use steward_types::direct_package::{ResourceQuantity, WorkspaceAuthority, WorkspaceEntryType};
use steward_types::{
    AgentRuntimeSpec, Budget, Duration, KubernetesQuantity, ModelRef, RunnerPlatform,
    RunnerRequirements, SpendSummary, ToolGrant,
};

/// Product-owned authorities that are versioned independently from mutable user and service
/// envelopes. A published version is immutable; changed authority requires a new module/version.
pub mod internal_authorities {
    use super::{Envelope, EnvelopeSpec};
    use steward_types::{
        Budget, Duration, KubernetesQuantity, RunnerPlatform, RunnerRequirements, ToolGrant,
    };

    pub mod steward_connections_v1 {
        use super::{
            Budget, Duration, Envelope, EnvelopeSpec, KubernetesQuantity, RunnerPlatform,
            RunnerRequirements, ToolGrant,
        };

        pub const AUTHORITY_ID: &str = "steward-connections";
        pub const AUTHORITY_VERSION: i64 = 1;
        pub const AUTHORITY_DIGEST: &str =
            "sha256:7735d22e083daef4bdbd51bb63a652720ef06f5499422e7a8eef4930a6c58663";
        pub const SERVICE: &str = "steward-connections";
        pub const AGENT_TYPE: &str = "connections-bridge";
        pub const BRIDGE_BINARY: &str = "/usr/local/bin/steward-connections-bridge";
        pub const INPUT_FILE: &str = "request.json";
        pub const OUTPUT_FILE: &str = "response.json";
        pub const RESPONSE_DEADLINE_SECONDS: i64 = 40;
        pub const MCP_GW_VERSION: &str = "0.3.2";
        pub const OAUTH_STATE_LIFETIME_SECONDS: i64 = 600;
        pub const OAUTH_CLOCK_SKEW_SECONDS: i64 = 30;

        pub fn provider_control_grant(action: &str) -> Option<ToolGrant> {
            matches!(action, "status" | "start" | "disconnect").then(|| ToolGrant {
                provider: "github".to_owned(),
                resource: "provider-control".to_owned(),
                action: action.to_owned(),
            })
        }

        pub fn envelope() -> Envelope {
            Envelope {
                revision: AUTHORITY_VERSION,
                spec: EnvelopeSpec {
                    llms: Vec::new(),
                    tools: ["status", "start", "disconnect"]
                        .into_iter()
                        .filter_map(provider_control_grant)
                        .collect(),
                    budget: Budget {
                        monthly_limit: "0.00".to_owned(),
                        single_run_limit: Some("0.00".to_owned()),
                        currency: "USD".to_owned(),
                    },
                    runtime_minutes_limit: None,
                    ttl: Duration("2m".to_owned()),
                    runner: RunnerRequirements {
                        platforms: vec![RunnerPlatform::Linux],
                        memory: Some(KubernetesQuantity("128Mi".to_owned())),
                        compute: Some(KubernetesQuantity("100m".to_owned())),
                        storage: Some(KubernetesQuantity("64Mi".to_owned())),
                    },
                    workspace: None,
                },
            }
        }
    }

    pub mod steward_connections_v2 {
        use super::{Envelope, steward_connections_v1};
        use steward_types::ToolGrant;

        pub const AUTHORITY_ID: &str = steward_connections_v1::AUTHORITY_ID;
        pub const AUTHORITY_VERSION: i64 = 2;
        pub const AUTHORITY_DIGEST: &str =
            "sha256:9a572bcefa75b6f2b5b4931d8604c1ad3f3e7560e0e0c2843646ec4f7853ef02";
        pub const SERVICE: &str = steward_connections_v1::SERVICE;
        pub const AGENT_TYPE: &str = steward_connections_v1::AGENT_TYPE;
        pub const BRIDGE_BINARY: &str = steward_connections_v1::BRIDGE_BINARY;
        pub const INPUT_FILE: &str = steward_connections_v1::INPUT_FILE;
        pub const OUTPUT_FILE: &str = steward_connections_v1::OUTPUT_FILE;
        pub const RESPONSE_DEADLINE_SECONDS: i64 =
            steward_connections_v1::RESPONSE_DEADLINE_SECONDS;
        pub const MCP_GW_VERSION: &str = "0.4.9";
        pub const OAUTH_STATE_LIFETIME_SECONDS: i64 =
            steward_connections_v1::OAUTH_STATE_LIFETIME_SECONDS;
        pub const OAUTH_CLOCK_SKEW_SECONDS: i64 = steward_connections_v1::OAUTH_CLOCK_SKEW_SECONDS;

        pub fn provider_control_grant(action: &str) -> Option<ToolGrant> {
            steward_connections_v1::provider_control_grant(action)
        }

        pub fn envelope() -> Envelope {
            let mut envelope = steward_connections_v1::envelope();
            envelope.revision = AUTHORITY_VERSION;
            envelope
        }
    }

    pub mod steward_connections_v3 {
        use super::{Envelope, steward_connections_v1, steward_connections_v2};
        use steward_types::ToolGrant;

        pub const AUTHORITY_ID: &str = steward_connections_v1::AUTHORITY_ID;
        pub const AUTHORITY_VERSION: i64 = 3;
        pub const AUTHORITY_DIGEST: &str =
            "sha256:d5878c6ae538174c5e0c32ac6aa4617f4ac8e6787b1495b08bd5af9e48f7fbe3";
        pub const SERVICE: &str = steward_connections_v1::SERVICE;
        pub const AGENT_TYPE: &str = steward_connections_v1::AGENT_TYPE;
        pub const BRIDGE_BINARY: &str = steward_connections_v1::BRIDGE_BINARY;
        pub const INPUT_FILE: &str = steward_connections_v1::INPUT_FILE;
        pub const OUTPUT_FILE: &str = steward_connections_v1::OUTPUT_FILE;
        pub const RESPONSE_DEADLINE_SECONDS: i64 =
            steward_connections_v1::RESPONSE_DEADLINE_SECONDS;
        pub const MCP_GW_VERSION: &str = steward_connections_v2::MCP_GW_VERSION;
        pub const OAUTH_STATE_LIFETIME_SECONDS: i64 =
            steward_connections_v1::OAUTH_STATE_LIFETIME_SECONDS;
        pub const OAUTH_CLOCK_SKEW_SECONDS: i64 = steward_connections_v1::OAUTH_CLOCK_SKEW_SECONDS;

        pub fn operation_grant(operation: &str) -> Option<ToolGrant> {
            if operation == "rerun" {
                Some(ToolGrant {
                    provider: "github".to_owned(),
                    resource: "actions_run_trigger".to_owned(),
                    action: "write".to_owned(),
                })
            } else {
                steward_connections_v1::provider_control_grant(operation)
            }
        }

        pub fn envelope() -> Envelope {
            let mut envelope = steward_connections_v2::envelope();
            envelope.revision = AUTHORITY_VERSION;
            envelope.spec.tools.push(ToolGrant {
                provider: "github".to_owned(),
                resource: "actions_run_trigger".to_owned(),
                action: "write".to_owned(),
            });
            envelope
        }
    }

    pub mod steward_connections_v4 {
        use super::{
            Envelope, steward_connections_v1, steward_connections_v2, steward_connections_v3,
        };
        use steward_types::ToolGrant;

        pub const AUTHORITY_ID: &str = steward_connections_v1::AUTHORITY_ID;
        pub const AUTHORITY_VERSION: i64 = 4;
        pub const AUTHORITY_DIGEST: &str =
            "sha256:55e4c02ca61f399b105ac87195913092753c0e398f7b3ef4241478e3ffa99945";
        pub const SERVICE: &str = steward_connections_v1::SERVICE;
        pub const AGENT_TYPE: &str = steward_connections_v1::AGENT_TYPE;
        pub const BRIDGE_BINARY: &str = steward_connections_v1::BRIDGE_BINARY;
        pub const INPUT_FILE: &str = steward_connections_v1::INPUT_FILE;
        pub const OUTPUT_FILE: &str = steward_connections_v1::OUTPUT_FILE;
        pub const RESPONSE_DEADLINE_SECONDS: i64 =
            steward_connections_v1::RESPONSE_DEADLINE_SECONDS;
        pub const MCP_GW_VERSION: &str = steward_connections_v2::MCP_GW_VERSION;
        pub const OAUTH_STATE_LIFETIME_SECONDS: i64 =
            steward_connections_v1::OAUTH_STATE_LIFETIME_SECONDS;
        pub const OAUTH_CLOCK_SKEW_SECONDS: i64 = steward_connections_v1::OAUTH_CLOCK_SKEW_SECONDS;

        fn github_grant(resource: &str, action: &str) -> ToolGrant {
            ToolGrant {
                provider: "github".to_owned(),
                resource: resource.to_owned(),
                action: action.to_owned(),
            }
        }

        pub fn operation_grants(operation: &str) -> Option<Vec<ToolGrant>> {
            match operation {
                "status" | "start" | "disconnect" | "rerun" => {
                    steward_connections_v3::operation_grant(operation).map(|grant| vec![grant])
                }
                "repositories" => Some(vec![
                    github_grant("search_repositories", "read"),
                    github_grant("get_me", "read"),
                ]),
                "workflow" => Some(vec![github_grant("get_file_contents", "read")]),
                "run-status" => Some(vec![
                    github_grant("actions_list", "read"),
                    github_grant("actions_get", "read"),
                    github_grant("get_job_logs", "read"),
                ]),
                "dispatch" => Some(vec![
                    github_grant("get_file_contents", "read"),
                    github_grant("actions_run_trigger", "write"),
                    github_grant("actions_list", "read"),
                ]),
                "publish" => Some(vec![
                    github_grant("get_commit", "read"),
                    github_grant("get_file_contents", "read"),
                    github_grant("list_pull_requests", "read"),
                    github_grant("create_branch", "write"),
                    github_grant("push_files", "write"),
                    github_grant("create_pull_request", "write"),
                ]),
                _ => None,
            }
        }

        pub fn envelope() -> Envelope {
            let mut envelope = steward_connections_v3::envelope();
            envelope.revision = AUTHORITY_VERSION;
            envelope.spec.tools.extend([
                github_grant("search_repositories", "read"),
                github_grant("get_me", "read"),
                github_grant("get_file_contents", "read"),
                github_grant("actions_list", "read"),
                github_grant("actions_get", "read"),
                github_grant("get_job_logs", "read"),
                github_grant("get_commit", "read"),
                github_grant("list_pull_requests", "read"),
                github_grant("create_branch", "write"),
                github_grant("push_files", "write"),
                github_grant("create_pull_request", "write"),
            ]);
            envelope
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvelopeScopeKind {
    MemberRole,
    Service,
}

impl EnvelopeScopeKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MemberRole => "member_role",
            Self::Service => "service",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Envelope {
    pub revision: i64,
    pub spec: EnvelopeSpec,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvelopeSpec {
    pub llms: Vec<ModelRef>,
    pub tools: Vec<ToolGrant>,
    pub budget: Budget,
    /// Cumulative execution allowance for one provisioned envelope instance.
    /// This is instance authority, not part of the AgentRuntime CRD manifest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_minutes_limit: Option<String>,
    pub ttl: Duration,
    #[serde(default)]
    pub runner: RunnerRequirements,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceAuthority>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "verdict")]
pub enum AdmissionDecision {
    Admit,
    Reject { deltas: Vec<AdmissionDelta> },
}

impl AdmissionDecision {
    pub fn counterexample(&self) -> Option<String> {
        let Self::Reject { deltas } = self else {
            return None;
        };
        let details = deltas
            .iter()
            .map(AdmissionDelta::counterexample)
            .collect::<Vec<_>>()
            .join("; ");
        Some(details)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "dimension")]
pub enum AdmissionDelta {
    Budget {
        requested: String,
        ceiling: String,
        currency: String,
    },
    SingleRunBudget {
        requested: Option<String>,
        ceiling: String,
        currency: String,
    },
    RuntimeMinutes {
        requested: Option<String>,
        ceiling: String,
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
    RunnerPlatforms {
        requested: Vec<RunnerPlatform>,
        ceiling: Vec<RunnerPlatform>,
    },
    RunnerMemory {
        requested: KubernetesQuantity,
        ceiling: Option<KubernetesQuantity>,
    },
    RunnerCompute {
        requested: KubernetesQuantity,
        ceiling: Option<KubernetesQuantity>,
    },
    RunnerStorage {
        requested: KubernetesQuantity,
        ceiling: Option<KubernetesQuantity>,
    },
    Workspace {
        requested: WorkspaceAuthority,
        ceiling: Option<WorkspaceAuthority>,
    },
}

impl AdmissionDelta {
    fn counterexample(&self) -> String {
        match self {
            Self::Budget {
                requested,
                ceiling,
                currency,
            } => format!(
                "Monthly budget {} is more than the {} this Envelope allows",
                format_money(requested, currency),
                format_money(ceiling, currency)
            ),
            Self::SingleRunBudget {
                requested,
                ceiling,
                currency,
            } => requested.as_deref().map_or_else(
                || {
                    format!(
                        "A per-run budget with no limit is more than the {} this Envelope allows",
                        format_money(ceiling, currency)
                    )
                },
                |requested| {
                    format!(
                        "Per-run budget {} is more than the {} this Envelope allows",
                        format_money(requested, currency),
                        format_money(ceiling, currency)
                    )
                },
            ),
            Self::RuntimeMinutes {
                requested: Some(requested),
                ceiling,
            } => {
                format!(
                    "Runtime allowance {requested} min is more than the {ceiling} min per month this Envelope allows"
                )
            }
            Self::RuntimeMinutes {
                requested: None,
                ceiling,
            } => format!(
                "A runtime allowance with no limit is more than the {ceiling} min per month this Envelope allows"
            ),
            Self::Ttl { requested, ceiling } => {
                format!(
                    "Maximum run duration {} is more than the {} this Envelope allows",
                    format_duration_label(requested),
                    format_duration_label(ceiling)
                )
            }
            Self::Models {
                requested,
                ceiling: _,
            } => outside_values_message(
                "Model",
                "Models",
                requested
                    .iter()
                    .map(|model| format!("{}/{}", model.provider, model.model))
                    .collect(),
                "models",
            ),
            Self::Tools {
                requested,
                ceiling: _,
            } => outside_values_message(
                "Tool",
                "Tools",
                requested
                    .iter()
                    .map(|tool| format!("{}:{}:{}", tool.provider, tool.resource, tool.action))
                    .collect(),
                "tools",
            ),
            Self::RunnerPlatforms {
                requested,
                ceiling: _,
            } => outside_values_message(
                "Runner platform",
                "Runner platforms",
                requested
                    .iter()
                    .map(|platform| format_runner_platforms(std::slice::from_ref(platform)))
                    .collect(),
                "platforms",
            ),
            Self::RunnerMemory { requested, ceiling } => {
                runner_resource_message("memory", requested, ceiling.as_ref())
            }
            Self::RunnerCompute { requested, ceiling } => {
                runner_resource_message("compute", requested, ceiling.as_ref())
            }
            Self::RunnerStorage { requested, ceiling } => {
                runner_resource_message("storage", requested, ceiling.as_ref())
            }
            Self::Workspace {
                requested,
                ceiling: Some(ceiling),
            } => format!(
                "Workspace access: requested {}; allowed {}",
                workspace_authority_summary(requested),
                workspace_authority_summary(ceiling)
            ),
            Self::Workspace {
                requested,
                ceiling: None,
            } => format!(
                "Workspace access: requested {}, but this Envelope does not allow workspace access",
                workspace_authority_summary(requested)
            ),
        }
    }
}

/// Formats a validated decimal amount for user-facing messages.
pub fn format_money(amount: &str, currency: &str) -> String {
    let Ok((integer, fractional)) = decimal_parts(amount) else {
        return format!("{amount} {currency}");
    };
    let integer = integer.trim_start_matches('0');
    let integer = if integer.is_empty() { "0" } else { integer };
    if integer == "0"
        && fractional.bytes().take(2).all(|digit| digit == b'0')
        && fractional.bytes().skip(2).any(|digit| digit != b'0')
    {
        return format!("< 0.01 {currency}");
    }

    let mut cents = fractional
        .bytes()
        .chain(std::iter::repeat(b'0'))
        .take(2)
        .fold(0_u8, |value, digit| value * 10 + digit - b'0');
    let round_up = fractional
        .as_bytes()
        .get(2)
        .is_some_and(|digit| *digit >= b'5');
    let mut integer = integer.to_owned();
    if round_up {
        cents += 1;
        if cents == 100 {
            cents = 0;
            increment_decimal_string(&mut integer);
        }
    }
    format!("{integer}.{cents:02} {currency}")
}

fn increment_decimal_string(value: &mut String) {
    let mut digits = value.as_bytes().to_vec();
    for digit in digits.iter_mut().rev() {
        if *digit < b'9' {
            *digit += 1;
            *value = digits.into_iter().map(char::from).collect();
            return;
        }
        *digit = b'0';
    }
    digits.insert(0, b'1');
    *value = digits.into_iter().map(char::from).collect();
}

fn format_duration_label(value: &str) -> String {
    let split = value.len().saturating_sub(1);
    let (amount, suffix) = value.split_at(split);
    match suffix {
        "s" => format!("{amount} s"),
        "m" => format!("{amount} min"),
        "h" => format!("{amount} h"),
        "d" => format!("{amount} d"),
        _ => value.to_owned(),
    }
}

fn outside_values_message(
    singular_label: &str,
    plural_label: &str,
    values: Vec<String>,
    allowed_noun: &str,
) -> String {
    if let [value] = values.as_slice() {
        format!("{singular_label} {value} is not one of the {allowed_noun} this Envelope allows")
    } else {
        format!(
            "{plural_label} {} are not among the {allowed_noun} this Envelope allows",
            values.join(", ")
        )
    }
}

fn format_runner_platforms(platforms: &[RunnerPlatform]) -> String {
    platforms
        .iter()
        .map(|platform| match platform {
            RunnerPlatform::Linux => "Linux",
            RunnerPlatform::Mac => "macOS",
            RunnerPlatform::Windows => "Windows",
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn runner_resource_message(
    resource: &str,
    requested: &KubernetesQuantity,
    ceiling: Option<&KubernetesQuantity>,
) -> String {
    let requested = format_binary_quantity(requested.0.as_str());
    match ceiling {
        Some(ceiling) => format!(
            "Runner {resource}: requested {requested}, limit {}",
            format_binary_quantity(ceiling.0.as_str())
        ),
        None => format!(
            "Runner {resource}: requested {requested}, but this Envelope does not allow a {resource} request"
        ),
    }
}

fn format_binary_quantity(value: &str) -> String {
    for (suffix, label) in [("Ti", "TiB"), ("Gi", "GiB"), ("Mi", "MiB"), ("Ki", "KiB")] {
        if let Some(number) = value.strip_suffix(suffix) {
            return format!("{number} {label}");
        }
    }
    value.to_owned()
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "error")]
pub enum AdmissionError {
    InvalidBudget { value: String },
    NonPositiveSingleRunLimit { value: String },
    InvalidCurrency { value: String },
    CurrencyMismatch { requested: String, ceiling: String },
    InvalidTtl { value: String },
    InvalidRunnerQuantity { resource: String, value: String },
    DuplicateRunnerPlatform { platform: RunnerPlatform },
    InvalidWorkspaceAuthority { reason: String },
    UnsupportedBindings,
}

pub fn validate_envelope(envelope: &Envelope) -> Result<(), AdmissionError> {
    validate_envelope_spec(&envelope.spec)
}

fn validate_envelope_spec(spec: &EnvelopeSpec) -> Result<(), AdmissionError> {
    Decimal::parse(&spec.budget.monthly_limit)?;
    if let Some(single_run_limit) = &spec.budget.single_run_limit {
        Decimal::parse(single_run_limit)?;
    }
    if let Some(runtime_minutes_limit) = &spec.runtime_minutes_limit {
        Decimal::parse(runtime_minutes_limit)?;
    }
    if spec.budget.currency.len() != 3
        || !spec
            .budget
            .currency
            .bytes()
            .all(|byte| byte.is_ascii_uppercase())
    {
        return Err(AdmissionError::InvalidCurrency {
            value: spec.budget.currency.clone(),
        });
    }
    duration_seconds(&spec.ttl)?;
    validate_runner(&spec.runner)?;
    if let Some(workspace) = &spec.workspace {
        validate_workspace_authority(workspace)?;
    }
    Ok(())
}

/// Validate an Envelope that a person can select for a run. Internal zero-budget authorities use
/// `validate_envelope`; a selectable per-run limit must leave positive spending authority.
pub fn validate_user_envelope(envelope: &Envelope) -> Result<(), AdmissionError> {
    validate_user_envelope_spec(&envelope.spec)
}

/// Validate authority a person selected for a Task before it is stored or started.
pub fn validate_user_envelope_spec(spec: &EnvelopeSpec) -> Result<(), AdmissionError> {
    validate_envelope_spec(spec)?;
    if let Some(single_run_limit) = &spec.budget.single_run_limit {
        parse_positive_single_run_limit(single_run_limit)?;
    }
    Ok(())
}

pub fn budget_is_exhausted(spend: &SpendSummary, budget: &Budget) -> Result<bool, AdmissionError> {
    if spend.currency != budget.currency {
        return Err(AdmissionError::CurrencyMismatch {
            requested: spend.currency.clone(),
            ceiling: budget.currency.clone(),
        });
    }
    let observed = Decimal::parse(&spend.observed_amount)?;
    let limit = Decimal::parse(&budget.monthly_limit)?;
    Ok(observed.cmp(&limit) != Ordering::Less)
}

pub fn evaluate(
    request: &AgentRuntimeSpec,
    envelope: &Envelope,
) -> Result<AdmissionDecision, AdmissionError> {
    validate_runner(&request.runner)?;
    if request.bindings.is_some() {
        return Err(AdmissionError::UnsupportedBindings);
    }
    evaluate_envelope_spec(
        &EnvelopeSpec {
            llms: request.llms.clone(),
            tools: request.tools.clone(),
            budget: request.budget.clone(),
            runtime_minutes_limit: envelope.spec.runtime_minutes_limit.clone(),
            ttl: request.ttl.clone(),
            runner: request.runner.clone(),
            workspace: None,
        },
        envelope,
    )
}

/// Compare an approved envelope snapshot with the immutable user request. Approval may only
/// narrow authority; a widening is returned as the same structured admission counterexample.
pub fn envelope_is_within(
    candidate: &Envelope,
    ceiling: &Envelope,
) -> Result<AdmissionDecision, AdmissionError> {
    validate_envelope(candidate)?;
    evaluate_envelope_spec(&candidate.spec, ceiling)
}

fn evaluate_envelope_spec(
    request: &EnvelopeSpec,
    envelope: &Envelope,
) -> Result<AdmissionDecision, AdmissionError> {
    validate_envelope(envelope)?;
    validate_runner(&request.runner)?;
    if request.budget.currency != envelope.spec.budget.currency {
        return Err(AdmissionError::CurrencyMismatch {
            requested: request.budget.currency.clone(),
            ceiling: envelope.spec.budget.currency.clone(),
        });
    }
    let mut deltas = Vec::new();
    let requested_budget = Decimal::parse(&request.budget.monthly_limit)?;
    let ceiling_budget = Decimal::parse(&envelope.spec.budget.monthly_limit)?;
    if requested_budget.cmp(&ceiling_budget) == Ordering::Greater {
        deltas.push(AdmissionDelta::Budget {
            requested: request.budget.monthly_limit.clone(),
            ceiling: envelope.spec.budget.monthly_limit.clone(),
            currency: request.budget.currency.clone(),
        });
    }
    let requested_single_run = request
        .budget
        .single_run_limit
        .as_deref()
        .map(Decimal::parse)
        .transpose()?;
    if let Some(ceiling_value) = envelope.spec.budget.single_run_limit.as_deref() {
        let ceiling_single_run = Decimal::parse(ceiling_value)?;
        if requested_single_run
            .as_ref()
            .is_none_or(|requested| requested.cmp(&ceiling_single_run) == Ordering::Greater)
        {
            deltas.push(AdmissionDelta::SingleRunBudget {
                requested: request.budget.single_run_limit.clone(),
                ceiling: ceiling_value.to_owned(),
                currency: request.budget.currency.clone(),
            });
        }
    }
    let requested_runtime_minutes = request
        .runtime_minutes_limit
        .as_deref()
        .map(Decimal::parse)
        .transpose()?;
    if let Some(ceiling_value) = envelope.spec.runtime_minutes_limit.as_deref() {
        let ceiling_runtime_minutes = Decimal::parse(ceiling_value)?;
        if requested_runtime_minutes
            .as_ref()
            .is_none_or(|requested| requested.cmp(&ceiling_runtime_minutes) == Ordering::Greater)
        {
            deltas.push(AdmissionDelta::RuntimeMinutes {
                requested: request.runtime_minutes_limit.clone(),
                ceiling: ceiling_value.to_owned(),
            });
        }
    }
    let requested_ttl = duration_seconds(&request.ttl)?;
    let ceiling_ttl = duration_seconds(&envelope.spec.ttl)?;
    if requested_ttl > ceiling_ttl {
        deltas.push(AdmissionDelta::Ttl {
            requested: request.ttl.0.clone(),
            ceiling: envelope.spec.ttl.0.clone(),
        });
    }
    let outside_models = request
        .llms
        .iter()
        .filter(|model| !envelope.spec.llms.contains(model))
        .cloned()
        .collect::<Vec<_>>();
    if !outside_models.is_empty() {
        deltas.push(AdmissionDelta::Models {
            requested: outside_models,
            ceiling: envelope.spec.llms.clone(),
        });
    }
    let outside_tools = request
        .tools
        .iter()
        .filter(|tool| !envelope.spec.tools.contains(tool))
        .cloned()
        .collect::<Vec<_>>();
    if !outside_tools.is_empty() {
        deltas.push(AdmissionDelta::Tools {
            requested: outside_tools,
            ceiling: envelope.spec.tools.clone(),
        });
    }
    let outside_platforms = request
        .runner
        .platforms
        .iter()
        .filter(|platform| !envelope.spec.runner.platforms.contains(platform))
        .copied()
        .collect::<Vec<_>>();
    if !outside_platforms.is_empty() {
        deltas.push(AdmissionDelta::RunnerPlatforms {
            requested: outside_platforms,
            ceiling: envelope.spec.runner.platforms.clone(),
        });
    }
    if let Some(requested) = request.runner.memory.clone()
        && runner_quantity_exceeds(
            Some(&requested),
            envelope.spec.runner.memory.as_ref(),
            RunnerResource::Memory,
        )?
    {
        deltas.push(AdmissionDelta::RunnerMemory {
            requested,
            ceiling: envelope.spec.runner.memory.clone(),
        });
    }
    if let Some(requested) = request.runner.compute.clone()
        && runner_quantity_exceeds(
            Some(&requested),
            envelope.spec.runner.compute.as_ref(),
            RunnerResource::Compute,
        )?
    {
        deltas.push(AdmissionDelta::RunnerCompute {
            requested,
            ceiling: envelope.spec.runner.compute.clone(),
        });
    }
    if let Some(requested) = request.runner.storage.clone()
        && runner_quantity_exceeds(
            Some(&requested),
            envelope.spec.runner.storage.as_ref(),
            RunnerResource::Storage,
        )?
    {
        deltas.push(AdmissionDelta::RunnerStorage {
            requested,
            ceiling: envelope.spec.runner.storage.clone(),
        });
    }
    if let Some(requested) = request.workspace.as_ref()
        && !workspace_authority_is_within(requested, envelope.spec.workspace.as_ref())?
    {
        deltas.push(AdmissionDelta::Workspace {
            requested: requested.clone(),
            ceiling: envelope.spec.workspace.clone(),
        });
    }
    if deltas.is_empty() {
        Ok(AdmissionDecision::Admit)
    } else {
        Ok(AdmissionDecision::Reject { deltas })
    }
}

/// Compare a Task's complete authority request with its effective Envelope ceiling.
/// Unlike [`evaluate`], this includes authority dimensions that are not represented on
/// `AgentRuntimeSpec`, such as the cumulative runtime-minutes limit and workspace authority.
pub fn evaluate_authority_request(
    request: &EnvelopeSpec,
    envelope: &Envelope,
) -> Result<AdmissionDecision, AdmissionError> {
    evaluate_envelope_spec(request, envelope)
}

fn workspace_authority_summary(authority: &WorkspaceAuthority) -> String {
    let types = authority
        .allowed_types
        .iter()
        .map(|entry_type| match entry_type {
            WorkspaceEntryType::Git => "git",
            WorkspaceEntryType::Scratch => "scratch",
        })
        .collect::<Vec<_>>();
    let types = match types.as_slice() {
        [] => "no workspace types".to_owned(),
        [only] => (*only).to_owned(),
        [first, second] => format!("{first} and {second}"),
        _ => types.join(", "),
    };
    format!(
        "{types}; total size {}; {} files; {} history generations; {} submodule levels",
        authority.max_total_size.as_ref().map_or_else(
            || "up to the deployment limit".to_owned(),
            |value| { format_binary_quantity(value.as_str()) }
        ),
        authority.max_files.map_or_else(
            || "up to the deployment limit".to_owned(),
            |value| format!("up to {value}")
        ),
        authority.max_history_depth.map_or_else(
            || "up to the deployment limit".to_owned(),
            |value| format!("up to {value}")
        ),
        authority.max_submodule_depth.map_or_else(
            || "up to the deployment limit".to_owned(),
            |value| format!("up to {value}")
        )
    )
}

fn validate_workspace_authority(authority: &WorkspaceAuthority) -> Result<(), AdmissionError> {
    if authority.allowed_types.is_empty() {
        return Err(AdmissionError::InvalidWorkspaceAuthority {
            reason: "allowedTypes must not be empty".to_owned(),
        });
    }
    for (index, entry_type) in authority.allowed_types.iter().enumerate() {
        if authority.allowed_types[..index].contains(entry_type) {
            return Err(AdmissionError::InvalidWorkspaceAuthority {
                reason: "allowedTypes contains a duplicate".to_owned(),
            });
        }
    }
    if authority.max_files == Some(0) {
        return Err(AdmissionError::InvalidWorkspaceAuthority {
            reason: "maxFiles must be positive".to_owned(),
        });
    }
    if authority.max_history_depth == Some(0) {
        return Err(AdmissionError::InvalidWorkspaceAuthority {
            reason: "maxHistoryDepth must be positive".to_owned(),
        });
    }
    if let Some(max_total_size) = authority.max_total_size.as_ref() {
        workspace_quantity(max_total_size.as_str())?;
    }
    Ok(())
}

fn workspace_authority_is_within(
    requested: &WorkspaceAuthority,
    ceiling: Option<&WorkspaceAuthority>,
) -> Result<bool, AdmissionError> {
    validate_workspace_authority(requested)?;
    let Some(ceiling) = ceiling else {
        return Ok(false);
    };
    validate_workspace_authority(ceiling)?;
    Ok(requested
        .allowed_types
        .iter()
        .all(|entry_type| ceiling.allowed_types.contains(entry_type))
        && optional_workspace_quantity_is_within(
            requested.max_total_size.as_ref(),
            ceiling.max_total_size.as_ref(),
        )?
        && optional_workspace_limit_is_within(requested.max_files, ceiling.max_files)
        && optional_workspace_limit_is_within(
            requested.max_history_depth,
            ceiling.max_history_depth,
        )
        && optional_workspace_limit_is_within(
            requested.max_submodule_depth,
            ceiling.max_submodule_depth,
        ))
}

fn optional_workspace_limit_is_within<T: Ord>(requested: Option<T>, ceiling: Option<T>) -> bool {
    match (requested, ceiling) {
        (_, None) => true,
        (Some(requested), Some(ceiling)) => requested <= ceiling,
        (None, Some(_)) => false,
    }
}

fn optional_workspace_quantity_is_within(
    requested: Option<&ResourceQuantity>,
    ceiling: Option<&ResourceQuantity>,
) -> Result<bool, AdmissionError> {
    match (requested, ceiling) {
        (_, None) => Ok(true),
        (Some(requested), Some(ceiling)) => {
            Ok(workspace_quantity(requested.as_str())? <= workspace_quantity(ceiling.as_str())?)
        }
        (None, Some(_)) => Ok(false),
    }
}

fn workspace_quantity(value: &str) -> Result<u128, AdmissionError> {
    runner_quantity(
        &KubernetesQuantity(value.to_owned()),
        RunnerResource::Storage,
    )
    .map_err(|_| AdmissionError::InvalidWorkspaceAuthority {
        reason: format!("maxTotalSize is invalid: {value}"),
    })
}

#[derive(Clone, Copy)]
enum RunnerResource {
    Memory,
    Compute,
    Storage,
}

impl RunnerResource {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::Compute => "compute",
            Self::Storage => "storage",
        }
    }
}

fn validate_runner(runner: &RunnerRequirements) -> Result<(), AdmissionError> {
    for (index, platform) in runner.platforms.iter().enumerate() {
        if runner.platforms[..index].contains(platform) {
            return Err(AdmissionError::DuplicateRunnerPlatform {
                platform: *platform,
            });
        }
    }
    for (value, resource) in [
        (runner.memory.as_ref(), RunnerResource::Memory),
        (runner.compute.as_ref(), RunnerResource::Compute),
        (runner.storage.as_ref(), RunnerResource::Storage),
    ] {
        if let Some(value) = value {
            runner_quantity(value, resource)?;
        }
    }
    Ok(())
}

fn runner_quantity_exceeds(
    requested: Option<&KubernetesQuantity>,
    ceiling: Option<&KubernetesQuantity>,
    resource: RunnerResource,
) -> Result<bool, AdmissionError> {
    let Some(requested) = requested else {
        return Ok(false);
    };
    let requested = runner_quantity(requested, resource)?;
    let Some(ceiling) = ceiling else {
        return Ok(true);
    };
    Ok(requested > runner_quantity(ceiling, resource)?)
}

fn runner_quantity(
    value: &KubernetesQuantity,
    resource: RunnerResource,
) -> Result<u128, AdmissionError> {
    let invalid = || AdmissionError::InvalidRunnerQuantity {
        resource: resource.as_str().to_owned(),
        value: value.0.clone(),
    };
    match resource {
        RunnerResource::Memory | RunnerResource::Storage => {
            let units = [("Ki", 1_u32), ("Mi", 2), ("Gi", 3), ("Ti", 4)];
            let Some((digits, power)) = units
                .iter()
                .find_map(|(unit, power)| value.0.strip_suffix(unit).map(|digits| (digits, power)))
            else {
                return Err(invalid());
            };
            let digits = digits.parse::<u128>().map_err(|_| invalid())?;
            if digits == 0 {
                return Err(invalid());
            }
            digits
                .checked_mul(1024_u128.pow(*power))
                .ok_or_else(invalid)
        }
        RunnerResource::Compute => {
            let millicores = if let Some(digits) = value.0.strip_suffix('m') {
                digits.parse::<u128>().map_err(|_| invalid())?
            } else {
                value
                    .0
                    .parse::<u128>()
                    .map_err(|_| invalid())?
                    .checked_mul(1000)
                    .ok_or_else(invalid)?
            };
            if millicores == 0 {
                Err(invalid())
            } else {
                Ok(millicores)
            }
        }
    }
}

pub fn evaluate_with_grants(
    request: &AgentRuntimeSpec,
    envelope: &Envelope,
    grants: &[AdmissionDelta],
) -> Result<AdmissionDecision, AdmissionError> {
    let decision = evaluate(request, envelope)?;
    let AdmissionDecision::Reject { deltas } = decision else {
        return Ok(AdmissionDecision::Admit);
    };
    let mut uncovered = Vec::new();
    for delta in deltas {
        if let Some(delta) = uncovered_delta(delta, grants)? {
            uncovered.push(delta);
        }
    }
    if uncovered.is_empty() {
        Ok(AdmissionDecision::Admit)
    } else {
        Ok(AdmissionDecision::Reject { deltas: uncovered })
    }
}

fn uncovered_delta(
    delta: AdmissionDelta,
    grants: &[AdmissionDelta],
) -> Result<Option<AdmissionDelta>, AdmissionError> {
    match delta {
        AdmissionDelta::Models { requested, ceiling } => {
            let requested = requested
                .into_iter()
                .filter(|model| {
                    !grants.iter().any(|grant| {
                        matches!(
                            grant,
                            AdmissionDelta::Models { requested, .. }
                                if requested.contains(model)
                        )
                    })
                })
                .collect::<Vec<_>>();
            Ok((!requested.is_empty()).then_some(AdmissionDelta::Models { requested, ceiling }))
        }
        AdmissionDelta::Tools { requested, ceiling } => {
            let requested = requested
                .into_iter()
                .filter(|tool| {
                    !grants.iter().any(|grant| {
                        matches!(
                            grant,
                            AdmissionDelta::Tools { requested, .. }
                                if requested.contains(tool)
                        )
                    })
                })
                .collect::<Vec<_>>();
            Ok((!requested.is_empty()).then_some(AdmissionDelta::Tools { requested, ceiling }))
        }
        delta => {
            for grant in grants {
                if grant_covers(grant, &delta)? {
                    return Ok(None);
                }
            }
            Ok(Some(delta))
        }
    }
}

pub fn add_budget_amount(left: &str, right: &str) -> Result<String, AdmissionError> {
    let (left_integer, left_fractional) = decimal_parts(left)?;
    let (right_integer, right_fractional) = decimal_parts(right)?;
    let mut left_digits = decimal_digits(left_integer, left_fractional);
    let mut right_digits = decimal_digits(right_integer, right_fractional);
    let scale = left_fractional.len().max(right_fractional.len());
    left_digits.extend(std::iter::repeat_n(0, scale - left_fractional.len()));
    right_digits.extend(std::iter::repeat_n(0, scale - right_fractional.len()));
    let width = left_digits.len().max(right_digits.len());
    left_digits.splice(0..0, std::iter::repeat_n(0, width - left_digits.len()));
    right_digits.splice(0..0, std::iter::repeat_n(0, width - right_digits.len()));

    let mut carry = 0;
    let mut sum = Vec::with_capacity(width + 1);
    for (left, right) in left_digits.into_iter().zip(right_digits).rev() {
        let value = left + right + carry;
        sum.push(value % 10);
        carry = value / 10;
    }
    if carry > 0 {
        sum.push(carry);
    }
    sum.reverse();
    let first_nonzero = sum
        .iter()
        .position(|digit| *digit != 0)
        .unwrap_or(sum.len().saturating_sub(scale + 1));
    let mut text = sum[first_nonzero..]
        .iter()
        .map(|digit| char::from(b'0' + *digit))
        .collect::<String>();
    if scale > 0 {
        if text.len() <= scale {
            text.insert_str(0, &"0".repeat(scale + 1 - text.len()));
        }
        text.insert(text.len() - scale, '.');
    }
    Ok(text)
}

#[derive(Debug)]
struct Decimal<'a> {
    integer: &'a str,
    fractional: &'a str,
}

impl<'a> Decimal<'a> {
    fn parse(value: &'a str) -> Result<Self, AdmissionError> {
        let (integer, fractional) = decimal_parts(value)?;
        Ok(Self {
            integer: integer.trim_start_matches('0'),
            fractional: fractional.trim_end_matches('0'),
        })
    }

    fn cmp(&self, other: &Self) -> Ordering {
        self.integer
            .len()
            .cmp(&other.integer.len())
            .then_with(|| self.integer.cmp(other.integer))
            .then_with(|| compare_fractional(self.fractional, other.fractional))
    }

    fn is_zero(&self) -> bool {
        self.integer.is_empty() && self.fractional.is_empty()
    }
}

fn parse_positive_single_run_limit(value: &str) -> Result<Decimal<'_>, AdmissionError> {
    let amount = Decimal::parse(value)?;
    if amount.is_zero() {
        return Err(AdmissionError::NonPositiveSingleRunLimit {
            value: value.to_owned(),
        });
    }
    Ok(amount)
}

fn decimal_parts(value: &str) -> Result<(&str, &str), AdmissionError> {
    let mut parts = value.split('.');
    let integer = parts.next().unwrap_or_default();
    let fractional = parts.next().unwrap_or_default();
    if integer.is_empty()
        || parts.next().is_some()
        || !integer.bytes().all(|byte| byte.is_ascii_digit())
        || !fractional.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(AdmissionError::InvalidBudget {
            value: value.to_owned(),
        });
    }
    Ok((integer, fractional))
}

fn decimal_digits(integer: &str, fractional: &str) -> Vec<u8> {
    integer
        .bytes()
        .chain(fractional.bytes())
        .map(|byte| byte - b'0')
        .collect()
}

fn compare_fractional(left: &str, right: &str) -> Ordering {
    let width = left.len().max(right.len());
    left.bytes()
        .chain(std::iter::repeat(b'0'))
        .zip(right.bytes().chain(std::iter::repeat(b'0')))
        .take(width)
        .find_map(|(left, right)| (left != right).then(|| left.cmp(&right)))
        .unwrap_or(Ordering::Equal)
}

pub fn duration_seconds(duration: &Duration) -> Result<u64, AdmissionError> {
    let value = duration.0.as_str();
    let split = value
        .find(|character: char| !character.is_ascii_digit())
        .ok_or_else(|| AdmissionError::InvalidTtl {
            value: value.to_owned(),
        })?;
    let (number, unit) = value.split_at(split);
    if number.is_empty() {
        return Err(AdmissionError::InvalidTtl {
            value: value.to_owned(),
        });
    }
    let amount = number
        .parse::<u64>()
        .map_err(|_| AdmissionError::InvalidTtl {
            value: value.to_owned(),
        })?;
    let multiplier = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        _ => {
            return Err(AdmissionError::InvalidTtl {
                value: value.to_owned(),
            });
        }
    };
    amount
        .checked_mul(multiplier)
        .ok_or_else(|| AdmissionError::InvalidTtl {
            value: value.to_owned(),
        })
}

fn grant_covers(
    grant: &AdmissionDelta,
    requested: &AdmissionDelta,
) -> Result<bool, AdmissionError> {
    match (grant, requested) {
        (
            AdmissionDelta::Budget {
                requested: granted,
                currency: grant_currency,
                ..
            },
            AdmissionDelta::Budget {
                requested,
                currency,
                ..
            },
        ) => Ok(grant_currency == currency
            && Decimal::parse(requested)?.cmp(&Decimal::parse(granted)?) != Ordering::Greater),
        (
            AdmissionDelta::SingleRunBudget {
                requested: granted,
                currency: grant_currency,
                ..
            },
            AdmissionDelta::SingleRunBudget {
                requested,
                currency,
                ..
            },
        ) => {
            if grant_currency != currency {
                return Ok(false);
            }
            match (granted.as_deref(), requested.as_deref()) {
                (None, _) => Ok(true),
                (Some(_), None) => Ok(false),
                (Some(granted), Some(requested)) => Ok(Decimal::parse(requested)?
                    .cmp(&Decimal::parse(granted)?)
                    != Ordering::Greater),
            }
        }
        (
            AdmissionDelta::RuntimeMinutes {
                requested: granted, ..
            },
            AdmissionDelta::RuntimeMinutes { requested, .. },
        ) => match (granted.as_deref(), requested.as_deref()) {
            (None, _) => Ok(true),
            (Some(_), None) => Ok(false),
            (Some(granted), Some(requested)) => {
                Ok(Decimal::parse(requested)?.cmp(&Decimal::parse(granted)?) != Ordering::Greater)
            }
        },
        (
            AdmissionDelta::Ttl {
                requested: granted, ..
            },
            AdmissionDelta::Ttl { requested, .. },
        ) => Ok(duration_seconds(&Duration(requested.clone()))?
            <= duration_seconds(&Duration(granted.clone()))?),
        (
            AdmissionDelta::Models {
                requested: granted, ..
            },
            AdmissionDelta::Models { requested, .. },
        ) => Ok(requested.iter().all(|model| granted.contains(model))),
        (
            AdmissionDelta::Tools {
                requested: granted, ..
            },
            AdmissionDelta::Tools { requested, .. },
        ) => Ok(requested.iter().all(|tool| granted.contains(tool))),
        (
            AdmissionDelta::RunnerPlatforms {
                requested: granted, ..
            },
            AdmissionDelta::RunnerPlatforms { requested, .. },
        ) => Ok(requested.iter().all(|platform| granted.contains(platform))),
        (
            AdmissionDelta::RunnerMemory {
                requested: granted, ..
            },
            AdmissionDelta::RunnerMemory { requested, .. },
        ) => runner_quantity(requested, RunnerResource::Memory).and_then(|requested| {
            runner_quantity(granted, RunnerResource::Memory).map(|granted| requested <= granted)
        }),
        (
            AdmissionDelta::RunnerCompute {
                requested: granted, ..
            },
            AdmissionDelta::RunnerCompute { requested, .. },
        ) => runner_quantity(requested, RunnerResource::Compute).and_then(|requested| {
            runner_quantity(granted, RunnerResource::Compute).map(|granted| requested <= granted)
        }),
        (
            AdmissionDelta::RunnerStorage {
                requested: granted, ..
            },
            AdmissionDelta::RunnerStorage { requested, .. },
        ) => runner_quantity(requested, RunnerResource::Storage).and_then(|requested| {
            runner_quantity(granted, RunnerResource::Storage).map(|granted| requested <= granted)
        }),
        (
            AdmissionDelta::Workspace {
                requested: granted, ..
            },
            AdmissionDelta::Workspace { requested, .. },
        ) => workspace_authority_is_within(requested, Some(granted)),
        _ => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use steward_types::direct_package::{ResourceQuantity, WorkspaceAuthority, WorkspaceEntryType};
    use steward_types::{
        AgentRuntimeSpec, AgentType, BindingRef, Budget, Duration, Email, KubernetesQuantity,
        ModelRef, Principal, RunnerPlatform, RunnerRequirements, ToolGrant,
    };

    use super::{
        AdmissionDecision, AdmissionDelta, AdmissionError, Envelope, EnvelopeSpec,
        add_budget_amount, envelope_is_within, evaluate, evaluate_authority_request,
        evaluate_with_grants, validate_envelope, workspace_authority_is_within,
    };

    fn assert_request_relation(
        label: &str,
        below: EnvelopeSpec,
        equal: EnvelopeSpec,
        above: EnvelopeSpec,
        ceiling: &Envelope,
    ) -> Result<(), String> {
        assert_eq!(
            evaluate_authority_request(&below, ceiling)
                .map_err(|error| format!("evaluate {label} below ceiling: {error:?}"))?,
            AdmissionDecision::Admit,
            "{label} below the ceiling must be admitted"
        );
        assert_eq!(
            evaluate_authority_request(&equal, ceiling)
                .map_err(|error| format!("evaluate {label} at ceiling: {error:?}"))?,
            AdmissionDecision::Admit,
            "{label} at the ceiling must be admitted"
        );
        assert!(
            matches!(
                evaluate_authority_request(&above, ceiling),
                Ok(AdmissionDecision::Reject { .. })
            ),
            "{label} above the ceiling must be rejected"
        );
        Ok(())
    }

    #[test]
    fn malformed_envelopes_are_rejected_before_they_become_authority() {
        for (budget, currency, ttl) in [
            ("not-a-decimal", "USD", "1h"),
            ("100.00", "", "1h"),
            ("100.00", "USD", "forever"),
        ] {
            let envelope = Envelope {
                revision: 1,
                spec: EnvelopeSpec {
                    llms: Vec::new(),
                    tools: Vec::new(),
                    budget: Budget {
                        monthly_limit: budget.to_owned(),
                        single_run_limit: None,
                        currency: currency.to_owned(),
                    },
                    runtime_minutes_limit: None,
                    ttl: Duration(ttl.to_owned()),
                    runner: RunnerRequirements::default(),
                    workspace: None,
                },
            };
            assert!(
                validate_envelope(&envelope).is_err(),
                "malformed envelope must fail closed: {envelope:?}"
            );
        }
    }

    fn request_with_budget(monthly_limit: &str) -> AgentRuntimeSpec {
        AgentRuntimeSpec {
            principal: Principal::User {
                acting_user: Email("alice@example.com".to_owned()),
            },
            owner: Email("alice@example.com".to_owned()),
            canonical_authority: None,
            agent_type: AgentType {
                name: "base".to_owned(),
            },
            llms: vec![ModelRef {
                provider: "provider-a".to_owned(),
                model: "model-a".to_owned(),
            }],
            tools: Vec::new(),
            budget: Budget {
                monthly_limit: monthly_limit.to_owned(),
                single_run_limit: None,
                currency: "USD".to_owned(),
            },
            ttl: Duration("24h".to_owned()),
            runner: RunnerRequirements::default(),
            bindings: None,
        }
    }

    fn envelope_with_budget(monthly_limit: &str) -> Envelope {
        Envelope {
            revision: 7,
            spec: EnvelopeSpec {
                llms: vec![ModelRef {
                    provider: "provider-a".to_owned(),
                    model: "model-a".to_owned(),
                }],
                tools: Vec::new(),
                budget: Budget {
                    monthly_limit: monthly_limit.to_owned(),
                    single_run_limit: None,
                    currency: "USD".to_owned(),
                },
                runtime_minutes_limit: None,
                ttl: Duration("24h".to_owned()),
                runner: RunnerRequirements::default(),
                workspace: None,
            },
        }
    }

    fn workspace_authority(
        allowed_types: Vec<WorkspaceEntryType>,
        max_total_size: &str,
        max_files: u64,
        max_history_depth: u32,
        max_submodule_depth: u32,
    ) -> Result<WorkspaceAuthority, String> {
        Ok(WorkspaceAuthority {
            allowed_types,
            max_total_size: Some(ResourceQuantity::parse(max_total_size)?),
            max_files: Some(max_files),
            max_history_depth: Some(max_history_depth),
            max_submodule_depth: Some(max_submodule_depth),
        })
    }

    #[test]
    fn envelope_workspace_authority_cannot_widen_any_cap() -> Result<(), String> {
        let mut ceiling = envelope_with_budget("100.00");
        ceiling.spec.workspace = Some(workspace_authority(
            vec![WorkspaceEntryType::Git],
            "1Gi",
            1_000,
            100,
            1,
        )?);
        let mut candidate = ceiling.clone();
        candidate.spec.workspace = Some(workspace_authority(
            vec![WorkspaceEntryType::Git, WorkspaceEntryType::Scratch],
            "2Gi",
            2_000,
            200,
            2,
        )?);

        let decision = envelope_is_within(&candidate, &ceiling)
            .map_err(|error| format!("workspace authority comparison failed: {error:?}"))?;
        assert!(matches!(
            decision,
            AdmissionDecision::Reject { deltas }
                if matches!(deltas.as_slice(), [AdmissionDelta::Workspace { .. }])
        ));
        Ok(())
    }

    #[test]
    fn workspace_authority_optional_limits_preserve_subset_semantics() -> Result<(), String> {
        let deployment_limited = WorkspaceAuthority {
            allowed_types: vec![WorkspaceEntryType::Git, WorkspaceEntryType::Scratch],
            max_total_size: None,
            max_files: None,
            max_history_depth: None,
            max_submodule_depth: None,
        };
        let narrowed = WorkspaceAuthority {
            allowed_types: vec![WorkspaceEntryType::Git],
            max_total_size: Some(ResourceQuantity::parse("2Gi")?),
            max_files: Some(100),
            max_history_depth: Some(20),
            max_submodule_depth: Some(2),
        };
        assert!(
            workspace_authority_is_within(&narrowed, Some(&deployment_limited))
                .map_err(|error| format!("compare deployment-limited authority: {error:?}"))?
        );

        for ceiling in [
            WorkspaceAuthority {
                allowed_types: vec![WorkspaceEntryType::Git],
                max_total_size: Some(ResourceQuantity::parse("2Gi")?),
                max_files: None,
                max_history_depth: None,
                max_submodule_depth: None,
            },
            WorkspaceAuthority {
                allowed_types: vec![WorkspaceEntryType::Git],
                max_total_size: None,
                max_files: Some(100),
                max_history_depth: None,
                max_submodule_depth: None,
            },
        ] {
            let requested = WorkspaceAuthority {
                allowed_types: vec![WorkspaceEntryType::Git],
                max_total_size: None,
                max_files: None,
                max_history_depth: None,
                max_submodule_depth: None,
            };
            assert!(
                !workspace_authority_is_within(&requested, Some(&ceiling))
                    .map_err(|error| format!("compare legacy-narrowed authority: {error:?}"))?,
                "omitting a limit must not widen a stricter parent Envelope"
            );
        }
        Ok(())
    }

    #[test]
    fn every_task_authority_dimension_uses_request_at_or_below_ceiling() -> Result<(), String> {
        let first_model = ModelRef {
            provider: "provider-a".to_owned(),
            model: "model-a".to_owned(),
        };
        let second_model = ModelRef {
            provider: "provider-b".to_owned(),
            model: "model-b".to_owned(),
        };
        let outside_model = ModelRef {
            provider: "provider-c".to_owned(),
            model: "model-c".to_owned(),
        };
        let first_tool = ToolGrant {
            provider: "tool-a".to_owned(),
            resource: "issues".to_owned(),
            action: "read".to_owned(),
        };
        let second_tool = ToolGrant {
            provider: "tool-b".to_owned(),
            resource: "pull-requests".to_owned(),
            action: "read".to_owned(),
        };
        let outside_tool = ToolGrant {
            provider: "tool-c".to_owned(),
            resource: "releases".to_owned(),
            action: "write".to_owned(),
        };
        let ceiling = Envelope {
            revision: 1,
            spec: EnvelopeSpec {
                llms: vec![first_model.clone(), second_model.clone()],
                tools: vec![first_tool.clone(), second_tool.clone()],
                budget: budget_with_single_run_limit("100.00", "10.00"),
                runtime_minutes_limit: Some("60".to_owned()),
                ttl: Duration("2h".to_owned()),
                runner: RunnerRequirements {
                    platforms: vec![RunnerPlatform::Linux, RunnerPlatform::Mac],
                    memory: Some(KubernetesQuantity("4Gi".to_owned())),
                    compute: Some(KubernetesQuantity("2000m".to_owned())),
                    storage: Some(KubernetesQuantity("20Gi".to_owned())),
                },
                workspace: Some(workspace_authority(
                    vec![WorkspaceEntryType::Git, WorkspaceEntryType::Scratch],
                    "4Gi",
                    100,
                    20,
                    2,
                )?),
            },
        };

        let mut below = ceiling.spec.clone();
        below.llms = vec![first_model];
        let equal = ceiling.spec.clone();
        let mut above = equal.clone();
        above.llms.push(outside_model);
        assert_request_relation("models", below, equal, above, &ceiling)?;

        let mut below = ceiling.spec.clone();
        below.tools = vec![first_tool];
        let equal = ceiling.spec.clone();
        let mut above = equal.clone();
        above.tools.push(outside_tool);
        assert_request_relation("tools", below, equal, above, &ceiling)?;

        for (label, below_value, equal_value, above_value, apply) in [
            ("monthly budget", "50.00", "100.00", "101.00", 0_u8),
            ("single-run budget", "5.00", "10.00", "11.00", 1_u8),
            ("runtime minutes", "30", "60", "61", 2_u8),
        ] {
            let mut below = ceiling.spec.clone();
            let mut equal = ceiling.spec.clone();
            let mut above = ceiling.spec.clone();
            match apply {
                0 => {
                    below.budget.monthly_limit = below_value.to_owned();
                    equal.budget.monthly_limit = equal_value.to_owned();
                    above.budget.monthly_limit = above_value.to_owned();
                }
                1 => {
                    below.budget.single_run_limit = Some(below_value.to_owned());
                    equal.budget.single_run_limit = Some(equal_value.to_owned());
                    above.budget.single_run_limit = Some(above_value.to_owned());
                }
                _ => {
                    below.runtime_minutes_limit = Some(below_value.to_owned());
                    equal.runtime_minutes_limit = Some(equal_value.to_owned());
                    above.runtime_minutes_limit = Some(above_value.to_owned());
                }
            }
            assert_request_relation(label, below, equal, above, &ceiling)?;
        }

        let mut below = ceiling.spec.clone();
        below.ttl = Duration("1h".to_owned());
        let equal = ceiling.spec.clone();
        let mut above = equal.clone();
        above.ttl = Duration("3h".to_owned());
        assert_request_relation("TTL", below, equal, above, &ceiling)?;

        let mut below = ceiling.spec.clone();
        below.runner.platforms = vec![RunnerPlatform::Linux];
        let equal = ceiling.spec.clone();
        let mut above = equal.clone();
        above.runner.platforms.push(RunnerPlatform::Windows);
        assert_request_relation("runner platforms", below, equal, above, &ceiling)?;

        for (label, below_value, equal_value, above_value, resource) in [
            ("runner memory", "2Gi", "4Gi", "5Gi", 0_u8),
            ("runner compute", "1000m", "2000m", "2001m", 1_u8),
            ("runner storage", "10Gi", "20Gi", "21Gi", 2_u8),
        ] {
            let mut below = ceiling.spec.clone();
            let mut equal = ceiling.spec.clone();
            let mut above = ceiling.spec.clone();
            let below_value = Some(KubernetesQuantity(below_value.to_owned()));
            let equal_value = Some(KubernetesQuantity(equal_value.to_owned()));
            let above_value = Some(KubernetesQuantity(above_value.to_owned()));
            match resource {
                0 => {
                    below.runner.memory = below_value;
                    equal.runner.memory = equal_value;
                    above.runner.memory = above_value;
                }
                1 => {
                    below.runner.compute = below_value;
                    equal.runner.compute = equal_value;
                    above.runner.compute = above_value;
                }
                _ => {
                    below.runner.storage = below_value;
                    equal.runner.storage = equal_value;
                    above.runner.storage = above_value;
                }
            }
            assert_request_relation(label, below, equal, above, &ceiling)?;
        }

        let mut type_ceiling = ceiling.clone();
        type_ceiling.spec.workspace = Some(workspace_authority(
            vec![WorkspaceEntryType::Git],
            "4Gi",
            100,
            20,
            2,
        )?);
        let mut no_workspace = type_ceiling.spec.clone();
        no_workspace.workspace = None;
        let equal_type = type_ceiling.spec.clone();
        let mut outside_type = equal_type.clone();
        outside_type.workspace = Some(workspace_authority(
            vec![WorkspaceEntryType::Git, WorkspaceEntryType::Scratch],
            "4Gi",
            100,
            20,
            2,
        )?);
        assert_request_relation(
            "workspace entry types",
            no_workspace,
            equal_type,
            outside_type,
            &type_ceiling,
        )?;

        let mut below_workspace = ceiling.spec.clone();
        below_workspace.workspace = Some(workspace_authority(
            vec![WorkspaceEntryType::Git],
            "2Gi",
            50,
            10,
            1,
        )?);
        assert_eq!(
            evaluate_authority_request(&below_workspace, &ceiling)
                .map_err(|error| format!("evaluate workspace below ceiling: {error:?}"))?,
            AdmissionDecision::Admit
        );
        assert_eq!(
            evaluate_authority_request(&ceiling.spec, &ceiling)
                .map_err(|error| format!("evaluate workspace at ceiling: {error:?}"))?,
            AdmissionDecision::Admit
        );
        for dimension in ["size", "files", "history depth", "submodule depth"] {
            let mut above = ceiling.spec.clone();
            let workspace = above
                .workspace
                .as_mut()
                .ok_or_else(|| "workspace ceiling must exist".to_owned())?;
            match dimension {
                "size" => workspace.max_total_size = Some(ResourceQuantity::parse("5Gi")?),
                "files" => workspace.max_files = Some(101),
                "history depth" => workspace.max_history_depth = Some(21),
                _ => workspace.max_submodule_depth = Some(3),
            }
            assert!(
                matches!(
                    evaluate_authority_request(&above, &ceiling),
                    Ok(AdmissionDecision::Reject { .. })
                ),
                "workspace {dimension} above the ceiling must be rejected"
            );
        }

        Ok(())
    }

    fn budget_with_single_run_limit(monthly_limit: &str, single_run_limit: &str) -> Budget {
        Budget {
            monthly_limit: monthly_limit.to_owned(),
            single_run_limit: Some(single_run_limit.to_owned()),
            currency: "USD".to_owned(),
        }
    }

    fn envelope_with_single_run_limit(monthly_limit: &str, single_run_limit: &str) -> Envelope {
        let mut envelope = envelope_with_budget(monthly_limit);
        envelope.spec.budget = budget_with_single_run_limit(monthly_limit, single_run_limit);
        envelope
    }

    fn request_with_single_run_limit(
        monthly_limit: &str,
        single_run_limit: &str,
    ) -> AgentRuntimeSpec {
        let mut request = request_with_budget(monthly_limit);
        request.budget = budget_with_single_run_limit(monthly_limit, single_run_limit);
        request
    }

    #[test]
    fn malformed_single_run_limit_is_rejected_before_it_becomes_authority() {
        let envelope = envelope_with_single_run_limit("100.00", "not-a-decimal");

        assert_eq!(
            super::validate_user_envelope(&envelope),
            Err(AdmissionError::InvalidBudget {
                value: "not-a-decimal".to_owned(),
            })
        );
    }

    #[test]
    fn zero_single_run_limit_is_rejected_before_it_becomes_authority() {
        let envelope = envelope_with_single_run_limit("100.00", "0");

        assert_eq!(
            super::validate_user_envelope(&envelope),
            Err(AdmissionError::NonPositiveSingleRunLimit {
                value: "0".to_owned(),
            })
        );
    }

    #[test]
    fn request_cannot_exceed_the_template_single_run_limit() -> Result<(), String> {
        let request = request_with_single_run_limit("100.00", "3.00");
        let envelope = envelope_with_single_run_limit("200.00", "2.00");

        assert_eq!(
            evaluate(&request, &envelope)
                .map_err(|error| format!("evaluate request: {error:?}"))?
                .counterexample()
                .as_deref(),
            Some("Per-run budget 3.00 USD is more than the 2.00 USD this Envelope allows")
        );
        Ok(())
    }

    #[test]
    fn constrained_template_rejects_an_unbounded_single_run_request() -> Result<(), String> {
        let request = request_with_budget("100.00");
        let envelope = envelope_with_single_run_limit("200.00", "2.00");

        assert_eq!(
            evaluate(&request, &envelope)
                .map_err(|error| format!("evaluate request: {error:?}"))?
                .counterexample()
                .as_deref(),
            Some("A per-run budget with no limit is more than the 2.00 USD this Envelope allows")
        );
        Ok(())
    }

    #[test]
    fn envelope_request_cannot_exceed_runtime_minutes_limit() -> Result<(), String> {
        let mut requested = envelope_with_budget("100.00");
        requested.spec.runtime_minutes_limit = Some("180".to_owned());
        let mut ceiling = envelope_with_budget("200.00");
        ceiling.spec.runtime_minutes_limit = Some("120".to_owned());

        assert_eq!(
            envelope_is_within(&requested, &ceiling)
                .map_err(|error| format!("evaluate envelope request: {error:?}"))?
                .counterexample()
                .as_deref(),
            Some(
                "Runtime allowance 180 min is more than the 120 min per month this Envelope allows"
            )
        );
        Ok(())
    }

    #[test]
    fn constrained_template_rejects_unbounded_runtime_minutes() -> Result<(), String> {
        let requested = envelope_with_budget("100.00");
        let mut ceiling = envelope_with_budget("200.00");
        ceiling.spec.runtime_minutes_limit = Some("120".to_owned());

        assert_eq!(
            envelope_is_within(&requested, &ceiling)
                .map_err(|error| format!("evaluate envelope request: {error:?}"))?
                .counterexample()
                .as_deref(),
            Some(
                "A runtime allowance with no limit is more than the 120 min per month this Envelope allows"
            )
        );
        Ok(())
    }

    #[test]
    fn single_run_grant_covers_only_its_explicit_budget_expansion() -> Result<(), String> {
        let request = request_with_single_run_limit("100.00", "3.00");
        let envelope = envelope_with_single_run_limit("200.00", "2.00");
        let narrow_grant = AdmissionDelta::SingleRunBudget {
            requested: Some("2.50".to_owned()),
            ceiling: "2.00".to_owned(),
            currency: "USD".to_owned(),
        };
        let grant = AdmissionDelta::SingleRunBudget {
            requested: Some("3.00".to_owned()),
            ceiling: "2.00".to_owned(),
            currency: "USD".to_owned(),
        };

        assert!(matches!(
            evaluate_with_grants(&request, &envelope, &[narrow_grant]),
            Ok(AdmissionDecision::Reject { .. })
        ));
        assert_eq!(
            evaluate_with_grants(&request, &envelope, &[grant])
                .map_err(|error| format!("evaluate granted request: {error:?}"))?,
            AdmissionDecision::Admit
        );
        Ok(())
    }

    #[test]
    fn runner_authority_rejects_platform_and_resource_expansion() {
        let mut request = request_with_budget("100.00");
        request.runner = RunnerRequirements {
            platforms: vec![RunnerPlatform::Windows],
            memory: Some(KubernetesQuantity("3Gi".to_owned())),
            compute: Some(KubernetesQuantity("1500m".to_owned())),
            storage: Some(KubernetesQuantity("11Gi".to_owned())),
        };
        let mut envelope = envelope_with_budget("200.00");
        envelope.spec.runner = RunnerRequirements {
            platforms: vec![RunnerPlatform::Linux],
            memory: Some(KubernetesQuantity("2Gi".to_owned())),
            compute: Some(KubernetesQuantity("1".to_owned())),
            storage: Some(KubernetesQuantity("10Gi".to_owned())),
        };

        assert!(matches!(
            evaluate(&request, &envelope),
            Ok(AdmissionDecision::Reject { deltas })
                if deltas.iter().any(|delta| matches!(delta, AdmissionDelta::RunnerPlatforms { .. }))
                    && deltas.iter().any(|delta| matches!(delta, AdmissionDelta::RunnerMemory { .. }))
                    && deltas.iter().any(|delta| matches!(delta, AdmissionDelta::RunnerCompute { .. }))
                    && deltas.iter().any(|delta| matches!(delta, AdmissionDelta::RunnerStorage { .. }))
        ));
    }

    #[test]
    fn approved_snapshot_cannot_widen_the_immutable_user_request() {
        let requested = envelope_with_budget("100.00");
        let mut approved = requested.clone();
        approved.spec.budget.monthly_limit = "101.00".to_owned();

        assert!(matches!(
            envelope_is_within(&approved, &requested),
            Ok(AdmissionDecision::Reject { deltas })
                if matches!(deltas.as_slice(), [AdmissionDelta::Budget { .. }])
        ));
    }

    #[test]
    fn rejects_the_absolute_value_after_individually_safe_edits() {
        let original = request_with_budget("100.00");
        let first_edit_if_isolated = request_with_budget("160.00");
        let second_edit_if_isolated = request_with_budget("160.00");
        let composed = request_with_budget("220.00");
        let envelope = envelope_with_budget("200.00");

        assert_eq!(evaluate(&original, &envelope), Ok(AdmissionDecision::Admit));
        assert_eq!(
            evaluate(&first_edit_if_isolated, &envelope),
            Ok(AdmissionDecision::Admit)
        );
        assert_eq!(
            evaluate(&second_edit_if_isolated, &envelope),
            Ok(AdmissionDecision::Admit)
        );
        assert_eq!(
            evaluate(&composed, &envelope),
            Ok(AdmissionDecision::Reject {
                deltas: vec![AdmissionDelta::Budget {
                    requested: "220.00".to_owned(),
                    ceiling: "200.00".to_owned(),
                    currency: "USD".to_owned(),
                }],
            }),
            "admission must compare the composed absolute budget, not either edit delta"
        );
    }

    #[test]
    fn rejection_reports_every_outside_dimension_in_stable_order() {
        let mut request = request_with_budget("201.00");
        request.llms.push(ModelRef {
            provider: "provider-b".to_owned(),
            model: "model-b".to_owned(),
        });
        request.tools.push(ToolGrant {
            provider: "tool-a".to_owned(),
            resource: "issues".to_owned(),
            action: "write".to_owned(),
        });
        request.ttl = Duration("25h".to_owned());
        let envelope = envelope_with_budget("200.00");

        assert_eq!(
            evaluate(&request, &envelope),
            Ok(AdmissionDecision::Reject {
                deltas: vec![
                    AdmissionDelta::Budget {
                        requested: "201.00".to_owned(),
                        ceiling: "200.00".to_owned(),
                        currency: "USD".to_owned(),
                    },
                    AdmissionDelta::Ttl {
                        requested: "25h".to_owned(),
                        ceiling: "24h".to_owned(),
                    },
                    AdmissionDelta::Models {
                        requested: vec![ModelRef {
                            provider: "provider-b".to_owned(),
                            model: "model-b".to_owned(),
                        }],
                        ceiling: envelope.spec.llms.clone(),
                    },
                    AdmissionDelta::Tools {
                        requested: vec![ToolGrant {
                            provider: "tool-a".to_owned(),
                            resource: "issues".to_owned(),
                            action: "write".to_owned(),
                        }],
                        ceiling: Vec::new(),
                    },
                ],
            }),
            "every outside absolute value must appear in the counterexample"
        );
    }

    #[test]
    fn reserved_plane_b_shape_fails_closed_but_service_uses_the_same_envelope_fence() {
        let envelope = envelope_with_budget("200.00");
        let mut service = request_with_budget("100.00");
        service.principal = Principal::Service {
            name: "service-a".to_owned(),
            acting_user: None,
        };
        assert_eq!(evaluate(&service, &envelope), Ok(AdmissionDecision::Admit));

        service.tools.push(ToolGrant {
            provider: "mcp".to_owned(),
            resource: "repository:outside-service-scope".to_owned(),
            action: "read".to_owned(),
        });
        assert!(
            matches!(
                evaluate(&service, &envelope),
                Ok(AdmissionDecision::Reject { deltas })
                    if matches!(deltas.as_slice(), [AdmissionDelta::Tools { .. }])
            ),
            "a service principal must not bypass the envelope anti-ratchet"
        );

        let mut bound = request_with_budget("100.00");
        bound.bindings = Some(vec![BindingRef("binding-a".to_owned())]);
        assert_eq!(
            evaluate(&bound, &envelope),
            Err(AdmissionError::UnsupportedBindings)
        );
    }

    #[test]
    fn provider_agnostic_tools_use_exact_request_at_or_below_ceiling() {
        let jira_read = ToolGrant {
            provider: "jira".to_owned(),
            resource: "jira_get_issue".to_owned(),
            action: "read".to_owned(),
        };
        let jira_write = ToolGrant {
            provider: "jira".to_owned(),
            resource: "edit_issue".to_owned(),
            action: "write".to_owned(),
        };
        let mut envelope = envelope_with_budget("200.00");
        envelope.spec.tools = vec![jira_read.clone()];

        let mut requested = request_with_budget("100.00");
        requested.tools = vec![jira_read.clone()];
        assert_eq!(
            evaluate(&requested, &envelope),
            Ok(AdmissionDecision::Admit),
            "an exact Jira read tuple at the ceiling must be admitted"
        );

        requested.tools.clear();
        assert_eq!(
            evaluate(&requested, &envelope),
            Ok(AdmissionDecision::Admit),
            "a Task may request fewer tools than its Envelope allows"
        );

        requested.tools = vec![jira_write.clone()];
        assert_eq!(
            evaluate(&requested, &envelope),
            Ok(AdmissionDecision::Reject {
                deltas: vec![AdmissionDelta::Tools {
                    requested: vec![jira_write],
                    ceiling: vec![jira_read],
                }],
            }),
            "a Jira write tuple absent from the ceiling must be rejected"
        );

        requested.tools = vec![ToolGrant {
            provider: "jira".to_owned(),
            resource: "search_issues".to_owned(),
            action: "read".to_owned(),
        }];
        envelope.spec.tools = vec![ToolGrant {
            provider: "jira".to_owned(),
            resource: "jira_search_issues".to_owned(),
            action: "read".to_owned(),
        }];
        assert!(
            matches!(
                evaluate(&requested, &envelope),
                Ok(AdmissionDecision::Reject { deltas })
                    if matches!(deltas.as_slice(), [AdmissionDelta::Tools { .. }])
            ),
            "a similarly named Jira resource must not receive the catalog tuple's authority"
        );
    }

    #[test]
    fn rejection_message_uses_plain_labels_and_units_for_both_front_doors() {
        let decision = AdmissionDecision::Reject {
            deltas: vec![
                AdmissionDelta::Budget {
                    requested: "220.00".to_owned(),
                    ceiling: "200.00".to_owned(),
                    currency: "USD".to_owned(),
                },
                AdmissionDelta::Ttl {
                    requested: "25h".to_owned(),
                    ceiling: "24h".to_owned(),
                },
            ],
        };

        assert_eq!(
            decision.counterexample(),
            Some(
                "Monthly budget 220.00 USD is more than the 200.00 USD this Envelope allows; Maximum run duration 25 h is more than the 24 h this Envelope allows"
                    .to_owned()
            )
        );
    }

    #[test]
    fn rejection_message_formats_money_to_two_decimal_places() {
        let decision = AdmissionDecision::Reject {
            deltas: vec![AdmissionDelta::Budget {
                requested: "25".to_owned(),
                ceiling: "20.0".to_owned(),
                currency: "USD".to_owned(),
            }],
        };

        assert_eq!(
            decision.counterexample(),
            Some(
                "Monthly budget 25.00 USD is more than the 20.00 USD this Envelope allows"
                    .to_owned()
            )
        );
    }

    #[test]
    fn rejection_message_does_not_expose_debug_platform_formatting() {
        let decision = AdmissionDecision::Reject {
            deltas: vec![AdmissionDelta::RunnerPlatforms {
                requested: vec![RunnerPlatform::Windows],
                ceiling: vec![RunnerPlatform::Linux],
            }],
        };

        assert_eq!(
            decision.counterexample(),
            Some(
                "Runner platform Windows is not one of the platforms this Envelope allows"
                    .to_owned()
            )
        );
    }

    #[test]
    fn rejection_message_names_only_the_tool_outside_the_envelope() {
        let decision = AdmissionDecision::Reject {
            deltas: vec![AdmissionDelta::Tools {
                requested: vec![ToolGrant {
                    provider: "github".to_owned(),
                    resource: "get_file_contents".to_owned(),
                    action: "read".to_owned(),
                }],
                ceiling: vec![ToolGrant {
                    provider: "github".to_owned(),
                    resource: "actions_list".to_owned(),
                    action: "read".to_owned(),
                }],
            }],
        };

        assert_eq!(
            decision.counterexample(),
            Some(
                "Tool github:get_file_contents:read is not one of the tools this Envelope allows"
                    .to_owned()
            )
        );
    }

    #[test]
    fn rejection_messages_explain_absent_runner_and_workspace_authority() -> Result<(), String> {
        let requested_workspace = WorkspaceAuthority {
            allowed_types: vec![WorkspaceEntryType::Git, WorkspaceEntryType::Scratch],
            max_total_size: Some(ResourceQuantity::parse("2Gi")?),
            max_files: Some(100),
            max_history_depth: Some(20),
            max_submodule_depth: Some(2),
        };
        let decision = AdmissionDecision::Reject {
            deltas: vec![
                AdmissionDelta::RunnerMemory {
                    requested: KubernetesQuantity("8Gi".to_owned()),
                    ceiling: None,
                },
                AdmissionDelta::Workspace {
                    requested: requested_workspace,
                    ceiling: None,
                },
            ],
        };

        assert_eq!(
            decision.counterexample(),
            Some(
                "Runner memory: requested 8 GiB, but this Envelope does not allow a memory request; Workspace access: requested git and scratch; total size 2 GiB; up to 100 files; up to 20 history generations; up to 2 submodule levels, but this Envelope does not allow workspace access"
                    .to_owned()
            )
        );
        Ok(())
    }

    #[test]
    fn budget_addition_uses_the_admission_decimal_syntax() {
        assert_eq!(add_budget_amount("099.90", "0.1"), Ok("100.00".to_owned()));
        assert_eq!(
            add_budget_amount("100.00", "+1"),
            Err(AdmissionError::InvalidBudget {
                value: "+1".to_owned(),
            })
        );
    }

    #[test]
    fn grant_covers_only_the_approved_absolute_value() {
        let envelope = envelope_with_budget("200.00");
        let grant = AdmissionDelta::Budget {
            requested: "220.00".to_owned(),
            ceiling: "200.00".to_owned(),
            currency: "USD".to_owned(),
        };

        assert_eq!(
            evaluate_with_grants(
                &request_with_budget("220.00"),
                &envelope,
                std::slice::from_ref(&grant),
            ),
            Ok(AdmissionDecision::Admit),
            "the exact approved budget must pass admission for the bound runtime"
        );
        assert_eq!(
            evaluate_with_grants(&request_with_budget("221.00"), &envelope, &[grant]),
            Ok(AdmissionDecision::Reject {
                deltas: vec![AdmissionDelta::Budget {
                    requested: "221.00".to_owned(),
                    ceiling: "200.00".to_owned(),
                    currency: "USD".to_owned(),
                }],
            }),
            "a grant must not authorize a value beyond the approved absolute ceiling"
        );
    }

    #[test]
    fn runner_grant_covers_only_the_approved_absolute_limit() {
        let mut requested = request_with_budget("100.00");
        requested.runner = RunnerRequirements {
            platforms: vec![RunnerPlatform::Windows],
            memory: Some(KubernetesQuantity("3Gi".to_owned())),
            compute: Some(KubernetesQuantity("1500m".to_owned())),
            storage: Some(KubernetesQuantity("11Gi".to_owned())),
        };
        let mut ceiling = envelope_with_budget("200.00");
        ceiling.spec.runner = RunnerRequirements {
            platforms: vec![RunnerPlatform::Linux],
            memory: Some(KubernetesQuantity("2Gi".to_owned())),
            compute: Some(KubernetesQuantity("1".to_owned())),
            storage: Some(KubernetesQuantity("10Gi".to_owned())),
        };
        let grants = vec![
            AdmissionDelta::RunnerPlatforms {
                requested: vec![RunnerPlatform::Windows],
                ceiling: vec![RunnerPlatform::Linux],
            },
            AdmissionDelta::RunnerMemory {
                requested: KubernetesQuantity("3Gi".to_owned()),
                ceiling: Some(KubernetesQuantity("2Gi".to_owned())),
            },
            AdmissionDelta::RunnerCompute {
                requested: KubernetesQuantity("1500m".to_owned()),
                ceiling: Some(KubernetesQuantity("1".to_owned())),
            },
            AdmissionDelta::RunnerStorage {
                requested: KubernetesQuantity("11Gi".to_owned()),
                ceiling: Some(KubernetesQuantity("10Gi".to_owned())),
            },
        ];

        assert_eq!(
            evaluate_with_grants(&requested, &ceiling, &grants),
            Ok(AdmissionDecision::Admit),
            "each runner exception must be scoped to the exact approved platform or resource value"
        );

        requested.runner.memory = Some(KubernetesQuantity("4Gi".to_owned()));
        assert!(matches!(
            evaluate_with_grants(&requested, &ceiling, &grants),
            Ok(AdmissionDecision::Reject { deltas })
                if matches!(deltas.as_slice(), [AdmissionDelta::RunnerMemory { .. }])
        ));
    }

    #[test]
    fn grants_cover_only_the_approved_values_in_each_dimension() {
        let envelope = envelope_with_budget("200.00");
        let extra_model = ModelRef {
            provider: "provider-b".to_owned(),
            model: "model-b".to_owned(),
        };
        let extra_tool = ToolGrant {
            provider: "tool-a".to_owned(),
            resource: "issues".to_owned(),
            action: "write".to_owned(),
        };
        let mut approved = request_with_budget("220.00");
        approved.ttl = Duration("25h".to_owned());
        approved.llms.push(extra_model.clone());
        approved.tools.push(extra_tool.clone());
        let grants = vec![
            AdmissionDelta::Budget {
                requested: "220.00".to_owned(),
                ceiling: "200.00".to_owned(),
                currency: "USD".to_owned(),
            },
            AdmissionDelta::Ttl {
                requested: "25h".to_owned(),
                ceiling: "24h".to_owned(),
            },
            AdmissionDelta::Models {
                requested: vec![extra_model],
                ceiling: envelope.spec.llms.clone(),
            },
            AdmissionDelta::Tools {
                requested: vec![extra_tool],
                ceiling: Vec::new(),
            },
        ];
        assert_eq!(
            evaluate_with_grants(&approved, &envelope, &grants),
            Ok(AdmissionDecision::Admit),
            "every approved exception dimension must be recognized together"
        );

        let mut outside = approved;
        outside.ttl = Duration("26h".to_owned());
        outside.llms.push(ModelRef {
            provider: "provider-c".to_owned(),
            model: "model-c".to_owned(),
        });
        outside.tools.push(ToolGrant {
            provider: "tool-b".to_owned(),
            resource: "deployments".to_owned(),
            action: "write".to_owned(),
        });
        assert_eq!(
            evaluate_with_grants(&outside, &envelope, &grants),
            Ok(AdmissionDecision::Reject {
                deltas: vec![
                    AdmissionDelta::Ttl {
                        requested: "26h".to_owned(),
                        ceiling: "24h".to_owned(),
                    },
                    AdmissionDelta::Models {
                        requested: vec![ModelRef {
                            provider: "provider-c".to_owned(),
                            model: "model-c".to_owned(),
                        }],
                        ceiling: envelope.spec.llms.clone(),
                    },
                    AdmissionDelta::Tools {
                        requested: vec![ToolGrant {
                            provider: "tool-b".to_owned(),
                            resource: "deployments".to_owned(),
                            action: "write".to_owned(),
                        }],
                        ceiling: Vec::new(),
                    },
                ],
            }),
            "unapproved values must remain outside even when another value in each dimension was granted"
        );
    }
}
