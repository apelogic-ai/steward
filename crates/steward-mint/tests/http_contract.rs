use std::future::{Future, ready};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use ed25519_dalek::SigningKey;
use rand_core::OsRng;
use serde_json::Value;
use steward_mint::{
    AuthorityBinding, AuthorityResolver, AuthorityState, DEFAULT_AUTHORITY_TTL, Mint, MintConfig,
    MintError, MintSigningKey, SvidAssertion, SvidValidationError, SvidValidator,
    ValidatedWorkload, router,
};
use steward_types::{Email, Principal, RuntimeId};
use tower::ServiceExt;

const WORKLOAD: &str = "spiffe://example.org/agent/runtime-a";
const VALID_FORM: &str = "grant_type=client_credentials&client_assertion_type=urn%3Aietf%3Aparams%3Aoauth%3Aclient-assertion-type%3Ajwt-spiffe&client_assertion=test-svid&audience=mcp-gw.example.test&scope=tools";

struct FixedValidator {
    calls: Arc<AtomicUsize>,
    outcome: Result<ValidatedWorkload, SvidValidationError>,
}

impl SvidValidator for FixedValidator {
    fn validate(
        &self,
        _audience: &str,
        _assertion: &SvidAssertion,
    ) -> impl Future<Output = Result<ValidatedWorkload, SvidValidationError>> + Send {
        self.calls.fetch_add(1, Ordering::SeqCst);
        ready(self.outcome.clone())
    }
}

struct FixedResolver {
    calls: Arc<AtomicUsize>,
}

impl AuthorityResolver for FixedResolver {
    fn resolve(
        &self,
        _workload: &ValidatedWorkload,
    ) -> impl Future<Output = Result<AuthorityBinding, MintError>> + Send {
        self.calls.fetch_add(1, Ordering::SeqCst);
        ready(Ok(AuthorityBinding {
            workload_id: WORKLOAD.to_owned(),
            runtime: RuntimeId("runtime-uid-a".to_owned()),
            principal: Principal::User {
                acting_user: Email("alice@example.com".to_owned()),
            },
            tools: Vec::new(),
            state: AuthorityState::Active,
        }))
    }
}

fn app(
    validator_outcome: Result<ValidatedWorkload, SvidValidationError>,
) -> Result<(axum::Router, Arc<AtomicUsize>, Arc<AtomicUsize>), String> {
    let signing_key = SigningKey::generate(&mut OsRng);
    let validator_calls = Arc::new(AtomicUsize::new(0));
    let resolver_calls = Arc::new(AtomicUsize::new(0));
    let mint = Mint::new(
        MintConfig {
            issuer: "https://mint.example.test".to_owned(),
            audience: "mcp-gw.example.test".to_owned(),
            allowed_scopes: vec!["tools".to_owned()],
            svid_audience: "https://mint.example.test".to_owned(),
            authority_ttl: DEFAULT_AUTHORITY_TTL,
        },
        MintSigningKey::from_bytes(&signing_key.to_bytes()),
        FixedValidator {
            calls: validator_calls.clone(),
            outcome: validator_outcome,
        },
        FixedResolver {
            calls: resolver_calls.clone(),
        },
    )
    .map_err(|error| format!("test mint config must be valid: {error:?}"))?;
    Ok((router(Arc::new(mint)), validator_calls, resolver_calls))
}

async fn call(
    app: axum::Router,
    method: &str,
    uri: &str,
    body: &str,
) -> Result<(StatusCode, Value), String> {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body.to_owned()))
        .map_err(|error| format!("build request: {error}"))?;
    let response = app
        .oneshot(request)
        .await
        .map_err(|error| format!("route request: {error}"))?;
    let status = response.status();
    let body = to_bytes(response.into_body(), 64 * 1024)
        .await
        .map_err(|error| format!("read response: {error}"))?;
    let json = serde_json::from_slice(&body)
        .map_err(|error| format!("OAuth responses must be JSON: {error}"))?;
    Ok((status, json))
}

#[tokio::test]
async fn openshell_client_assertion_form_returns_an_oauth_token_response() -> Result<(), String> {
    let (app, validator_calls, resolver_calls) = app(Ok(ValidatedWorkload {
        spiffe_id: WORKLOAD.to_owned(),
    }))?;

    let (status, body) = call(app, "POST", "/token", VALID_FORM).await?;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["token_type"], "Bearer");
    assert_eq!(body["expires_in"], 60);
    assert_eq!(body["scope"], "tools");
    assert!(
        body["access_token"]
            .as_str()
            .is_some_and(|token| token.split('.').count() == 3),
        "successful exchange must return a compact JWT"
    );
    assert_eq!(validator_calls.load(Ordering::SeqCst), 1);
    assert_eq!(resolver_calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn unsupported_grant_fails_before_reading_the_svid() -> Result<(), String> {
    let (app, validator_calls, resolver_calls) = app(Ok(ValidatedWorkload {
        spiffe_id: WORKLOAD.to_owned(),
    }))?;
    let form = VALID_FORM.replace("grant_type=client_credentials", "grant_type=password");

    let (status, body) = call(app, "POST", "/token", &form).await?;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "unsupported_grant_type");
    assert_eq!(validator_calls.load(Ordering::SeqCst), 0);
    assert_eq!(resolver_calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn unconfigured_scope_fails_before_reading_the_svid() -> Result<(), String> {
    let (app, validator_calls, resolver_calls) = app(Ok(ValidatedWorkload {
        spiffe_id: WORKLOAD.to_owned(),
    }))?;
    let form = VALID_FORM.replace("scope=tools", "scope=admin");

    let (status, body) = call(app, "POST", "/token", &form).await?;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_scope");
    assert_eq!(validator_calls.load(Ordering::SeqCst), 0);
    assert_eq!(resolver_calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn rejected_svid_returns_a_sanitized_invalid_client_error() -> Result<(), String> {
    let (app, validator_calls, resolver_calls) = app(Err(SvidValidationError::Rejected))?;

    let (status, body) = call(app, "POST", "/token", VALID_FORM).await?;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "invalid_client");
    assert!(
        !body.to_string().contains("test-svid"),
        "an OAuth error must not echo the JWT-SVID"
    );
    assert_eq!(validator_calls.load(Ordering::SeqCst), 1);
    assert_eq!(resolver_calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn malformed_form_returns_a_json_invalid_request_without_validation() -> Result<(), String> {
    let (app, validator_calls, resolver_calls) = app(Ok(ValidatedWorkload {
        spiffe_id: WORKLOAD.to_owned(),
    }))?;
    let form = "grant_type=client_credentials&audience=mcp-gw.example.test";

    let (status, body) = call(app, "POST", "/token", form).await?;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_request");
    assert_eq!(validator_calls.load(Ordering::SeqCst), 0);
    assert_eq!(resolver_calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn unavailable_spire_returns_a_retryable_sanitized_error() -> Result<(), String> {
    let (app, validator_calls, resolver_calls) = app(Err(SvidValidationError::Unavailable))?;

    let (status, body) = call(app, "POST", "/token", VALID_FORM).await?;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"], "temporarily_unavailable");
    assert_eq!(validator_calls.load(Ordering::SeqCst), 1);
    assert_eq!(resolver_calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn jwks_route_publishes_the_active_public_key() -> Result<(), String> {
    let (app, _, _) = app(Ok(ValidatedWorkload {
        spiffe_id: WORKLOAD.to_owned(),
    }))?;

    let (status, body) = call(app, "GET", "/.well-known/jwks.json", "").await?;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["keys"][0]["alg"], "EdDSA");
    assert!(body["keys"][0].get("d").is_none());
    Ok(())
}
