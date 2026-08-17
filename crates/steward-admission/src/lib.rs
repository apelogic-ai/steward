//! Shared admission boundary for every desired-state writer.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};
use steward_types::{
    AgentRuntimeSpec, Budget, Duration, KubernetesQuantity, ModelRef, RunnerPlatform,
    RunnerRequirements, SpendSummary, ToolGrant,
};

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
    pub ttl: Duration,
    #[serde(default)]
    pub runner: RunnerRequirements,
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
        Some(format!("envelope exceeded: {details}"))
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
}

impl AdmissionDelta {
    fn counterexample(&self) -> String {
        match self {
            Self::Budget {
                requested,
                ceiling,
                currency,
            } => format!(
                "budget.monthlyLimit requested {requested} {currency}, ceiling {ceiling} {currency}"
            ),
            Self::Ttl { requested, ceiling } => {
                format!("ttl requested {requested}, ceiling {ceiling}")
            }
            Self::Models { requested, ceiling } => format!(
                "llms requested [{}], ceiling [{}]",
                requested
                    .iter()
                    .map(|model| format!("{}/{}", model.provider, model.model))
                    .collect::<Vec<_>>()
                    .join(", "),
                ceiling
                    .iter()
                    .map(|model| format!("{}/{}", model.provider, model.model))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::Tools { requested, ceiling } => format!(
                "tools requested [{}], ceiling [{}]",
                requested
                    .iter()
                    .map(|tool| format!("{}:{}:{}", tool.provider, tool.resource, tool.action))
                    .collect::<Vec<_>>()
                    .join(", "),
                ceiling
                    .iter()
                    .map(|tool| format!("{}:{}:{}", tool.provider, tool.resource, tool.action))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::RunnerPlatforms { requested, ceiling } => format!(
                "runner.platforms requested {:?}, ceiling {:?}",
                requested, ceiling
            ),
            Self::RunnerMemory { requested, ceiling } => format!(
                "runner.memory requested {}, ceiling {}",
                requested.0,
                ceiling.as_ref().map_or("none", |value| value.0.as_str())
            ),
            Self::RunnerCompute { requested, ceiling } => format!(
                "runner.compute requested {}, ceiling {}",
                requested.0,
                ceiling.as_ref().map_or("none", |value| value.0.as_str())
            ),
            Self::RunnerStorage { requested, ceiling } => format!(
                "runner.storage requested {}, ceiling {}",
                requested.0,
                ceiling.as_ref().map_or("none", |value| value.0.as_str())
            ),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "error")]
pub enum AdmissionError {
    InvalidBudget { value: String },
    InvalidCurrency { value: String },
    CurrencyMismatch { requested: String, ceiling: String },
    InvalidTtl { value: String },
    InvalidRunnerQuantity { resource: String, value: String },
    DuplicateRunnerPlatform { platform: RunnerPlatform },
    UnsupportedBindings,
}

pub fn validate_envelope(envelope: &Envelope) -> Result<(), AdmissionError> {
    Decimal::parse(&envelope.spec.budget.monthly_limit)?;
    if envelope.spec.budget.currency.len() != 3
        || !envelope
            .spec
            .budget
            .currency
            .bytes()
            .all(|byte| byte.is_ascii_uppercase())
    {
        return Err(AdmissionError::InvalidCurrency {
            value: envelope.spec.budget.currency.clone(),
        });
    }
    duration_seconds(&envelope.spec.ttl)?;
    validate_runner(&envelope.spec.runner)?;
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
            ttl: request.ttl.clone(),
            runner: request.runner.clone(),
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
    if deltas.is_empty() {
        Ok(AdmissionDecision::Admit)
    } else {
        Ok(AdmissionDecision::Reject { deltas })
    }
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
        _ => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use steward_types::{
        AgentRuntimeSpec, AgentType, BindingRef, Budget, Duration, Email, KubernetesQuantity,
        ModelRef, Principal, RunnerPlatform, RunnerRequirements, ToolGrant,
    };

    use super::{
        AdmissionDecision, AdmissionDelta, AdmissionError, Envelope, EnvelopeSpec,
        add_budget_amount, envelope_is_within, evaluate, evaluate_with_grants, validate_envelope,
    };

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
                        currency: currency.to_owned(),
                    },
                    ttl: Duration(ttl.to_owned()),
                    runner: RunnerRequirements::default(),
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
                    currency: "USD".to_owned(),
                },
                ttl: Duration("24h".to_owned()),
                runner: RunnerRequirements::default(),
            },
        }
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
    fn counterexample_message_is_stable_for_both_front_doors() {
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
                "envelope exceeded: budget.monthlyLimit requested 220.00 USD, ceiling 200.00 USD; ttl requested 25h, ceiling 24h"
                    .to_owned()
            )
        );
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
