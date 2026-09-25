use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

pub const LEGACY_TASK_TOKEN_CONTRACT: &str = "steward-task-v2";
pub const FEDERATED_TASK_TOKEN_CONTRACT: &str = "steward-task-v3";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskAuthDiscoveryConfig {
    resource: String,
    authorization_server: String,
    federated_subjects_enabled: bool,
}

impl TaskAuthDiscoveryConfig {
    pub fn new(
        resource: String,
        authorization_server: String,
        federated_subjects_enabled: bool,
    ) -> Result<Self, String> {
        validate_resource_url(&resource, false)?;
        validate_authorization_server_url(&authorization_server, false)?;
        Ok(Self {
            resource,
            authorization_server,
            federated_subjects_enabled,
        })
    }

    #[cfg(test)]
    fn new_for_test(
        resource: String,
        authorization_server: String,
        federated_subjects_enabled: bool,
    ) -> Result<Self, String> {
        validate_resource_url(&resource, true)?;
        validate_authorization_server_url(&authorization_server, true)?;
        Ok(Self {
            resource,
            authorization_server,
            federated_subjects_enabled,
        })
    }
}

#[derive(Clone)]
struct DiscoveryState {
    config: Option<TaskAuthDiscoveryConfig>,
}

#[derive(Serialize)]
struct ProtectedResourceMetadata<'a> {
    resource: &'a str,
    authorization_servers: [&'a str; 1],
    bearer_methods_supported: [&'static str; 1],
    steward_task_token_contracts: Vec<&'static str>,
}

pub fn task_auth_discovery_router(config: Option<TaskAuthDiscoveryConfig>) -> Router {
    Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource_metadata),
        )
        .with_state(DiscoveryState { config })
}

async fn protected_resource_metadata(State(state): State<DiscoveryState>) -> Response {
    let Some(config) = state.config.as_ref() else {
        return discovery_unavailable();
    };
    let mut contracts = vec![LEGACY_TASK_TOKEN_CONTRACT];
    if config.federated_subjects_enabled {
        contracts.push(FEDERATED_TASK_TOKEN_CONTRACT);
    }
    (
        StatusCode::OK,
        [(header::CACHE_CONTROL, "public, max-age=300")],
        Json(ProtectedResourceMetadata {
            resource: &config.resource,
            authorization_servers: [&config.authorization_server],
            bearer_methods_supported: ["header"],
            steward_task_token_contracts: contracts,
        }),
    )
        .into_response()
}

fn discovery_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::RETRY_AFTER, "30"),
        ],
        Json(serde_json::json!({
            "error": "task_auth.discovery_unavailable",
            "retryable": true,
        })),
    )
        .into_response()
}

pub(crate) fn valid_authorization_server_url(value: &str) -> bool {
    validate_authorization_server_url(value, false).is_ok()
}

fn validate_resource_url(value: &str, allow_loopback_http: bool) -> Result<(), String> {
    let url = validate_url(value, allow_loopback_http, "task auth resource")?;
    if url.path() != "/" {
        return Err("task auth resource must be one exact URL origin".to_owned());
    }
    Ok(())
}

fn validate_authorization_server_url(
    value: &str,
    allow_loopback_http: bool,
) -> Result<(), String> {
    validate_url(value, allow_loopback_http, "task token authorization server").map(|_| ())
}

fn validate_url(
    value: &str,
    allow_loopback_http: bool,
    description: &str,
) -> Result<reqwest::Url, String> {
    if value.is_empty()
        || value.len() > 2_048
        || value.trim() != value
        || value.ends_with('/')
        || value.chars().any(char::is_whitespace)
    {
        return Err(format!("{description} must be one bounded canonical URL"));
    }
    let url = reqwest::Url::parse(value)
        .map_err(|_| format!("{description} must be one bounded canonical URL"))?;
    let loopback_http = allow_loopback_http
        && url.scheme() == "http"
        && url.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
    if (url.scheme() != "https" && !loopback_http)
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.as_str().trim_end_matches('/') != value
    {
        return Err(format!("{description} must be one bounded canonical URL"));
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    use super::{TaskAuthDiscoveryConfig, task_auth_discovery_router};

    #[tokio::test]
    async fn discovery_advertises_exact_resource_issuer_and_enabled_contracts()
    -> Result<(), String> {
        let config = TaskAuthDiscoveryConfig::new(
            "https://steward.example.test".to_owned(),
            "https://identity.example.test".to_owned(),
            true,
        )?;
        let response = task_auth_discovery_router(Some(config))
            .oneshot(
                Request::builder()
                    .uri("/.well-known/oauth-protected-resource")
                    .body(Body::empty())
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("public, max-age=300")
        );
        let body = to_bytes(response.into_body(), 8 * 1024)
            .await
            .map_err(|error| error.to_string())?;
        let body: serde_json::Value =
            serde_json::from_slice(&body).map_err(|error| error.to_string())?;
        assert_eq!(body["resource"], "https://steward.example.test");
        assert_eq!(
            body["authorization_servers"],
            serde_json::json!(["https://identity.example.test"])
        );
        assert_eq!(
            body["steward_task_token_contracts"],
            serde_json::json!(["steward-task-v2", "steward-task-v3"])
        );
        Ok(())
    }

    #[tokio::test]
    async fn unconfigured_discovery_is_stable_retryable_and_uncacheable() -> Result<(), String> {
        let response = task_auth_discovery_router(None)
            .oneshot(
                Request::builder()
                    .uri("/.well-known/oauth-protected-resource")
                    .body(Body::empty())
                    .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response
                .headers()
                .get(header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("no-store")
        );
        let body = to_bytes(response.into_body(), 8 * 1024)
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body)
                .map_err(|error| error.to_string())?["error"],
            "task_auth.discovery_unavailable"
        );
        Ok(())
    }

    #[test]
    fn discovery_configuration_rejects_ambiguous_urls_and_limits_http_to_tests() {
        for resource in [
            "http://steward.example.test",
            "https://alice@steward.example.test",
            "https://steward.example.test/api",
            "https://steward.example.test?mode=test",
            "https://steward.example.test#fragment",
        ] {
            assert!(
                TaskAuthDiscoveryConfig::new(
                    resource.to_owned(),
                    "https://identity.example.test".to_owned(),
                    true,
                )
                .is_err(),
                "accepted invalid resource {resource}"
            );
        }
        assert!(
            TaskAuthDiscoveryConfig::new_for_test(
                "http://127.0.0.1:8443".to_owned(),
                "http://localhost:9443".to_owned(),
                true,
            )
            .is_ok()
        );
    }
}
