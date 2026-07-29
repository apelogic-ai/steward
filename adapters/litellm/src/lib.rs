//! LiteLLM implementation of Steward's inference enforcement plane.

use std::collections::BTreeSet;

use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use steward_ports::{
    InferenceCapabilities, InferenceCredential, InferenceObservation, InferencePlane,
    InferenceRequest, PortError, ProvisionedInference,
};
use steward_types::{Budget, ModelRef, SpendSummary};

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
}

impl InferencePlane for LiteLlmAdapter {
    fn capabilities(&self) -> InferenceCapabilities {
        let mut capabilities = InferenceCapabilities::default();
        capabilities.model_allowlist = true;
        capabilities.spend_enforcement = true;
        capabilities
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
        observation_from_key_list(request, &alias, &body)
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
    token: Option<String>,
}

fn model_name(model: &ModelRef) -> String {
    format!("{}/{}", model.provider, model.model)
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
    let limit = budget
        .monthly_limit
        .parse::<f64>()
        .map_err(|_| PortError::Rejected {
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
        .filter(|token| !token.is_empty())
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
    use steward_ports::{InferenceObservation, InferencePlane, InferenceRequest, PortError};
    use steward_types::{Budget, ModelRef, RuntimeId, SpendSummary};

    use super::{
        LiteLlmAdapter, LiteLlmConfig, configuration_update, observation_from_key_list,
        unpriced_models,
    };

    struct MockLiteLlm {
        base_url: String,
        requests: Arc<Mutex<Vec<String>>>,
        server: thread::JoinHandle<Result<(), String>>,
    }

    impl MockLiteLlm {
        fn start(responses: Vec<&str>) -> Result<Self, String> {
            let listener = TcpListener::bind(("localhost", 0))
                .map_err(|error| format!("failed to bind mock LiteLLM: {error}"))?;
            listener
                .set_nonblocking(true)
                .map_err(|error| format!("failed to configure mock LiteLLM: {error}"))?;
            let address = listener
                .local_addr()
                .map_err(|error| format!("failed to read mock LiteLLM address: {error}"))?;
            let requests = Arc::new(Mutex::new(Vec::new()));
            let server_requests = requests.clone();
            let responses = responses.into_iter().map(str::to_owned).collect::<Vec<_>>();
            let server = thread::spawn(move || {
                for response_body in responses {
                    let deadline = Instant::now() + Duration::from_secs(2);
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
                    let request = read_request(&mut stream)?;
                    server_requests
                        .lock()
                        .map_err(|_| "mock LiteLLM request lock was poisoned".to_owned())?
                        .push(request);
                    write_response(&mut stream, &response_body)?;
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

    #[tokio::test]
    async fn non_usd_budget_fails_configuration_validation() -> Result<(), String> {
        let adapter = LiteLlmAdapter::new(LiteLlmConfig {
            base_url: "http://127.0.0.1:9".to_owned(),
            master_key: "fixture-management-value".to_owned(),
        })
        .map_err(|error| format!("fixture adapter must be configurable: {error:?}"))?;
        let budget = Budget {
            monthly_limit: "1.00".to_owned(),
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
        assert!(update.contains(r#""models":["openai/priced-model"]"#));
        assert!(update.contains(r#""max_budget":2.0"#));
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
                        "models": ["openai/priced-model"],
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

    fn write_response(stream: &mut TcpStream, body: &str) -> Result<(), String> {
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .map_err(|error| format!("mock LiteLLM write failed: {error}"))
    }
}
