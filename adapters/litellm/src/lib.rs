//! LiteLLM implementation of Steward's inference enforcement plane.

use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use steward_ports::{
    InferenceCapabilities, InferenceCredential, InferenceObservation, InferencePlane,
    InferenceRequest, PortError, ProvisionedInference,
};
use steward_types::{ModelRef, SpendSummary};

pub const IMPLEMENTED_PORTS: [&str; 1] = ["InferencePlane"];

pub struct LiteLlmConfig {
    pub base_url: String,
    pub master_key: String,
}

#[derive(Clone)]
pub struct LiteLlmAdapter {
    base_url: String,
    client: Client,
    master_key: String,
}

impl LiteLlmAdapter {
    pub fn new(config: LiteLlmConfig) -> Result<Self, PortError> {
        let base_url = config.base_url.trim_end_matches('/').to_owned();
        if base_url.is_empty() || config.master_key.is_empty() {
            return Err(PortError::Rejected {
                reason: "inference endpoint and management credential are required".to_owned(),
            });
        }
        Ok(Self {
            base_url,
            client: Client::new(),
            master_key: config.master_key,
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
}

impl InferencePlane for LiteLlmAdapter {
    fn capabilities(&self) -> InferenceCapabilities {
        let mut capabilities = InferenceCapabilities::default();
        capabilities.model_allowlist = true;
        capabilities.spend_enforcement = true;
        capabilities
    }

    async fn validate_models(&self, models: &[ModelRef]) -> Result<(), PortError> {
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
                    "models are absent from the priced inference catalog: {}",
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
        self.validate_models(&request.models).await?;
        if request.budget.currency != "USD" {
            return Err(PortError::Rejected {
                reason: format!(
                    "configured inference plane cannot enforce {} budgets",
                    request.budget.currency
                ),
            });
        }
        let max_budget =
            request
                .budget
                .monthly_limit
                .parse::<f64>()
                .map_err(|_| PortError::Rejected {
                    reason: "runtime inference budget is not a decimal amount".to_owned(),
                })?;
        if !max_budget.is_finite() || max_budget <= 0.0 {
            return Err(PortError::Rejected {
                reason: "runtime inference budget must be a positive finite amount".to_owned(),
            });
        }
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

    async fn observe(&self, request: &InferenceRequest) -> Result<InferenceObservation, PortError> {
        let response = self
            .client
            .get(format!("{}/key/list", self.base_url))
            .bearer_auth(&self.master_key)
            .query(&[
                ("key_alias", runtime_alias(&request.runtime.0)),
                ("return_full_object", "true".to_owned()),
            ])
            .send()
            .await
            .map_err(|_| PortError::Failed {
                reason: "inference spend observation was unavailable".to_owned(),
            })?;
        let body = self.response_json(response).await?;
        observation_from_key_list(request, &runtime_alias(&request.runtime.0), &body)
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

#[derive(Deserialize)]
struct KeyInfo {
    #[serde(default)]
    blocked: Option<bool>,
    max_budget: Value,
    spend: Value,
}

fn model_name(model: &ModelRef) -> String {
    format!("{}/{}", model.provider, model.model)
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
    let spend = value_number(&info.spend).ok_or_else(|| PortError::Failed {
        reason: "inference key spend was not numeric".to_owned(),
    })?;
    let max_budget = value_number(&info.max_budget).ok_or_else(|| PortError::Failed {
        reason: "inference key budget was not numeric".to_owned(),
    })?;
    if !spend.is_finite() || !max_budget.is_finite() || spend < 0.0 || max_budget <= 0.0 {
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
    let observation = if info.blocked.unwrap_or(false) || spend_amount(&spend)? >= max_budget {
        InferenceObservation::Exhausted {
            reference: reference.to_owned(),
            spend,
        }
    } else {
        InferenceObservation::Active {
            reference: reference.to_owned(),
            spend,
        }
    };
    Ok(observation)
}

fn spend_amount(spend: &SpendSummary) -> Result<f64, PortError> {
    spend
        .observed_amount
        .parse()
        .map_err(|_| PortError::Failed {
            reason: "inference key spend was not numeric".to_owned(),
        })
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use steward_ports::{InferenceObservation, InferenceRequest};
    use steward_types::{Budget, ModelRef, RuntimeId, SpendSummary};

    use super::{observation_from_key_list, unpriced_models};

    fn request() -> InferenceRequest {
        InferenceRequest {
            runtime: RuntimeId("runtime-a".to_owned()),
            models: vec![ModelRef {
                provider: "openai".to_owned(),
                model: "priced-model".to_owned(),
            }],
            budget: Budget {
                monthly_limit: "1.00".to_owned(),
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
                "model_name": "openai/unpriced-model",
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
            },
            "a null blocked field in pinned LiteLLM 1.93.0 means the key is not blocked"
        );
        Ok(())
    }
}
