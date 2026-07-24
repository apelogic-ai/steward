//! Shared admission boundary for every desired-state writer.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};
use steward_types::{AgentRuntimeSpec, Budget, Duration, ModelRef, Principal, ToolGrant};

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
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "error")]
pub enum AdmissionError {
    InvalidBudget { value: String },
    CurrencyMismatch { requested: String, ceiling: String },
    InvalidTtl { value: String },
    UnsupportedServicePrincipal,
    UnsupportedBindings,
}

pub fn evaluate(
    request: &AgentRuntimeSpec,
    envelope: &Envelope,
) -> Result<AdmissionDecision, AdmissionError> {
    if matches!(request.principal, Principal::Service { .. }) {
        return Err(AdmissionError::UnsupportedServicePrincipal);
    }
    if request.bindings.is_some() {
        return Err(AdmissionError::UnsupportedBindings);
    }
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
    if deltas.is_empty() {
        Ok(AdmissionDecision::Admit)
    } else {
        Ok(AdmissionDecision::Reject { deltas })
    }
}

#[derive(Debug)]
struct Decimal<'a> {
    integer: &'a str,
    fractional: &'a str,
}

impl<'a> Decimal<'a> {
    fn parse(value: &'a str) -> Result<Self, AdmissionError> {
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

fn compare_fractional(left: &str, right: &str) -> Ordering {
    let width = left.len().max(right.len());
    left.bytes()
        .chain(std::iter::repeat(b'0'))
        .zip(right.bytes().chain(std::iter::repeat(b'0')))
        .take(width)
        .find_map(|(left, right)| (left != right).then(|| left.cmp(&right)))
        .unwrap_or(Ordering::Equal)
}

fn duration_seconds(duration: &Duration) -> Result<u64, AdmissionError> {
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

#[cfg(test)]
mod tests {
    use steward_types::{
        AgentRuntimeSpec, AgentType, BindingRef, Budget, Duration, Email, ModelRef, Principal,
        ToolGrant,
    };

    use super::{
        AdmissionDecision, AdmissionDelta, AdmissionError, Envelope, EnvelopeSpec, evaluate,
    };

    fn request_with_budget(monthly_limit: &str) -> AgentRuntimeSpec {
        AgentRuntimeSpec {
            principal: Principal::User {
                acting_user: Email("alice@example.com".to_owned()),
            },
            owner: Email("alice@example.com".to_owned()),
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
            },
        }
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
    fn reserved_plane_b_and_service_shapes_fail_closed() {
        let envelope = envelope_with_budget("200.00");
        let mut service = request_with_budget("100.00");
        service.principal = Principal::Service {
            name: "service-a".to_owned(),
        };
        assert_eq!(
            evaluate(&service, &envelope),
            Err(AdmissionError::UnsupportedServicePrincipal)
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
}
