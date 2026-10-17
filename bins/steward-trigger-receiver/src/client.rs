use std::time::Duration;

use k8s_openapi::api::authentication::v1::{TokenRequest, TokenRequestSpec};
use k8s_openapi::api::core::v1::ServiceAccount;
use kube::Client as KubeClient;
use kube::api::{Api, PostParams};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::time::sleep;

use crate::{ReceiverFuture, TaskInvocation, TaskLifecycle, TaskStart, TriggerConfig};
use steward_apiserver::browser_admin::CapabilityCatalog;
use steward_store::PgStore;
use steward_types::CanonicalUserId;

#[derive(Clone)]
pub struct PgEnvelopeAuthority {
    store: PgStore,
}

impl PgEnvelopeAuthority {
    pub fn new(store: PgStore) -> Self {
        Self { store }
    }
}

impl crate::EnvelopeAuthority for PgEnvelopeAuthority {
    fn active_tools<'a>(
        &'a self,
        owner: &'a CanonicalUserId,
        digest: &'a str,
    ) -> ReceiverFuture<'a, Result<Vec<steward_types::ToolGrant>, crate::EnvelopeError>> {
        Box::pin(async move {
            let rows = self
                .store
                .active_provisioned_user_envelopes_by_digest(owner, digest)
                .await
                .map_err(|_| crate::EnvelopeError::Unavailable)?;
            if rows.len() != 1 {
                return Err(if rows.is_empty() {
                    crate::EnvelopeError::Absent
                } else {
                    crate::EnvelopeError::Ambiguous
                });
            }
            let envelope = rows[0]
                .approved_envelope
                .as_ref()
                .ok_or(crate::EnvelopeError::Stale)?;
            Ok(envelope.spec.tools.clone())
        })
    }
}

#[derive(Clone)]
pub struct StaticCatalog(pub CapabilityCatalog);

impl crate::CapabilityCatalogReader for StaticCatalog {
    fn current(&self) -> ReceiverFuture<'_, Result<CapabilityCatalog, String>> {
        Box::pin(async move { Ok(self.0.clone()) })
    }
}

#[derive(Clone)]
pub struct KubernetesTokenRequester {
    client: KubeClient,
    namespace: String,
    service_account: String,
    audience: String,
}

impl KubernetesTokenRequester {
    pub fn new(
        client: KubeClient,
        namespace: String,
        service_account: String,
        audience: String,
    ) -> Result<Self, String> {
        if namespace.is_empty() || service_account.is_empty() || audience.is_empty() {
            return Err("TokenRequest settings are incomplete".to_owned());
        }
        Ok(Self {
            client,
            namespace,
            service_account,
            audience,
        })
    }

    pub async fn request(&self) -> Result<String, String> {
        let api: Api<ServiceAccount> = Api::namespaced(self.client.clone(), &self.namespace);
        let request = TokenRequest {
            metadata: Default::default(),
            spec: TokenRequestSpec {
                audiences: vec![self.audience.clone()],
                expiration_seconds: Some(300),
                bound_object_ref: None,
            },
            status: None,
        };
        let response = api
            .create_token_request(&self.service_account, &PostParams::default(), &request)
            .await
            .map_err(|_| "TokenRequest was refused".to_owned())?;
        response
            .status
            .map(|status| status.token)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| "TokenRequest reply is invalid".to_owned())
    }
}

#[derive(Clone)]
pub struct WorkloadExchangeClient {
    http: Client,
    exchange_endpoint: String,
    task_origin: String,
    audience: String,
    source_token: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenResponse {
    access_token: String,
    token_type: String,
    expires_in: u64,
}

#[derive(Debug, Serialize)]
struct TaskRequest<'a> {
    workflow: &'a str,
    #[serde(rename = "envelopeDigest")]
    envelope_digest: &'a str,
}

impl WorkloadExchangeClient {
    pub fn new(
        exchange_endpoint: String,
        task_origin: String,
        audience: String,
        source_token: String,
    ) -> Result<Self, String> {
        if exchange_endpoint.is_empty()
            || task_origin.is_empty()
            || audience.is_empty()
            || source_token.is_empty()
        {
            return Err("workload exchange settings are incomplete".to_owned());
        }
        Ok(Self {
            http: Client::new(),
            exchange_endpoint,
            task_origin,
            audience,
            source_token,
        })
    }

    async fn token(&self) -> Result<String, String> {
        let response = self
            .http
            .post(&self.exchange_endpoint)
            .form(&[
                ("grant_type", "client_credentials"),
                ("audience", self.audience.as_str()),
                ("client_assertion", self.source_token.as_str()),
            ])
            .send()
            .await
            .map_err(|_| "workload exchange failed".to_owned())?;
        if response.status() != StatusCode::OK {
            return Err("workload exchange was refused".to_owned());
        }
        let token: TokenResponse = response
            .json()
            .await
            .map_err(|_| "workload exchange reply is invalid".to_owned())?;
        if token.token_type != "Bearer" || token.access_token.is_empty() || token.expires_in == 0 {
            return Err("workload exchange reply is invalid".to_owned());
        }
        Ok(token.access_token)
    }

    async fn run(
        &self,
        _trigger: &TriggerConfig,
        invocation: TaskInvocation,
    ) -> Result<(), String> {
        let token = self.token().await?;
        let auth = format!("Bearer {token}");
        let create = self
            .http
            .post(format!("{}/v1/tasks", self.task_origin))
            .bearer_auth(&token)
            .header("Idempotency-Key", &invocation.idempotency_key)
            .json(&TaskRequest {
                workflow: &invocation.workflow,
                envelope_digest: &invocation.envelope_digest,
            })
            .send()
            .await
            .map_err(|_| "task creation failed".to_owned())?;
        if !matches!(create.status(), StatusCode::ACCEPTED | StatusCode::OK) {
            return Err("task creation was refused".to_owned());
        }
        let created: serde_json::Value = create
            .json()
            .await
            .map_err(|_| "task creation reply is invalid".to_owned())?;
        let uid = created
            .get("taskUid")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "task creation reply is invalid".to_owned())?;
        let base = format!("{}/v1/tasks/{uid}", self.task_origin);
        let upload = self
            .http
            .put(format!("{base}/inputs"))
            .header("Authorization", &auth)
            .header("Content-Type", "application/x-tar")
            .body(invocation.input_archive)
            .send()
            .await
            .map_err(|_| "task inputs failed".to_owned())?;
        if upload.status() != StatusCode::NO_CONTENT {
            return Err("task inputs were refused".to_owned());
        }
        let execute = self
            .http
            .post(format!("{base}/execute"))
            .header("Authorization", &auth)
            .send()
            .await
            .map_err(|_| "task execution failed".to_owned())?;
        if execute.status() != StatusCode::ACCEPTED {
            return Err("task execution was refused".to_owned());
        }
        loop {
            let status = self
                .http
                .get(&base)
                .header("Authorization", &auth)
                .send()
                .await
                .map_err(|_| "task status failed".to_owned())?;
            if status.status() != StatusCode::OK {
                return Err("task status was refused".to_owned());
            }
            let value: serde_json::Value = status
                .json()
                .await
                .map_err(|_| "task status reply is invalid".to_owned())?;
            match value.get("phase").and_then(serde_json::Value::as_str) {
                Some("succeeded") | Some("failed") | Some("cancelled") => return Ok(()),
                Some(_) => sleep(Duration::from_millis(100)).await,
                None => return Err("task status reply is invalid".to_owned()),
            }
        }
    }
}

impl TaskLifecycle for WorkloadExchangeClient {
    fn start<'a>(
        &'a self,
        trigger: &'a TriggerConfig,
        invocation: TaskInvocation,
    ) -> ReceiverFuture<'a, Result<TaskStart, String>> {
        Box::pin(async move {
            let this = self.clone();
            let trigger = trigger.clone();
            Ok(TaskStart::Started(Box::pin(async move {
                let _ = this.run(&trigger, invocation).await;
            })))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_incomplete_exchange_settings() {
        assert!(
            WorkloadExchangeClient::new(
                String::new(),
                "https://api.test".into(),
                "aud".into(),
                "token".into()
            )
            .is_err()
        );
    }
}
