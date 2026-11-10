//! LiteLLM implementation of Steward's inference enforcement plane.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use steward_ports::{
    InferenceCapabilities, InferenceCredential, InferenceObservation, InferencePlane,
    InferenceRequest, InferenceUsage, PortError, ProvisionedInference,
};
use steward_types::{Budget, ModelRef, SpendSummary};

pub const IMPLEMENTED_PORTS: [&str; 1] = ["InferencePlane"];
pub const DEFAULT_SPEND_LOG_TIMEOUT: Duration = Duration::from_secs(10);
/// Minimum gap between two warnings with the same spend-log reason.
const SPEND_LOG_WARNING_INTERVAL: Duration = Duration::from_secs(600);

pub struct LiteLlmConfig {
    pub base_url: String,
    pub master_key: String,
}

#[derive(Clone)]
pub struct LiteLlmAdapter {
    base_url: String,
    client: Client,
    master_key: String,
    spend_log_timeout: Duration,
    spend_log_warnings: Arc<SpendLogWarnings>,
}

/// Outcome of one per-key `/spend/logs` read. An unavailable read carries a stable reason so
/// operators can tell a missing database, a refused key and an unreachable gateway apart.
#[derive(Debug, PartialEq)]
enum SpendLogRead {
    Reported(ReportedUsage),
    Unavailable {
        reason: &'static str,
        http_status: Option<u16>,
    },
}

impl SpendLogRead {
    fn unavailable(reason: &'static str) -> Self {
        Self::Unavailable {
            reason,
            http_status: None,
        }
    }
}

/// Rate-limits spend-log warnings to one per reason per interval.
#[derive(Default)]
struct SpendLogWarnings {
    last_emitted: Mutex<HashMap<&'static str, Instant>>,
}

impl SpendLogWarnings {
    fn should_emit(&self, reason: &'static str, now: Instant) -> bool {
        let Ok(mut last_emitted) = self.last_emitted.lock() else {
            return false;
        };
        match last_emitted.get(reason) {
            Some(previous)
                if now.saturating_duration_since(*previous) < SPEND_LOG_WARNING_INTERVAL =>
            {
                false
            }
            _ => {
                last_emitted.insert(reason, now);
                true
            }
        }
    }
}

/// One structured warning line: component, reason code and HTTP status only. Never a key,
/// credential-bearing URL or response body.
fn spend_log_warning_line(reason: &'static str, http_status: Option<u16>) -> String {
    let mut line = serde_json::json!({
        "level": "warn",
        "component": "litellm",
        "event": "spend_logs_unavailable",
        "reason": reason,
    });
    if let (Some(status), Some(object)) = (http_status, line.as_object_mut()) {
        object.insert("httpStatus".to_owned(), Value::from(status));
    }
    line.to_string()
}

impl LiteLlmAdapter {
    pub fn new(config: LiteLlmConfig) -> Result<Self, PortError> {
        Self::new_with_spend_log_timeout(config, DEFAULT_SPEND_LOG_TIMEOUT)
    }

    pub fn new_with_spend_log_timeout(
        config: LiteLlmConfig,
        spend_log_timeout: Duration,
    ) -> Result<Self, PortError> {
        let base_url = config.base_url.trim_end_matches('/').to_owned();
        if base_url.is_empty() || config.master_key.is_empty() {
            return Err(PortError::Rejected {
                reason: "inference endpoint and management credential are required".to_owned(),
            });
        }
        if spend_log_timeout.is_zero() {
            return Err(PortError::Rejected {
                reason: "inference usage report timeout must be positive".to_owned(),
            });
        }
        Ok(Self {
            base_url,
            client: Client::new(),
            master_key: config.master_key,
            spend_log_timeout,
            spend_log_warnings: Arc::new(SpendLogWarnings::default()),
        })
    }

    async fn response_json(&self, response: reqwest::Response) -> Result<Value, PortError> {
        if !response.status().is_success() {
            return Err(PortError::Failed {
                reason: format!(
                    "inference management API returned HTTP {}",
                    response.status()
                ),
            });
        }
        response.json().await.map_err(|_| PortError::Failed {
            reason: "inference management API returned malformed JSON".to_owned(),
        })
    }

    async fn delete_alias(&self, alias: &str) -> Result<(), PortError> {
        let response = self
            .client
            .post(format!("{}/key/delete", self.base_url))
            .bearer_auth(&self.master_key)
            .json(&DeleteKeysRequest {
                key_aliases: [alias],
            })
            .send()
            .await
            .map_err(|_| PortError::Failed {
                reason: "inference management API key deletion was unavailable".to_owned(),
            })?;
        if response.status().is_success() || response.status() == StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(PortError::Failed {
                reason: format!(
                    "inference management API key deletion returned HTTP {}",
                    response.status()
                ),
            })
        }
    }

    async fn list_alias(&self, alias: &str) -> Result<Value, PortError> {
        let response = self
            .client
            .get(format!("{}/key/list", self.base_url))
            .bearer_auth(&self.master_key)
            .query(&[
                ("key_alias", alias.to_owned()),
                ("return_full_object", "true".to_owned()),
            ])
            .send()
            .await
            .map_err(|_| PortError::Failed {
                reason: "inference key lookup was unavailable".to_owned(),
            })?;
        self.response_json(response).await
    }

    async fn read_spend_logs(&self, key_hash: &str) -> SpendLogRead {
        let response = self
            .client
            .get(format!("{}/spend/logs", self.base_url))
            .bearer_auth(&self.master_key)
            .query(&[("api_key", key_hash)])
            .timeout(self.spend_log_timeout)
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) if error.is_timeout() => {
                return SpendLogRead::unavailable("gateway_timeout");
            }
            Err(_) => return SpendLogRead::unavailable("gateway_unreachable"),
        };
        let status = response.status();
        if !status.is_success() {
            let reason = match status {
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => "spend_logs_refused",
                StatusCode::NOT_FOUND => "spend_logs_not_found",
                _ => "spend_logs_failed",
            };
            return SpendLogRead::Unavailable {
                reason,
                http_status: Some(status.as_u16()),
            };
        }
        let truncated = response
            .headers()
            .get("x-litellm-spend-logs-truncated")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.eq_ignore_ascii_case("true"));
        let body = match response.bytes().await {
            Ok(body) => body,
            Err(error) if error.is_timeout() => {
                return SpendLogRead::unavailable("gateway_timeout");
            }
            Err(_) => return SpendLogRead::unavailable("gateway_unreachable"),
        };
        if truncated {
            return SpendLogRead::unavailable("spend_logs_incomplete");
        }
        match serde_json::from_slice::<Value>(&body) {
            Ok(body) => reported_usage(&body, key_hash),
            Err(_) => SpendLogRead::unavailable("spend_logs_unrecognized"),
        }
    }

    fn warn_spend_logs_unavailable(&self, reason: &'static str, http_status: Option<u16>) {
        if self.spend_log_warnings.should_emit(reason, Instant::now()) {
            eprintln!("{}", spend_log_warning_line(reason, http_status));
        }
    }
}

impl InferencePlane for LiteLlmAdapter {
    fn capabilities(&self) -> InferenceCapabilities {
        let mut capabilities = InferenceCapabilities::default();
        capabilities.model_allowlist = true;
        capabilities.spend_enforcement = true;
        capabilities
    }

    fn usage_report_timeout(&self) -> Duration {
        self.spend_log_timeout
    }

    async fn validate_configuration(
        &self,
        models: &[ModelRef],
        budget: &Budget,
    ) -> Result<(), PortError> {
        budget_limit(budget)?;
        if models.is_empty() {
            return Ok(());
        }
        let response = self
            .client
            .get(format!("{}/v1/model/info", self.base_url))
            .bearer_auth(&self.master_key)
            .send()
            .await
            .map_err(|_| PortError::Failed {
                reason: "inference model catalog was unavailable".to_owned(),
            })?;
        let catalog = self.response_json(response).await?;
        let unpriced = unpriced_models(&catalog, models);
        if unpriced.is_empty() {
            Ok(())
        } else {
            Err(PortError::Rejected {
                reason: format!(
                    "These models are not available: {}.",
                    unpriced
                        .iter()
                        .map(model_name)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            })
        }
    }

    async fn provision(
        &self,
        request: &InferenceRequest,
    ) -> Result<ProvisionedInference, PortError> {
        self.validate_configuration(&request.models, &request.budget)
            .await?;
        let max_budget = budget_limit(&request.budget)?;
        let alias = runtime_alias(&request.runtime.0);
        self.delete_alias(&alias).await?;
        let response = self
            .client
            .post(format!("{}/key/generate", self.base_url))
            .bearer_auth(&self.master_key)
            .json(&GenerateKeyRequest {
                budget_duration: "1mo",
                key_alias: &alias,
                max_budget,
                metadata: RuntimeMetadata {
                    steward_runtime_uid: &request.runtime.0,
                },
                models: request.models.iter().map(model_name).collect(),
            })
            .send()
            .await
            .map_err(|_| PortError::Failed {
                reason: "inference credential provisioning was unavailable".to_owned(),
            })?;
        let body = self.response_json(response).await?;
        let credential = body
            .get("key")
            .and_then(Value::as_str)
            .filter(|key| !key.is_empty())
            .ok_or_else(|| PortError::Failed {
                reason: "inference credential response omitted the bearer value".to_owned(),
            })?;
        Ok(ProvisionedInference {
            reference: alias,
            credential: InferenceCredential::new(credential.to_owned()),
        })
    }

    async fn reconcile_configuration(&self, request: &InferenceRequest) -> Result<(), PortError> {
        let alias = runtime_alias(&request.runtime.0);
        let body = self.list_alias(&alias).await?;
        let Some(update) = configuration_update(request, &body)? else {
            return Ok(());
        };
        let response = self
            .client
            .post(format!("{}/key/update", self.base_url))
            .bearer_auth(&self.master_key)
            .json(&update)
            .send()
            .await
            .map_err(|_| PortError::Failed {
                reason: "inference key configuration update was unavailable".to_owned(),
            })?;
        self.response_json(response).await.map(|_| ())
    }

    async fn observe(&self, request: &InferenceRequest) -> Result<InferenceObservation, PortError> {
        let alias = runtime_alias(&request.runtime.0);
        let body = self.list_alias(&alias).await?;
        let usage = match key_hash_from_key_list(&body)? {
            KeyHash::Absent => return Ok(InferenceObservation::Absent),
            KeyHash::Unavailable => None,
            KeyHash::Available(key_hash) => match self.read_spend_logs(&key_hash).await {
                SpendLogRead::Reported(usage) => Some(usage),
                SpendLogRead::Unavailable {
                    reason,
                    http_status,
                } => {
                    self.warn_spend_logs_unavailable(reason, http_status);
                    None
                }
            },
        };
        observation_from_key_list(request, &alias, &body, usage)
    }

    async fn revoke(&self, request: &InferenceRequest) -> Result<(), PortError> {
        self.delete_alias(&runtime_alias(&request.runtime.0)).await
    }
}

#[derive(Serialize)]
struct DeleteKeysRequest<'a> {
    key_aliases: [&'a str; 1],
}

#[derive(Serialize)]
struct GenerateKeyRequest<'a> {
    budget_duration: &'static str,
    key_alias: &'a str,
    max_budget: f64,
    metadata: RuntimeMetadata<'a>,
    models: Vec<String>,
}

#[derive(Serialize)]
struct RuntimeMetadata<'a> {
    steward_runtime_uid: &'a str,
}

#[derive(Serialize)]
struct UpdateKeyRequest {
    key: String,
    max_budget: f64,
    models: Vec<String>,
}

#[derive(Deserialize)]
struct KeyInfo {
    #[serde(default)]
    blocked: Option<bool>,
    max_budget: Value,
    #[serde(default)]
    models: Vec<String>,
    spend: Value,
    #[serde(default)]
    token: Option<Value>,
}

#[derive(Debug, PartialEq)]
struct ReportedUsage {
    observed_amount: String,
    total_tokens: Option<i64>,
}

fn model_name(model: &ModelRef) -> String {
    model.model.clone()
}

fn budget_limit(budget: &Budget) -> Result<f64, PortError> {
    if budget.currency != "USD" {
        return Err(PortError::Rejected {
            reason: format!(
                "configured inference plane cannot enforce {} budgets",
                budget.currency
            ),
        });
    }
    let monthly_limit = parse_budget_amount(&budget.monthly_limit)?;
    let limit = budget
        .single_run_limit
        .as_deref()
        .map(parse_budget_amount)
        .transpose()?
        .map_or(monthly_limit, |single_run_limit| {
            monthly_limit.min(single_run_limit)
        });
    Ok(limit)
}

fn parse_budget_amount(value: &str) -> Result<f64, PortError> {
    let limit = value.parse::<f64>().map_err(|_| PortError::Rejected {
        reason: "runtime inference budget is not a decimal amount".to_owned(),
    })?;
    if !limit.is_finite() || limit <= 0.0 {
        return Err(PortError::Rejected {
            reason: "runtime inference budget must be a positive finite amount".to_owned(),
        });
    }
    Ok(limit)
}

fn runtime_alias(runtime_uid: &str) -> String {
    let digest = Sha256::digest(format!("steward/inference/{runtime_uid}"));
    let suffix = digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("steward-{suffix}")
}

fn positive_number(value: Option<&Value>) -> bool {
    value
        .and_then(value_number)
        .is_some_and(|number| number.is_finite() && number > 0.0)
}

fn value_number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|number| number.parse().ok()))
}

fn unpriced_models(catalog: &Value, models: &[ModelRef]) -> Vec<ModelRef> {
    let entries = catalog
        .get("data")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    models
        .iter()
        .filter(|model| {
            let requested = model_name(model);
            !entries.iter().any(|entry| {
                entry.get("model_name").and_then(Value::as_str) == Some(requested.as_str())
                    && entry.get("model_info").is_some_and(|info| {
                        positive_number(info.get("input_cost_per_token"))
                            || positive_number(info.get("output_cost_per_token"))
                    })
            })
        })
        .cloned()
        .collect()
}

fn observation_from_key_list(
    request: &InferenceRequest,
    reference: &str,
    response: &Value,
    reported: Option<ReportedUsage>,
) -> Result<InferenceObservation, PortError> {
    let keys = response
        .get("keys")
        .and_then(Value::as_array)
        .ok_or_else(|| PortError::Failed {
            reason: "inference key list omitted the keys collection".to_owned(),
        })?;
    let [key] = keys.as_slice() else {
        return if keys.is_empty() {
            Ok(InferenceObservation::Absent)
        } else {
            Err(PortError::Failed {
                reason: "inference runtime must resolve to exactly one virtual key".to_owned(),
            })
        };
    };
    let info: KeyInfo = serde_json::from_value(key.clone()).map_err(|_| PortError::Failed {
        reason: "inference key info omitted spend enforcement fields".to_owned(),
    })?;
    let key_spend = value_number(&info.spend).ok_or_else(|| PortError::Failed {
        reason: "inference key spend was not numeric".to_owned(),
    })?;
    let max_budget = value_number(&info.max_budget).ok_or_else(|| PortError::Failed {
        reason: "inference key budget was not numeric".to_owned(),
    })?;
    if !key_spend.is_finite() || !max_budget.is_finite() || key_spend < 0.0 || max_budget <= 0.0 {
        return Err(PortError::Failed {
            reason: "inference key returned invalid spend enforcement values".to_owned(),
        });
    }
    let spend = SpendSummary {
        observed_amount: match &info.spend {
            Value::String(amount) => amount.clone(),
            amount => amount.to_string(),
        },
        currency: request.budget.currency.clone(),
    };
    let usage = reported.map(|reported| InferenceUsage {
        spend: SpendSummary {
            observed_amount: reported.observed_amount,
            currency: request.budget.currency.clone(),
        },
        total_tokens: reported.total_tokens,
    });
    let observation = if info.blocked.unwrap_or(false) || key_spend >= max_budget {
        InferenceObservation::Exhausted {
            reference: reference.to_owned(),
            spend,
            usage,
        }
    } else {
        InferenceObservation::Active {
            reference: reference.to_owned(),
            spend,
            usage,
        }
    };
    Ok(observation)
}

enum KeyHash {
    Absent,
    Available(String),
    Unavailable,
}

fn key_hash_from_key_list(response: &Value) -> Result<KeyHash, PortError> {
    let keys = response
        .get("keys")
        .and_then(Value::as_array)
        .ok_or_else(|| PortError::Failed {
            reason: "inference key list omitted the keys collection".to_owned(),
        })?;
    let [key] = keys.as_slice() else {
        return if keys.is_empty() {
            Ok(KeyHash::Absent)
        } else {
            Err(PortError::Failed {
                reason: "inference runtime must resolve to exactly one virtual key".to_owned(),
            })
        };
    };
    Ok(key
        .get("token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .map_or(KeyHash::Unavailable, KeyHash::Available))
}

fn reported_usage(response: &Value, key_hash: &str) -> SpendLogRead {
    let Some(logs) = response
        .as_array()
        .or_else(|| response.get("data").and_then(Value::as_array))
    else {
        return SpendLogRead::unavailable("spend_logs_unrecognized");
    };
    if response.get("data").is_some() {
        let incomplete_total = response
            .get("total")
            .and_then(Value::as_u64)
            .is_some_and(|total| total > logs.len() as u64);
        let multiple_pages = response
            .get("total_pages")
            .and_then(Value::as_u64)
            .is_some_and(|pages| pages > 1);
        if incomplete_total || multiple_pages {
            return SpendLogRead::unavailable("spend_logs_incomplete");
        }
    }
    let logs = logs
        .iter()
        .filter(|log| log.get("api_key").and_then(Value::as_str) == Some(key_hash))
        .collect::<Vec<_>>();
    if logs.is_empty() {
        return SpendLogRead::unavailable("spend_logs_empty");
    }
    let mut spend = "0".to_owned();
    let mut total_tokens = Some(0_i64);
    for log in logs {
        let Some(next) = log
            .get("spend")
            .and_then(value_decimal)
            .and_then(|amount| add_decimal_amount(&spend, &amount))
        else {
            return SpendLogRead::unavailable("spend_logs_unrecognized");
        };
        spend = next;
        total_tokens = total_tokens.and_then(|total| {
            log.get("total_tokens")
                .and_then(Value::as_i64)
                .filter(|tokens| *tokens >= 0)
                .and_then(|tokens| total.checked_add(tokens))
        });
    }
    SpendLogRead::Reported(ReportedUsage {
        observed_amount: spend,
        total_tokens,
    })
}

fn value_decimal(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => normalize_decimal(value),
        Value::Number(value) => {
            let value = value.to_string();
            normalize_decimal(&value)
        }
        _ => None,
    }
}

fn normalize_decimal(value: &str) -> Option<String> {
    let exponent_separator = value.find(['e', 'E']);
    let has_exponent = exponent_separator.is_some();
    let (mantissa, exponent) = if let Some(separator) = exponent_separator {
        let (mantissa, exponent) = value.split_at(separator);
        if exponent[1..].contains(['e', 'E']) {
            return None;
        }
        (mantissa, exponent[1..].parse::<i64>().ok()?)
    } else {
        (value, 0)
    };
    let (integer, fractional) = decimal_parts(mantissa)?;
    let digits = format!("{integer}{fractional}");
    if digits.bytes().all(|digit| digit == b'0') {
        return Some("0".to_owned());
    }
    if has_exponent {
        let finite_value = value.parse::<f64>().ok()?;
        if !finite_value.is_finite() || finite_value == 0.0 {
            return None;
        }
    }
    let decimal_position = i128::try_from(integer.len())
        .ok()?
        .checked_add(i128::from(exponent))?;
    let digit_count = i128::try_from(digits.len()).ok()?;
    let mut expanded = String::new();
    if decimal_position <= 0 {
        expanded.try_reserve(2).ok()?;
        expanded.push_str("0.");
        push_zeros(
            &mut expanded,
            usize::try_from(decimal_position.checked_neg()?).ok()?,
        )?;
        expanded.try_reserve(digits.len()).ok()?;
        expanded.push_str(&digits);
    } else if decimal_position >= digit_count {
        expanded.try_reserve(digits.len()).ok()?;
        expanded.push_str(&digits);
        push_zeros(
            &mut expanded,
            usize::try_from(decimal_position.checked_sub(digit_count)?).ok()?,
        )?;
    } else {
        let decimal_position = usize::try_from(decimal_position).ok()?;
        expanded.try_reserve(digits.len().checked_add(1)?).ok()?;
        expanded.push_str(&digits[..decimal_position]);
        expanded.push('.');
        expanded.push_str(&digits[decimal_position..]);
    }
    canonical_decimal(&expanded)
}

fn push_zeros(value: &mut String, count: usize) -> Option<()> {
    value.try_reserve(count).ok()?;
    value.extend(std::iter::repeat_n('0', count));
    Some(())
}

fn canonical_decimal(value: &str) -> Option<String> {
    let (integer, fractional) = decimal_parts(value)?;
    let integer = integer.trim_start_matches('0');
    let fractional = fractional.trim_end_matches('0');
    let integer = if integer.is_empty() { "0" } else { integer };
    if fractional.is_empty() {
        Some(integer.to_owned())
    } else {
        Some(format!("{integer}.{fractional}"))
    }
}

fn add_decimal_amount(left: &str, right: &str) -> Option<String> {
    let (left_integer, left_fractional) = decimal_parts(left)?;
    let (right_integer, right_fractional) = decimal_parts(right)?;
    let scale = left_fractional.len().max(right_fractional.len());
    let mut left_digits = decimal_digits(left_integer, left_fractional);
    let mut right_digits = decimal_digits(right_integer, right_fractional);
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
        while text.ends_with('0') {
            text.pop();
        }
        if text.ends_with('.') {
            text.pop();
        }
    }
    Some(text)
}

fn decimal_parts(value: &str) -> Option<(&str, &str)> {
    let mut parts = value.split('.');
    let integer = parts.next()?;
    let fractional = parts.next().unwrap_or_default();
    (parts.next().is_none()
        && !integer.is_empty()
        && integer.bytes().all(|byte| byte.is_ascii_digit())
        && fractional.bytes().all(|byte| byte.is_ascii_digit()))
    .then_some((integer, fractional))
}

fn decimal_digits(integer: &str, fractional: &str) -> Vec<u8> {
    integer
        .bytes()
        .chain(fractional.bytes())
        .map(|byte| byte - b'0')
        .collect()
}

fn configuration_update(
    request: &InferenceRequest,
    response: &Value,
) -> Result<Option<UpdateKeyRequest>, PortError> {
    let keys = response
        .get("keys")
        .and_then(Value::as_array)
        .ok_or_else(|| PortError::Failed {
            reason: "inference key list omitted the keys collection".to_owned(),
        })?;
    let [key] = keys.as_slice() else {
        return if keys.is_empty() {
            Ok(None)
        } else {
            Err(PortError::Failed {
                reason: "inference runtime must resolve to exactly one virtual key".to_owned(),
            })
        };
    };
    let info: KeyInfo = serde_json::from_value(key.clone()).map_err(|_| PortError::Failed {
        reason: "inference key info omitted configuration fields".to_owned(),
    })?;
    let max_budget = value_number(&info.max_budget).ok_or_else(|| PortError::Failed {
        reason: "inference key budget was not numeric".to_owned(),
    })?;
    let desired_budget = budget_limit(&request.budget)?;
    let desired_models = request
        .models
        .iter()
        .map(model_name)
        .collect::<BTreeSet<_>>();
    let live_models = info.models.into_iter().collect::<BTreeSet<_>>();
    if max_budget == desired_budget && live_models == desired_models {
        return Ok(None);
    }
    let token = info
        .token
        .as_ref()
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| PortError::Failed {
            reason: "inference key info omitted its update identifier".to_owned(),
        })?;
    Ok(Some(UpdateKeyRequest {
        key: token,
        max_budget: desired_budget,
        models: desired_models.into_iter().collect(),
    }))
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use serde_json::json;
    use steward_ports::{
        InferenceObservation, InferencePlane, InferenceRequest, InferenceUsage, PortError,
    };
    use steward_types::{Budget, ModelRef, RuntimeId, SpendSummary};

    use super::{
        LiteLlmAdapter, LiteLlmConfig, SPEND_LOG_WARNING_INTERVAL, SpendLogRead, SpendLogWarnings,
        budget_limit, configuration_update, observation_from_key_list, spend_log_warning_line,
        unpriced_models,
    };

    const MOCK_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

    struct MockLiteLlm {
        base_url: String,
        requests: Arc<Mutex<Vec<String>>>,
        server: thread::JoinHandle<Result<(), String>>,
    }

    impl MockLiteLlm {
        fn start(responses: Vec<&str>) -> Result<Self, String> {
            Self::start_with_timeout(responses, MOCK_REQUEST_TIMEOUT)
        }

        fn start_with_timeout(
            responses: Vec<&str>,
            request_timeout: Duration,
        ) -> Result<Self, String> {
            Self::start_with_statuses(
                responses.into_iter().map(|body| (200, body)).collect(),
                request_timeout,
            )
        }

        fn start_with_statuses(
            responses: Vec<(u16, &str)>,
            request_timeout: Duration,
        ) -> Result<Self, String> {
            Self::start_with_headers(
                responses
                    .into_iter()
                    .map(|(status, body)| (status, body, ""))
                    .collect(),
                request_timeout,
            )
        }

        fn start_with_headers(
            responses: Vec<(u16, &str, &str)>,
            request_timeout: Duration,
        ) -> Result<Self, String> {
            Self::start_with_response_delays(
                responses
                    .into_iter()
                    .map(|(status, body, headers)| (status, body, headers, Duration::from_secs(0)))
                    .collect(),
                request_timeout,
            )
        }

        fn start_with_response_delays(
            responses: Vec<(u16, &str, &str, Duration)>,
            request_timeout: Duration,
        ) -> Result<Self, String> {
            let listener = TcpListener::bind(("127.0.0.1", 0))
                .map_err(|error| format!("failed to bind mock LiteLLM: {error}"))?;
            listener
                .set_nonblocking(true)
                .map_err(|error| format!("failed to configure mock LiteLLM: {error}"))?;
            let address = listener
                .local_addr()
                .map_err(|error| format!("failed to read mock LiteLLM address: {error}"))?;
            let requests = Arc::new(Mutex::new(Vec::new()));
            let server_requests = requests.clone();
            let responses = responses
                .into_iter()
                .map(|(status, body, headers, delay)| {
                    (status, body.to_owned(), headers.to_owned(), delay)
                })
                .collect::<Vec<_>>();
            let server = thread::spawn(move || {
                for (status, response_body, response_headers, delay) in responses {
                    let deadline = Instant::now() + request_timeout;
                    let (mut stream, _) = loop {
                        match listener.accept() {
                            Ok(connection) => break connection,
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                if Instant::now() >= deadline {
                                    return Err("timed out waiting for LiteLLM management request"
                                        .to_owned());
                                }
                                thread::sleep(Duration::from_millis(10));
                            }
                            Err(error) => {
                                return Err(format!("mock LiteLLM accept failed: {error}"));
                            }
                        }
                    };
                    stream.set_nonblocking(false).map_err(|error| {
                        format!("failed to configure accepted mock LiteLLM connection: {error}")
                    })?;
                    let request = read_request(&mut stream)?;
                    server_requests
                        .lock()
                        .map_err(|_| "mock LiteLLM request lock was poisoned".to_owned())?
                        .push(request);
                    thread::sleep(delay);
                    write_response(&mut stream, status, &response_headers, &response_body)?;
                }
                Ok(())
            });
            Ok(Self {
                base_url: format!("http://{address}"),
                requests,
                server,
            })
        }

        fn finish(self) -> Result<Vec<String>, String> {
            self.server
                .join()
                .map_err(|_| "mock LiteLLM server panicked".to_owned())??;
            self.requests
                .lock()
                .map_err(|_| "mock LiteLLM request lock was poisoned".to_owned())
                .map(|requests| requests.clone())
        }
    }

    async fn assert_request_logs_supply_run_spend(spend_logs: &str) -> Result<(), String> {
        let server = MockLiteLlm::start_with_timeout(
            vec![
                r#"{"keys":[{"token":"fixture-token-hash","models":["openai/priced-model"],"spend":0,"max_budget":1.0,"blocked":false,"hosted_extension":"ignored"}]}"#,
                spend_logs,
            ],
            MOCK_REQUEST_TIMEOUT,
        )?;
        let adapter = LiteLlmAdapter::new(LiteLlmConfig {
            base_url: server.base_url.clone(),
            master_key: "fixture-management-value".to_owned(),
        })
        .map_err(|error| format!("fixture adapter must be configurable: {error:?}"))?;

        let observation = adapter
            .observe(&request())
            .await
            .map_err(|error| format!("hosted spend observation failed: {error:?}"))?;
        let requests = server.finish()?;

        assert_eq!(
            requests.len(),
            2,
            "one exact key lookup and one spend-log read are required"
        );
        assert!(requests[0].starts_with("GET /key/list?"));
        assert!(requests[1].starts_with("GET /spend/logs?api_key=fixture-token-hash "));
        assert!(requests[1].contains("authorization: Bearer fixture-management-value"));
        assert_eq!(
            observation,
            InferenceObservation::Active {
                reference: super::runtime_alias("runtime-a"),
                spend: SpendSummary {
                    observed_amount: "0".to_owned(),
                    currency: "USD".to_owned(),
                },
                usage: Some(InferenceUsage {
                    spend: SpendSummary {
                        observed_amount: "0.02".to_owned(),
                        currency: "USD".to_owned(),
                    },
                    total_tokens: Some(250),
                }),
            },
            "request-level logs must remain separate from the enforcement counter"
        );
        Ok(())
    }

    #[tokio::test]
    async fn flat_request_logs_supply_hosted_run_spend() -> Result<(), String> {
        assert_request_logs_supply_run_spend(
            r#"[
                {"api_key":"fixture-token-hash","spend":0.0125,"prompt_tokens":120,"completion_tokens":30,"total_tokens":150,"hosted_extension":"ignored"},
                {"api_key":"fixture-token-hash","spend":0.0075,"prompt_tokens":80,"completion_tokens":20,"total_tokens":100,"hosted_extension":"ignored"}
            ]"#,
        )
        .await
    }

    #[tokio::test]
    async fn data_envelope_request_logs_supply_bundled_run_spend() -> Result<(), String> {
        assert_request_logs_supply_run_spend(
            r#"{"data":[
                {"api_key":"fixture-token-hash","spend":"0.0125","prompt_tokens":120,"completion_tokens":30,"total_tokens":150,"bundled_extension":true},
                {"api_key":"fixture-token-hash","spend":"0.0075","prompt_tokens":80,"completion_tokens":20,"total_tokens":100,"bundled_extension":true}
            ],"total":2,"page":1,"page_size":50,"total_pages":1}"#,
        )
        .await
    }

    #[tokio::test]
    async fn request_log_spend_is_summed_without_floating_point_rounding() -> Result<(), String> {
        let server = MockLiteLlm::start(vec![
            r#"{"keys":[{"token":"fixture-token-hash","models":["openai/priced-model"],"spend":0,"max_budget":1.0,"blocked":false}]}"#,
            r#"[{"api_key":"fixture-token-hash","spend":0.1,"total_tokens":1},{"api_key":"fixture-token-hash","spend":0.2,"total_tokens":1}]"#,
        ])?;
        let adapter = LiteLlmAdapter::new(LiteLlmConfig {
            base_url: server.base_url.clone(),
            master_key: "fixture-management-value".to_owned(),
        })
        .map_err(|error| format!("fixture adapter must be configurable: {error:?}"))?;

        let observation = adapter
            .observe(&request())
            .await
            .map_err(|error| format!("exact usage report failed: {error:?}"))?;
        server.finish()?;

        assert!(matches!(
            observation,
            InferenceObservation::Active {
                usage: Some(InferenceUsage {
                    spend: SpendSummary { observed_amount, .. },
                    total_tokens: Some(2),
                }),
                ..
            } if observed_amount == "0.3"
        ));
        Ok(())
    }

    fn spend_log_adapter(base_url: &str) -> Result<LiteLlmAdapter, String> {
        LiteLlmAdapter::new_with_spend_log_timeout(
            LiteLlmConfig {
                base_url: base_url.to_owned(),
                master_key: "fixture-management-value".to_owned(),
            },
            Duration::from_millis(500),
        )
        .map_err(|error| format!("fixture adapter must be configurable: {error:?}"))
    }

    #[tokio::test]
    async fn each_unavailable_spend_log_read_has_a_stable_reason() -> Result<(), String> {
        let cases: Vec<(u16, &str, &str, &str, Option<u16>)> = vec![
            (
                401,
                r#"{"detail":"no"}"#,
                "",
                "spend_logs_refused",
                Some(401),
            ),
            (
                403,
                r#"{"detail":"not permitted"}"#,
                "",
                "spend_logs_refused",
                Some(403),
            ),
            (
                404,
                r#"{"detail":"Not Found"}"#,
                "",
                "spend_logs_not_found",
                Some(404),
            ),
            (500, r#"{"error":"db"}"#, "", "spend_logs_failed", Some(500)),
            (200, "[]", "", "spend_logs_empty", None),
            (
                200,
                r#"[{"api_key":"other-key-hash","spend":0.02}]"#,
                "",
                "spend_logs_empty",
                None,
            ),
            (
                200,
                r#"[{"api_key":"fixture-token-hash","spend":0.02}]"#,
                "x-litellm-spend-logs-truncated: true\r\n",
                "spend_logs_incomplete",
                None,
            ),
            (
                200,
                r#"{"data":[{"api_key":"fixture-token-hash","spend":0.02}],"total":5,"total_pages":2}"#,
                "",
                "spend_logs_incomplete",
                None,
            ),
            (
                200,
                r#"{"unexpected":true}"#,
                "",
                "spend_logs_unrecognized",
                None,
            ),
            (
                200,
                r#"[{"api_key":"fixture-token-hash","spend":"lots"}]"#,
                "",
                "spend_logs_unrecognized",
                None,
            ),
            (200, "not json", "", "spend_logs_unrecognized", None),
        ];
        for (status, body, headers, reason, http_status) in cases {
            let server = MockLiteLlm::start_with_headers(
                vec![(status, body, headers)],
                MOCK_REQUEST_TIMEOUT,
            )?;
            let read = spend_log_adapter(&server.base_url)?
                .read_spend_logs("fixture-token-hash")
                .await;
            server.finish()?;
            assert_eq!(
                read,
                SpendLogRead::Unavailable {
                    reason,
                    http_status
                },
                "status {status} body {body} must report {reason}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn unreachable_and_slow_spend_logs_have_stable_reasons() -> Result<(), String> {
        let unreachable = spend_log_adapter("http://127.0.0.1:9")?
            .read_spend_logs("fixture-token-hash")
            .await;
        assert_eq!(
            unreachable,
            SpendLogRead::Unavailable {
                reason: "gateway_unreachable",
                http_status: None
            }
        );

        let server = MockLiteLlm::start_with_response_delays(
            vec![(200, "[]", "", Duration::from_secs(2))],
            MOCK_REQUEST_TIMEOUT,
        )?;
        let slow = spend_log_adapter(&server.base_url)?
            .read_spend_logs("fixture-token-hash")
            .await;
        assert_eq!(
            slow,
            SpendLogRead::Unavailable {
                reason: "gateway_timeout",
                http_status: None
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn bundled_and_hosted_spend_log_shapes_are_reported_with_unknown_fields_ignored()
    -> Result<(), String> {
        for body in [
            r#"[{"api_key":"fixture-token-hash","spend":0.02,"total_tokens":250,"new_field":{"x":1}}]"#,
            r#"{"data":[{"api_key":"fixture-token-hash","spend":0.02,"total_tokens":250}],"total":1,"total_pages":1,"extra":true}"#,
        ] {
            let server = MockLiteLlm::start(vec![body])?;
            let read = spend_log_adapter(&server.base_url)?
                .read_spend_logs("fixture-token-hash")
                .await;
            server.finish()?;
            assert!(
                matches!(&read, SpendLogRead::Reported(usage)
                    if usage.observed_amount == "0.02" && usage.total_tokens == Some(250)),
                "{body} must be reported, got {read:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn spend_log_warnings_are_rate_limited_per_reason() {
        let warnings = SpendLogWarnings::default();
        let start = Instant::now();
        assert!(warnings.should_emit("spend_logs_refused", start));
        assert!(!warnings.should_emit("spend_logs_refused", start + Duration::from_secs(1)));
        assert!(warnings.should_emit("spend_logs_empty", start + Duration::from_secs(1)));
        assert!(warnings.should_emit(
            "spend_logs_refused",
            start + SPEND_LOG_WARNING_INTERVAL + Duration::from_secs(1)
        ));
    }

    #[test]
    fn spend_log_warning_line_names_only_component_reason_and_status() -> Result<(), String> {
        let line = spend_log_warning_line("spend_logs_refused", Some(403));
        let value: serde_json::Value = serde_json::from_str(&line)
            .map_err(|error| format!("warning must be JSON: {error}"))?;
        assert_eq!(
            value,
            json!({
                "level": "warn",
                "component": "litellm",
                "event": "spend_logs_unavailable",
                "reason": "spend_logs_refused",
                "httpStatus": 403
            })
        );
        let without_status = spend_log_warning_line("gateway_unreachable", None);
        assert!(!without_status.contains("httpStatus"));
        Ok(())
    }

    #[tokio::test]
    async fn refused_request_logs_are_not_reported_as_key_spend() -> Result<(), String> {
        let server = MockLiteLlm::start_with_statuses(
            vec![
                (
                    200,
                    r#"{"keys":[{"token":"fixture-token-hash","models":["openai/priced-model"],"spend":0.25,"max_budget":1.0,"blocked":false}]}"#,
                ),
                (403, r#"{"detail":"not permitted"}"#),
            ],
            MOCK_REQUEST_TIMEOUT,
        )?;
        let adapter = LiteLlmAdapter::new(LiteLlmConfig {
            base_url: server.base_url.clone(),
            master_key: "fixture-management-value".to_owned(),
        })
        .map_err(|error| format!("fixture adapter must be configurable: {error:?}"))?;

        let observation = adapter.observe(&request()).await.map_err(|error| {
            format!("refused usage report must preserve enforcement: {error:?}")
        })?;
        server.finish()?;

        assert_eq!(
            observation,
            InferenceObservation::Active {
                reference: super::runtime_alias("runtime-a"),
                spend: SpendSummary {
                    observed_amount: "0.25".to_owned(),
                    currency: "USD".to_owned(),
                },
                usage: None,
            },
            "a key counter may enforce a budget but must not become reported run usage"
        );
        Ok(())
    }

    #[tokio::test]
    async fn empty_request_logs_are_not_reported_as_zero_usage() -> Result<(), String> {
        let server = MockLiteLlm::start(vec![
            r#"{"keys":[{"token":"fixture-token-hash","models":["openai/priced-model"],"spend":0.25,"max_budget":1.0,"blocked":false}]}"#,
            "[]",
        ])?;
        let adapter = LiteLlmAdapter::new(LiteLlmConfig {
            base_url: server.base_url.clone(),
            master_key: "fixture-management-value".to_owned(),
        })
        .map_err(|error| format!("fixture adapter must be configurable: {error:?}"))?;

        let observation = adapter
            .observe(&request())
            .await
            .map_err(|error| format!("empty usage report blocked enforcement: {error:?}"))?;
        server.finish()?;

        assert!(matches!(
            observation,
            InferenceObservation::Active {
                spend: SpendSummary { observed_amount, .. },
                usage: None,
                ..
            } if observed_amount == "0.25"
        ));
        Ok(())
    }

    #[tokio::test]
    async fn slow_request_logs_do_not_delay_budget_enforcement() -> Result<(), String> {
        let server = MockLiteLlm::start_with_response_delays(
            vec![
                (
                    200,
                    r#"{"keys":[{"token":"fixture-token-hash","models":["openai/priced-model"],"spend":0.25,"max_budget":1.0,"blocked":false}]}"#,
                    "",
                    Duration::from_secs(0),
                ),
                (
                    200,
                    r#"[{"api_key":"fixture-token-hash","spend":0.02,"total_tokens":250}]"#,
                    "",
                    Duration::from_millis(100),
                ),
            ],
            MOCK_REQUEST_TIMEOUT,
        )?;
        let adapter = LiteLlmAdapter::new_with_spend_log_timeout(
            LiteLlmConfig {
                base_url: server.base_url.clone(),
                master_key: "fixture-management-value".to_owned(),
            },
            Duration::from_millis(50),
        )
        .map_err(|error| format!("fixture adapter must be configurable: {error:?}"))?;

        let observed =
            tokio::time::timeout(Duration::from_millis(80), adapter.observe(&request())).await;
        let server_result = server.finish();
        let observation = observed
            .map_err(|_| "slow usage reporting delayed budget enforcement".to_owned())?
            .map_err(|error| format!("slow usage report blocked enforcement: {error:?}"))?;
        server_result?;

        assert!(matches!(
            observation,
            InferenceObservation::Active {
                spend: SpendSummary { observed_amount, .. },
                usage: None,
                ..
            } if observed_amount == "0.25"
        ));
        Ok(())
    }

    #[tokio::test]
    async fn request_logs_include_only_the_requested_key_hash() -> Result<(), String> {
        let server = MockLiteLlm::start(vec![
            r#"{"keys":[{"token":"fixture-token-hash","models":["openai/priced-model"],"spend":0.25,"max_budget":1.0,"blocked":false}]}"#,
            r#"[
                {"api_key":"another-key-hash","spend":9.99,"total_tokens":999},
                {"api_key":"fixture-token-hash","spend":0.02,"total_tokens":250}
            ]"#,
        ])?;
        let adapter = LiteLlmAdapter::new(LiteLlmConfig {
            base_url: server.base_url.clone(),
            master_key: "fixture-management-value".to_owned(),
        })
        .map_err(|error| format!("fixture adapter must be configurable: {error:?}"))?;

        let observation = adapter
            .observe(&request())
            .await
            .map_err(|error| format!("mixed usage report blocked enforcement: {error:?}"))?;
        server.finish()?;

        assert!(matches!(
            observation,
            InferenceObservation::Active {
                usage: Some(InferenceUsage {
                    spend: SpendSummary { observed_amount, .. },
                    total_tokens: Some(250),
                }),
                ..
            } if observed_amount == "0.02"
        ));
        Ok(())
    }

    #[tokio::test]
    async fn exponent_form_request_log_spend_is_aggregated_exactly() -> Result<(), String> {
        let server = MockLiteLlm::start(vec![
            r#"{"keys":[{"token":"fixture-token-hash","models":["openai/priced-model"],"spend":0.25,"max_budget":1.0,"blocked":false}]}"#,
            r#"[{"api_key":"fixture-token-hash","spend":"1.5e-05","total_tokens":1}]"#,
        ])?;
        let adapter = LiteLlmAdapter::new(LiteLlmConfig {
            base_url: server.base_url.clone(),
            master_key: "fixture-management-value".to_owned(),
        })
        .map_err(|error| format!("fixture adapter must be configurable: {error:?}"))?;

        let observation = adapter
            .observe(&request())
            .await
            .map_err(|error| format!("exponent usage report blocked enforcement: {error:?}"))?;
        server.finish()?;

        assert!(matches!(
            observation,
            InferenceObservation::Active {
                usage: Some(InferenceUsage {
                    spend: SpendSummary { observed_amount, .. },
                    total_tokens: Some(1),
                }),
                ..
            } if observed_amount == "0.000015"
        ));
        Ok(())
    }

    #[tokio::test]
    async fn exponent_form_spend_outside_the_finite_range_is_not_expanded() -> Result<(), String> {
        let server = MockLiteLlm::start(vec![
            r#"{"keys":[{"token":"fixture-token-hash","models":["openai/priced-model"],"spend":0.25,"max_budget":1.0,"blocked":false}]}"#,
            r#"[{"api_key":"fixture-token-hash","spend":"1e10000","total_tokens":1}]"#,
        ])?;
        let adapter = LiteLlmAdapter::new(LiteLlmConfig {
            base_url: server.base_url.clone(),
            master_key: "fixture-management-value".to_owned(),
        })
        .map_err(|error| format!("fixture adapter must be configurable: {error:?}"))?;

        let observation = adapter
            .observe(&request())
            .await
            .map_err(|error| format!("extreme exponent blocked enforcement: {error:?}"))?;
        server.finish()?;

        assert!(matches!(
            observation,
            InferenceObservation::Active {
                spend: SpendSummary { observed_amount, .. },
                usage: None,
                ..
            } if observed_amount == "0.25"
        ));
        Ok(())
    }

    #[tokio::test]
    async fn unavailable_key_hash_does_not_block_budget_enforcement() -> Result<(), String> {
        for token_field in ["", r#","token":"""#, r#","token":17"#] {
            let body = format!(
                r#"{{"keys":[{{"models":["openai/priced-model"],"spend":1.0,"max_budget":1.0,"blocked":false{token_field}}}]}}"#
            );
            let server = MockLiteLlm::start(vec![body.as_str()])?;
            let adapter = LiteLlmAdapter::new(LiteLlmConfig {
                base_url: server.base_url.clone(),
                master_key: "fixture-management-value".to_owned(),
            })
            .map_err(|error| format!("fixture adapter must be configurable: {error:?}"))?;

            let observation = adapter.observe(&request()).await.map_err(|error| {
                format!("unavailable report key blocked enforcement: {error:?}")
            })?;
            server.finish()?;

            assert!(matches!(
                observation,
                InferenceObservation::Exhausted {
                    spend: SpendSummary { observed_amount, .. },
                    usage: None,
                    ..
                } if observed_amount == "1.0"
            ));
        }
        Ok(())
    }

    #[tokio::test]
    async fn absent_total_tokens_leave_only_tokens_unreported() -> Result<(), String> {
        let server = MockLiteLlm::start(vec![
            r#"{"keys":[{"token":"fixture-token-hash","models":["openai/priced-model"],"spend":0,"max_budget":1.0,"blocked":false}]}"#,
            r#"[{"api_key":"fixture-token-hash","spend":0.02,"prompt_tokens":200,"completion_tokens":50}]"#,
        ])?;
        let adapter = LiteLlmAdapter::new(LiteLlmConfig {
            base_url: server.base_url.clone(),
            master_key: "fixture-management-value".to_owned(),
        })
        .map_err(|error| format!("fixture adapter must be configurable: {error:?}"))?;

        let observation = adapter
            .observe(&request())
            .await
            .map_err(|error| format!("usage report without token total failed: {error:?}"))?;
        server.finish()?;

        assert!(matches!(
            observation,
            InferenceObservation::Active {
                usage: Some(InferenceUsage {
                    total_tokens: None,
                    ..
                }),
                ..
            }
        ));
        Ok(())
    }

    #[tokio::test]
    async fn invalid_request_log_spend_is_not_reported() -> Result<(), String> {
        let server = MockLiteLlm::start(vec![
            r#"{"keys":[{"token":"fixture-token-hash","models":["openai/priced-model"],"spend":0,"max_budget":1.0,"blocked":false}]}"#,
            r#"[{"api_key":"fixture-token-hash","spend":-0.01,"total_tokens":1}]"#,
        ])?;
        let adapter = LiteLlmAdapter::new(LiteLlmConfig {
            base_url: server.base_url.clone(),
            master_key: "fixture-management-value".to_owned(),
        })
        .map_err(|error| format!("fixture adapter must be configurable: {error:?}"))?;

        let observation = adapter
            .observe(&request())
            .await
            .map_err(|error| format!("invalid usage report blocked enforcement: {error:?}"))?;
        server.finish()?;

        assert!(matches!(
            observation,
            InferenceObservation::Active {
                spend: SpendSummary { observed_amount, .. },
                usage: None,
                ..
            } if observed_amount == "0"
        ));
        Ok(())
    }

    #[tokio::test]
    async fn incomplete_request_log_page_is_not_reported() -> Result<(), String> {
        let server = MockLiteLlm::start(vec![
            r#"{"keys":[{"token":"fixture-token-hash","models":["openai/priced-model"],"spend":0.25,"max_budget":1.0,"blocked":false}]}"#,
            r#"{"data":[{"api_key":"fixture-token-hash","spend":0.02,"total_tokens":250}],"total":2,"page":1,"page_size":1,"total_pages":2}"#,
        ])?;
        let adapter = LiteLlmAdapter::new(LiteLlmConfig {
            base_url: server.base_url.clone(),
            master_key: "fixture-management-value".to_owned(),
        })
        .map_err(|error| format!("fixture adapter must be configurable: {error:?}"))?;

        let observation = adapter
            .observe(&request())
            .await
            .map_err(|error| format!("incomplete usage report blocked enforcement: {error:?}"))?;
        server.finish()?;

        assert!(matches!(
            observation,
            InferenceObservation::Active {
                spend: SpendSummary { observed_amount, .. },
                usage: None,
                ..
            } if observed_amount == "0.25"
        ));
        Ok(())
    }

    #[tokio::test]
    async fn truncated_request_logs_are_not_reported() -> Result<(), String> {
        let server = MockLiteLlm::start_with_headers(
            vec![
                (
                    200,
                    r#"{"keys":[{"token":"fixture-token-hash","models":["openai/priced-model"],"spend":0.25,"max_budget":1.0,"blocked":false}]}"#,
                    "",
                ),
                (
                    200,
                    r#"[{"api_key":"fixture-token-hash","spend":0.02,"total_tokens":250}]"#,
                    "x-litellm-spend-logs-truncated: true\r\n",
                ),
            ],
            MOCK_REQUEST_TIMEOUT,
        )?;
        let adapter = LiteLlmAdapter::new(LiteLlmConfig {
            base_url: server.base_url.clone(),
            master_key: "fixture-management-value".to_owned(),
        })
        .map_err(|error| format!("fixture adapter must be configurable: {error:?}"))?;

        let observation = adapter
            .observe(&request())
            .await
            .map_err(|error| format!("truncated usage report blocked enforcement: {error:?}"))?;
        server.finish()?;

        assert!(matches!(
            observation,
            InferenceObservation::Active {
                spend: SpendSummary { observed_amount, .. },
                usage: None,
                ..
            } if observed_amount == "0.25"
        ));
        Ok(())
    }

    #[tokio::test]
    async fn request_logs_do_not_weaken_key_budget_enforcement() -> Result<(), String> {
        let server = MockLiteLlm::start(vec![
            r#"{"keys":[{"token":"fixture-token-hash","models":["openai/priced-model"],"spend":1.0,"max_budget":1.0,"blocked":false}]}"#,
            r#"[{"api_key":"fixture-token-hash","spend":0.02,"total_tokens":250}]"#,
        ])?;
        let adapter = LiteLlmAdapter::new(LiteLlmConfig {
            base_url: server.base_url.clone(),
            master_key: "fixture-management-value".to_owned(),
        })
        .map_err(|error| format!("fixture adapter must be configurable: {error:?}"))?;

        let observation = adapter
            .observe(&request())
            .await
            .map_err(|error| format!("reported usage weakened enforcement: {error:?}"))?;
        server.finish()?;

        assert!(matches!(
            observation,
            InferenceObservation::Exhausted {
                spend: SpendSummary { observed_amount, .. },
                usage: Some(InferenceUsage {
                    spend: SpendSummary {
                        observed_amount: reported_amount,
                        ..
                    },
                    total_tokens: Some(250),
                }),
                ..
            } if observed_amount == "1.0" && reported_amount == "0.02"
        ));
        Ok(())
    }

    fn request() -> InferenceRequest {
        InferenceRequest {
            runtime: RuntimeId("runtime-a".to_owned()),
            models: vec![ModelRef {
                provider: "openai".to_owned(),
                model: "priced-model".to_owned(),
            }],
            budget: Budget {
                monthly_limit: "1.00".to_owned(),
                single_run_limit: None,
                currency: "USD".to_owned(),
            },
        }
    }

    #[test]
    fn unpriced_models_fail_catalog_validation() {
        let requested = vec![ModelRef {
            provider: "openai".to_owned(),
            model: "unpriced-model".to_owned(),
        }];
        let catalog = json!({
            "data": [{
                "model_name": "unpriced-model",
                "model_info": {
                    "input_cost_per_token": 0,
                    "output_cost_per_token": 0
                }
            }]
        });

        assert_eq!(
            unpriced_models(&catalog, &requested),
            requested,
            "a zero-cost model must be rejected because it cannot enforce a spend budget"
        );
    }

    #[test]
    fn priced_hosted_alias_is_found_without_deriving_a_provider_prefix() {
        let requested = vec![ModelRef {
            provider: "provider-a".to_owned(),
            model: "hosted-alias".to_owned(),
        }];
        let catalog = json!({
            "data": [{
                "model_name": "hosted-alias",
                "model_info": {
                    "input_cost_per_token": 0.000001,
                    "output_cost_per_token": 0.000002
                },
                "unknown_hosted_field": {"ignored": true}
            }]
        });

        assert!(
            unpriced_models(&catalog, &requested).is_empty(),
            "the configured model alias must be compared exactly while unknown response fields are ignored"
        );
    }

    #[tokio::test]
    async fn unknown_hosted_alias_is_rejected_with_a_plain_named_error() -> Result<(), String> {
        let server = MockLiteLlm::start(vec![
            r#"{"data":[{"model_name":"other-alias","model_info":{"input_cost_per_token":0.000001,"output_cost_per_token":0.000002},"unknown_hosted_field":true}]}"#,
        ])?;
        let adapter = LiteLlmAdapter::new(LiteLlmConfig {
            base_url: server.base_url.clone(),
            master_key: "fixture-management-value".to_owned(),
        })
        .map_err(|error| format!("configure hosted fake adapter: {error:?}"))?;
        let models = [ModelRef {
            provider: "provider-a".to_owned(),
            model: "unknown-alias".to_owned(),
        }];
        let budget = Budget {
            monthly_limit: "1.00".to_owned(),
            single_run_limit: None,
            currency: "USD".to_owned(),
        };

        assert_eq!(
            adapter.validate_configuration(&models, &budget).await,
            Err(PortError::Rejected {
                reason: "These models are not available: unknown-alias.".to_owned(),
            })
        );
        let requests = server.finish()?;
        assert_eq!(requests.len(), 1);
        assert!(requests[0].contains("authorization: Bearer fixture-management-value"));
        Ok(())
    }

    #[tokio::test]
    async fn hosted_alias_provisions_an_exact_runtime_key_allowlist() -> Result<(), String> {
        let server = MockLiteLlm::start(vec![
            r#"{"data":[{"model_name":"priced-model","model_info":{"input_cost_per_token":0.000001,"output_cost_per_token":0.000002},"unknown_hosted_field":true}]}"#,
            "{}",
            r#"{"key":"fixture-runtime-value","unknown_hosted_field":true}"#,
        ])?;
        let adapter = LiteLlmAdapter::new(LiteLlmConfig {
            base_url: server.base_url.clone(),
            master_key: "fixture-management-value".to_owned(),
        })
        .map_err(|error| format!("configure hosted fake adapter: {error:?}"))?;

        let provisioned = adapter
            .provision(&request())
            .await
            .map_err(|error| format!("provision hosted alias: {error:?}"))?;

        assert_eq!(
            provisioned.credential.expose_secret(),
            "fixture-runtime-value"
        );
        let requests = server.finish()?;
        assert_eq!(requests.len(), 3);
        assert!(
            requests
                .iter()
                .all(|request| request.contains("authorization: Bearer fixture-management-value"))
        );
        assert!(requests[0].starts_with("GET /v1/model/info "));
        assert!(requests[1].starts_with("POST /key/delete "));
        assert!(requests[2].starts_with("POST /key/generate "));
        assert!(requests[2].contains(r#""models":["priced-model"]"#));
        assert!(!requests[2].contains("openai/priced-model"));
        Ok(())
    }

    #[tokio::test]
    async fn non_usd_budget_fails_configuration_validation() -> Result<(), String> {
        let adapter = LiteLlmAdapter::new(LiteLlmConfig {
            base_url: "http://127.0.0.1:9".to_owned(),
            master_key: "fixture-management-value".to_owned(),
        })
        .map_err(|error| format!("fixture adapter must be configurable: {error:?}"))?;
        let budget = Budget {
            monthly_limit: "1.00".to_owned(),
            single_run_limit: None,
            currency: "EUR".to_owned(),
        };

        let result = adapter.validate_configuration(&[], &budget).await;

        assert_eq!(
            result,
            Err(PortError::Rejected {
                reason: "configured inference plane cannot enforce EUR budgets".to_owned(),
            }),
            "unsupported budget currencies must fail before an AgentRuntime is persisted"
        );
        Ok(())
    }

    #[test]
    fn single_run_limit_caps_the_gateway_key_budget() -> Result<(), String> {
        let budget = Budget {
            monthly_limit: "100.00".to_owned(),
            single_run_limit: Some("0.25".to_owned()),
            currency: "USD".to_owned(),
        };

        assert_eq!(
            budget_limit(&budget)
                .map_err(|error| format!("fixture budget must be enforceable: {error:?}"))?,
            0.25,
            "a runtime key must use the smaller per-run authority instead of the monthly ceiling"
        );
        Ok(())
    }

    #[tokio::test]
    async fn existing_key_configuration_is_updated_without_resetting_spend() -> Result<(), String> {
        let server = MockLiteLlm::start(vec![
            r#"{"keys":[{"token":"fixture-token-hash","models":["openai/old-model"],"spend":0.25,"max_budget":1.0,"blocked":false}]}"#,
            "{}",
        ])?;
        let adapter = LiteLlmAdapter::new(LiteLlmConfig {
            base_url: server.base_url.clone(),
            master_key: "fixture-management-value".to_owned(),
        })
        .map_err(|error| format!("fixture adapter must be configurable: {error:?}"))?;

        let mut desired = request();
        desired.budget.monthly_limit = "2.00".to_owned();
        desired.budget.single_run_limit = Some("0.50".to_owned());
        adapter
            .reconcile_configuration(&desired)
            .await
            .map_err(|error| format!("configuration reconciliation failed: {error:?}"))?;

        let requests = server.finish()?;
        assert_eq!(
            requests.len(),
            2,
            "configuration reconciliation must inspect and then update the existing key"
        );
        assert!(requests[0].starts_with("GET /key/list?"));
        let update = &requests[1];
        assert!(update.starts_with("POST /key/update "));
        assert!(update.contains(r#""key":"fixture-token-hash""#));
        assert!(update.contains(r#""models":["priced-model"]"#));
        assert!(update.contains(r#""max_budget":0.5"#));
        assert!(
            !update.contains(r#""spend""#),
            "an in-place policy update must not reset accumulated spend: {update}"
        );
        Ok(())
    }

    #[test]
    fn matching_key_configuration_requires_no_mutation() -> Result<(), String> {
        assert!(
            configuration_update(
                &request(),
                &json!({
                    "keys": [{
                        "token": "fixture-token-hash",
                        "models": ["priced-model"],
                        "spend": 0.25,
                        "max_budget": 1.0,
                        "blocked": false
                    }]
                }),
            )
            .map_err(|error| format!("matching key info must be readable: {error:?}"))?
            .is_none(),
            "an already-converged key must not be rewritten on the controller's periodic reconcile"
        );
        Ok(())
    }

    #[test]
    fn spend_at_the_runtime_limit_is_exhausted() -> Result<(), String> {
        let observation = observation_from_key_list(
            &request(),
            "runtime-a",
            &json!({
                "keys": [{
                    "spend": 1.0,
                    "max_budget": 1.0,
                    "blocked": false
                }]
            }),
            None,
        )
        .map_err(|error| format!("key info must be readable: {error:?}"))?;

        assert_eq!(
            observation,
            InferenceObservation::Exhausted {
                reference: "runtime-a".to_owned(),
                spend: SpendSummary {
                    observed_amount: "1.0".to_owned(),
                    currency: "USD".to_owned(),
                },
                usage: None,
            },
            "spend at the hard limit must drive the runtime to exhaustion"
        );
        Ok(())
    }

    #[test]
    fn null_blocked_from_pinned_key_list_is_not_blocked() -> Result<(), String> {
        let observation = observation_from_key_list(
            &request(),
            "runtime-a",
            &json!({
                "keys": [{
                    "spend": 0.25,
                    "max_budget": 1.0,
                    "blocked": null
                }]
            }),
            None,
        )
        .map_err(|error| format!("pinned LiteLLM key info must be readable: {error:?}"))?;

        assert_eq!(
            observation,
            InferenceObservation::Active {
                reference: "runtime-a".to_owned(),
                spend: SpendSummary {
                    observed_amount: "0.25".to_owned(),
                    currency: "USD".to_owned(),
                },
                usage: None,
            },
            "a null blocked field in pinned LiteLLM 1.93.0 means the key is not blocked"
        );
        Ok(())
    }

    fn read_request(stream: &mut TcpStream) -> Result<String, String> {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let read = stream
                .read(&mut buffer)
                .map_err(|error| format!("mock LiteLLM read failed: {error}"))?;
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..read]);
            if request_is_complete(&bytes)? {
                break;
            }
        }
        String::from_utf8(bytes)
            .map_err(|error| format!("mock LiteLLM request was not UTF-8: {error}"))
    }

    fn request_is_complete(bytes: &[u8]) -> Result<bool, String> {
        let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
            return Ok(false);
        };
        let headers = std::str::from_utf8(&bytes[..header_end])
            .map_err(|error| format!("mock LiteLLM headers were not UTF-8: {error}"))?;
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(str::trim)
                    .map(str::to_owned)
            })
            .map(|value| {
                value
                    .parse::<usize>()
                    .map_err(|error| format!("invalid mock LiteLLM content length: {error}"))
            })
            .transpose()?
            .unwrap_or_default();
        Ok(bytes.len() >= header_end + 4 + content_length)
    }

    fn write_response(
        stream: &mut TcpStream,
        status: u16,
        headers: &str,
        body: &str,
    ) -> Result<(), String> {
        let reason = if status == 200 {
            "OK"
        } else {
            "Fixture response"
        };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .map_err(|error| format!("mock LiteLLM write failed: {error}"))
    }
}
