//! Production MCP-GW provider-connection broker.
//!
//! The browser session is never forwarded to MCP-GW.  A deployment supplies a
//! reviewed issuer which derives one short-lived HOP-1 bearer for the exact
//! canonical browser principal and opaque browser-session binding.  This adapter
//! then uses that bearer only for MCP-GW's private provider-control endpoints.

use std::collections::BTreeSet;
use std::hash::Hash;
use std::time::Duration;

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use reqwest::{Client, Method, Url};
use serde::Deserialize;

use crate::BoxFuture;
use crate::connections::{
    AuthorizationUrl, ConnectionBrokerError, ConnectionContinuation, ConnectionPhase,
    ConnectionSession, ProviderConnectionBroker, ProviderConnectionStatus,
};

const STATUS_PATH: &str = "/oauth/github/status";
const START_PATH: &str = "/oauth/github/start";
const DISCONNECT_PATH: &str = "/oauth/github/disconnect";
const MAX_RESPONSE_BYTES: usize = 32 * 1024;
const MAX_SCOPES: usize = 32;
const MAX_SCOPE_LENGTH: usize = 128;

/// An opaque short-lived bearer issued for one browser connection operation.
///
/// This type intentionally implements neither `Debug` nor `Display`, so a
/// bearer cannot leak through ordinary diagnostics or browser responses.
pub struct McpGwBearer(HeaderValue);

impl McpGwBearer {
    pub fn new(value: String) -> Result<Self, &'static str> {
        if value.is_empty() || value.len() > 8 * 1024 || value.chars().any(char::is_whitespace) {
            return Err("MCP-GW bearer must be a bounded non-whitespace value");
        }
        let value = HeaderValue::from_str(&format!("Bearer {value}"))
            .map_err(|_| "MCP-GW bearer must be valid HTTP header material")?;
        Ok(Self(value))
    }

    fn header_value(&self) -> HeaderValue {
        self.0.clone()
    }
}

/// Issues a bearer for MCP-GW's HOP-1 boundary from the currently authenticated
/// browser identity.  Implementations must reject a session that does not map
/// to the exact canonical principal; this adapter never accepts a configured,
/// ambient, or user-supplied bearer.
pub trait BrowserMcpGwBearerIssuer<B>: Clone + Send + Sync + 'static
where
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    fn issue<'a>(
        &'a self,
        session: &'a ConnectionSession<B>,
    ) -> BoxFuture<'a, Result<McpGwBearer, ConnectionBrokerError>>;
}

#[derive(Clone)]
pub struct McpGwConnectionsConfig {
    origin: Url,
    redirect_after: Url,
}

impl McpGwConnectionsConfig {
    /// Build the exact private MCP-GW origin and the browser destination to
    /// which MCP-GW redirects only after its provider callback has completed.
    ///
    /// `http://127.0.0.1` is allowed exclusively for the local testbed's
    /// loopback adapter. All non-loopback deployments require HTTPS.
    pub fn new(mcp_gateway_origin: String, browser_origin: String) -> Result<Self, String> {
        let origin = validate_origin(&mcp_gateway_origin, "MCP-GW")?;
        let mut redirect_after = validate_origin(&browser_origin, "browser")?;
        redirect_after.set_path("/connections");
        redirect_after.set_query(None);
        redirect_after.set_fragment(Some("github-connected"));
        Ok(Self {
            origin,
            redirect_after,
        })
    }
}

#[derive(Clone)]
pub struct McpGwConnectionsBroker<B, I> {
    config: McpGwConnectionsConfig,
    client: Client,
    issuer: I,
    binding: std::marker::PhantomData<fn() -> B>,
}

impl<B, I> McpGwConnectionsBroker<B, I>
where
    B: Clone + Eq + Hash + Send + Sync + 'static,
    I: BrowserMcpGwBearerIssuer<B>,
{
    pub fn new(config: McpGwConnectionsConfig, issuer: I) -> Result<Self, String> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| "build bounded MCP-GW Connections client".to_owned())?;
        Ok(Self {
            config,
            client,
            issuer,
            binding: std::marker::PhantomData,
        })
    }

    async fn request(
        &self,
        session: &ConnectionSession<B>,
        method: Method,
        path: &'static str,
        body: Option<serde_json::Value>,
    ) -> Result<(u16, Vec<u8>), ConnectionBrokerError> {
        let bearer = self.issuer.issue(session).await?;
        let target = endpoint(&self.config.origin, path)?;
        let request = self
            .client
            .request(method, target)
            .header(AUTHORIZATION, bearer.header_value());
        let request = if let Some(body) = body {
            request.header(CONTENT_TYPE, "application/json").json(&body)
        } else {
            request
        };
        let response = request
            .send()
            .await
            .map_err(|_| ConnectionBrokerError::Unavailable)?;
        let status = response.status().as_u16();
        let bytes = response
            .bytes()
            .await
            .map_err(|_| ConnectionBrokerError::Unavailable)?;
        if bytes.len() > MAX_RESPONSE_BYTES {
            return Err(ConnectionBrokerError::Unavailable);
        }
        Ok((status, bytes.to_vec()))
    }
}

impl<B, I> ProviderConnectionBroker<B> for McpGwConnectionsBroker<B, I>
where
    B: Clone + Eq + Hash + Send + Sync + 'static,
    I: BrowserMcpGwBearerIssuer<B>,
{
    fn status<'a>(
        &'a self,
        session: &'a ConnectionSession<B>,
    ) -> BoxFuture<'a, Result<ProviderConnectionStatus, ConnectionBrokerError>> {
        Box::pin(async move {
            let (status, body) = self
                .request(session, Method::GET, STATUS_PATH, None)
                .await?;
            if status != 200 {
                return Err(ConnectionBrokerError::Unavailable);
            }
            let status: GithubStatus =
                serde_json::from_slice(&body).map_err(|_| ConnectionBrokerError::Unavailable)?;
            status.into_provider_status()
        })
    }

    fn start<'a>(
        &'a self,
        session: &'a ConnectionSession<B>,
    ) -> BoxFuture<'a, Result<AuthorizationUrl, ConnectionBrokerError>> {
        Box::pin(async move {
            let (status, body) = self
                .request(
                    session,
                    Method::POST,
                    START_PATH,
                    Some(serde_json::json!({
                        "redirectAfter": self.config.redirect_after.as_str(),
                    })),
                )
                .await?;
            if status != 200 {
                return Err(ConnectionBrokerError::Unavailable);
            }
            let started: GithubStart =
                serde_json::from_slice(&body).map_err(|_| ConnectionBrokerError::Unavailable)?;
            AuthorizationUrl::new(started.authorization_url)
                .map_err(|_| ConnectionBrokerError::Unavailable)
        })
    }

    fn complete<'a>(
        &'a self,
        _session: &'a ConnectionSession<B>,
        _continuation: &'a ConnectionContinuation,
    ) -> BoxFuture<'a, Result<(), ConnectionBrokerError>> {
        // MCP-GW owns and verifies the provider callback. It redirects directly
        // to the fragment-only browser destination above; Steward never accepts
        // a provider continuation through its UI or query string.
        Box::pin(async { Err(ConnectionBrokerError::InvalidOrExpiredContinuation) })
    }

    fn disconnect<'a>(
        &'a self,
        session: &'a ConnectionSession<B>,
    ) -> BoxFuture<'a, Result<(), ConnectionBrokerError>> {
        Box::pin(async move {
            let (status, body) = self
                .request(session, Method::POST, DISCONNECT_PATH, None)
                .await?;
            if status != 204 || !body.is_empty() {
                return Err(ConnectionBrokerError::Unavailable);
            }
            Ok(())
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GithubStatus {
    connected: bool,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    scopes_required: Vec<String>,
    #[serde(default)]
    scopes_granted: Vec<String>,
    #[serde(default)]
    missing_scopes: Vec<String>,
}

impl GithubStatus {
    fn into_provider_status(self) -> Result<ProviderConnectionStatus, ConnectionBrokerError> {
        validate_scopes(&self.scopes_required)?;
        validate_scopes(&self.scopes_granted)?;
        validate_scopes(&self.missing_scopes)?;
        let email = self
            .email
            .filter(|email| !email.is_empty())
            .map(|email| {
                steward_types::Email::parse(email).map_err(|_| ConnectionBrokerError::Unavailable)
            })
            .transpose()?
            .map(|email| email.0);
        if self.connected && (email.is_none() || !self.missing_scopes.is_empty()) {
            return Err(ConnectionBrokerError::Unavailable);
        }
        let phase = if self.connected {
            ConnectionPhase::Connected
        } else if email.is_some() && !self.missing_scopes.is_empty() {
            ConnectionPhase::ReauthRequired
        } else {
            ConnectionPhase::Disconnected
        };
        Ok(ProviderConnectionStatus {
            phase,
            account_email: email,
            scopes_required: self.scopes_required,
            scopes_granted: self.scopes_granted,
            scopes_missing: self.missing_scopes,
            expires_at: None,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GithubStart {
    authorization_url: String,
}

fn validate_scopes(scopes: &[String]) -> Result<(), ConnectionBrokerError> {
    if scopes.len() > MAX_SCOPES
        || scopes
            .iter()
            .any(|scope| scope.is_empty() || scope.len() > MAX_SCOPE_LENGTH)
        || scopes.iter().collect::<BTreeSet<_>>().len() != scopes.len()
    {
        return Err(ConnectionBrokerError::Unavailable);
    }
    Ok(())
}

fn validate_origin(value: &str, name: &str) -> Result<Url, String> {
    let origin = Url::parse(value).map_err(|_| format!("{name} origin must be a URL"))?;
    let loopback_http = origin.scheme() == "http" && origin.host_str() == Some("127.0.0.1");
    if origin.scheme() != "https" && !loopback_http {
        return Err(format!(
            "{name} origin must use HTTPS except explicit 127.0.0.1 loopback"
        ));
    }
    if origin.host_str().is_none()
        || origin.port_or_known_default().is_none()
        || !origin.username().is_empty()
        || origin.password().is_some()
        || origin.path() != "/"
        || origin.query().is_some()
        || origin.fragment().is_some()
    {
        return Err(format!("{name} origin must be an exact origin"));
    }
    Ok(origin)
}

fn endpoint(origin: &Url, path: &'static str) -> Result<Url, ConnectionBrokerError> {
    origin
        .join(path)
        .map_err(|_| ConnectionBrokerError::Unavailable)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::body::{Body, to_bytes};
    use axum::extract::{Request, State};
    use axum::http::{Response, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::any;
    use axum::{Json, Router};
    use steward_types::CanonicalUserId;
    use tokio::net::TcpListener;

    use super::*;
    use crate::connections::ConnectionSubject;

    #[derive(Clone, Eq, Hash, PartialEq)]
    struct TestBinding(&'static str);

    #[derive(Clone, Default)]
    struct TestIssuer {
        calls: Arc<Mutex<Vec<(CanonicalUserId, TestBinding)>>>,
    }

    impl BrowserMcpGwBearerIssuer<TestBinding> for TestIssuer {
        fn issue<'a>(
            &'a self,
            session: &'a ConnectionSession<TestBinding>,
        ) -> BoxFuture<'a, Result<McpGwBearer, ConnectionBrokerError>> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .map_err(|_| ConnectionBrokerError::Unavailable)?
                    .push((
                        session.subject.canonical_user_id.clone(),
                        session.binding.clone(),
                    ));
                McpGwBearer::new("opaque-test-bearer".to_owned())
                    .map_err(|_| ConnectionBrokerError::Unavailable)
            })
        }
    }

    #[derive(Clone, Default)]
    struct GatewayState {
        requests: GatewayRequests,
    }

    type GatewayRequest = (String, String, Option<String>, Vec<u8>);
    type GatewayRequests = Arc<Mutex<Vec<GatewayRequest>>>;

    async fn gateway(State(state): State<GatewayState>, request: Request) -> Response<Body> {
        let (parts, body) = request.into_parts();
        let body = match to_bytes(body, MAX_RESPONSE_BYTES).await {
            Ok(body) => body.to_vec(),
            Err(_) => {
                return Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(Body::empty())
                    .unwrap_or_else(|_| Response::new(Body::empty()));
            }
        };
        let path = parts.uri.path().to_owned();
        let method = parts.method.to_string();
        let authorization = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        if let Ok(mut requests) = state.requests.lock() {
            requests.push((method.clone(), path.clone(), authorization, body));
        }
        match (method.as_str(), path.as_str()) {
            ("GET", STATUS_PATH) => Json(serde_json::json!({
                "connected": false,
                "scopesRequired": ["repo"],
                "scopesGranted": [],
                "missingScopes": ["repo"],
            }))
            .into_response(),
            ("POST", START_PATH) => Json(serde_json::json!({
                "authorizationUrl": "https://github.test/login/oauth/authorize?state=opaque"
            }))
            .into_response(),
            ("POST", DISCONNECT_PATH) => Response::builder()
                .status(StatusCode::NO_CONTENT)
                .body(Body::empty())
                .unwrap_or_else(|_| Response::new(Body::empty())),
            _ => Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap_or_else(|_| Response::new(Body::empty())),
        }
    }

    fn session() -> Result<ConnectionSession<TestBinding>, String> {
        Ok(ConnectionSession {
            subject: ConnectionSubject {
                canonical_user_id: CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?,
                display_email: "alice@example.com".to_owned(),
            },
            binding: TestBinding("browser-session-a"),
        })
    }

    #[tokio::test]
    async fn broker_binds_every_gateway_request_to_the_browser_canonical_principal()
    -> Result<(), String> {
        let state = GatewayState::default();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|error| format!("bind MCP-GW test server: {error}"))?;
        let address = listener
            .local_addr()
            .map_err(|error| format!("read MCP-GW test server address: {error}"))?;
        let app = Router::new()
            .fallback(any(gateway))
            .with_state(state.clone());
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let issuer = TestIssuer::default();
        let broker = McpGwConnectionsBroker::new(
            McpGwConnectionsConfig::new(
                format!("http://127.0.0.1:{}/", address.port()),
                format!("http://127.0.0.1:{}/", address.port()),
            )?,
            issuer.clone(),
        )?;
        let session = session()?;

        let status = broker
            .status(&session)
            .await
            .map_err(|error| format!("read MCP-GW status: {error:?}"))?;
        assert_eq!(status.phase, ConnectionPhase::Disconnected);
        let authorization = broker
            .start(&session)
            .await
            .map_err(|error| format!("start MCP-GW OAuth: {error:?}"))?;
        assert_eq!(
            authorization.as_str(),
            "https://github.test/login/oauth/authorize?state=opaque"
        );
        let rejected_callback = broker
            .complete(
                &session,
                &ConnectionContinuation::new("browser-visible-continuation".to_owned())
                    .map_err(str::to_owned)?,
            )
            .await;
        assert_eq!(
            rejected_callback,
            Err(ConnectionBrokerError::InvalidOrExpiredContinuation),
            "Steward must not accept a provider continuation from the browser"
        );
        broker
            .disconnect(&session)
            .await
            .map_err(|error| format!("disconnect MCP-GW OAuth: {error:?}"))?;

        let requests = state
            .requests
            .lock()
            .map_err(|_| "read MCP-GW test requests".to_owned())?
            .clone();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests
                .iter()
                .map(|(method, path, _, _)| (method.as_str(), path.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("GET", STATUS_PATH),
                ("POST", START_PATH),
                ("POST", DISCONNECT_PATH),
            ]
        );
        assert!(requests.iter().all(|(_, _, authorization, _)| {
            authorization.as_deref() == Some("Bearer opaque-test-bearer")
        }));
        let start_body: serde_json::Value = serde_json::from_slice(&requests[1].3)
            .map_err(|error| format!("parse MCP-GW start body: {error}"))?;
        assert_eq!(
            start_body["redirectAfter"],
            format!(
                "http://127.0.0.1:{}/connections#github-connected",
                address.port()
            )
        );
        let calls = issuer
            .calls
            .lock()
            .map_err(|_| "read browser bearer issuer calls".to_owned())?
            .clone();
        assert_eq!(calls.len(), 3);
        assert!(calls.iter().all(|(canonical, binding)| {
            canonical == &session.subject.canonical_user_id && binding == &session.binding
        }));
        assert!(
            !format!("{:?}", ConnectionBrokerError::Unavailable).contains("opaque-test-bearer"),
            "the public broker error cannot carry bearer material"
        );
        server.abort();
        Ok(())
    }

    #[test]
    fn configuration_rejects_non_loopback_http_and_ambiguous_origins() {
        for value in [
            "http://gateway.example.test:8080/",
            "https://gateway.example.test:443/prefix",
            "https://user@gateway.example.test:443/",
            "https://gateway.example.test:443/?query=value",
        ] {
            assert!(
                McpGwConnectionsConfig::new(
                    value.to_owned(),
                    "https://steward.example.test:443/".to_owned()
                )
                .is_err(),
                "accepted invalid MCP-GW origin {value}"
            );
        }
    }
}
