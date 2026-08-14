use std::future::{Future, ready};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use ed25519_dalek::SigningKey;
use rand_core::OsRng;
use serde_json::Value;
use steward_mint::{
    AuthorityBinding, AuthorityResolver, AuthorityState, DEFAULT_AUTHORITY_TTL,
    IntrospectionClientCredential, Mint, MintConfig, MintError, MintSigningKey, SvidAssertion,
    SvidValidationError, SvidValidator, ValidatedWorkload, router,
};
use steward_types::{CanonicalAuthorityBinding, CanonicalUserId, Email, Principal, RuntimeId};
use tower::ServiceExt;

const WORKLOAD: &str = "spiffe://example.org/agent/runtime-a";
const CANONICAL_USER: &str = "usr_0123456789abcdef0123456789abcdef";
const OTHER_CANONICAL_USER: &str = "usr_abcdef0123456789abcdef0123456789";
const VALID_FORM: &str = "grant_type=client_credentials&client_assertion_type=urn%3Aietf%3Aparams%3Aoauth%3Aclient-assertion-type%3Ajwt-spiffe&client_assertion=test-svid&audience=mcp-gw.example.test&scope=tools";

fn person_authority() -> Result<CanonicalAuthorityBinding, String> {
    person_authority_for(CANONICAL_USER)
}

fn person_authority_for(value: &str) -> Result<CanonicalAuthorityBinding, String> {
    let user_id = CanonicalUserId::parse(value)
        .map_err(|_| "canonical-user fixture must satisfy the reviewed wire format".to_owned())?;
    CanonicalAuthorityBinding::new(user_id.clone(), Some(user_id))
        .map_err(|_| "person-authority fixture must satisfy the reviewed invariant".to_owned())
}

fn active_user_binding() -> Result<AuthorityBinding, String> {
    Ok(AuthorityBinding {
        workload_id: WORKLOAD.to_owned(),
        runtime: RuntimeId("runtime-uid-a".to_owned()),
        runtime_namespace: "team-a".to_owned(),
        principal: Principal::User {
            acting_user: Email("alice@example.com".to_owned()),
        },
        canonical_authority: Some(person_authority()?),
        tools: Vec::new(),
        state: AuthorityState::Active,
    })
}

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
        ready(
            person_authority()
                .map_err(|_| MintError::AuthorityUnavailable)
                .map(|canonical_authority| AuthorityBinding {
                    workload_id: WORKLOAD.to_owned(),
                    runtime: RuntimeId("runtime-uid-a".to_owned()),
                    runtime_namespace: "team-a".to_owned(),
                    principal: Principal::User {
                        acting_user: Email("alice@example.com".to_owned()),
                    },
                    canonical_authority: Some(canonical_authority),
                    tools: Vec::new(),
                    state: AuthorityState::Active,
                }),
        )
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
            introspection_client_credential: IntrospectionClientCredential::new(
                "gateway-credential".to_owned(),
            ),
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
) -> Result<(StatusCode, HeaderMap, Value), String> {
    call_with_authorization(app, method, uri, body, None).await
}

async fn call_as_gateway(
    app: axum::Router,
    method: &str,
    uri: &str,
    body: &str,
) -> Result<(StatusCode, HeaderMap, Value), String> {
    call_with_authorization(app, method, uri, body, Some("Bearer gateway-credential")).await
}

async fn call_with_authorization(
    app: axum::Router,
    method: &str,
    uri: &str,
    body: &str,
    authorization: Option<&str>,
) -> Result<(StatusCode, HeaderMap, Value), String> {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded");
    if let Some(authorization) = authorization {
        request = request.header("authorization", authorization);
    }
    let request = request
        .body(Body::from(body.to_owned()))
        .map_err(|error| format!("build request: {error}"))?;
    let response = app
        .oneshot(request)
        .await
        .map_err(|error| format!("route request: {error}"))?;
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), 64 * 1024)
        .await
        .map_err(|error| format!("read response: {error}"))?;
    let json = serde_json::from_slice(&body)
        .map_err(|error| format!("OAuth responses must be JSON: {error}"))?;
    Ok((status, headers, json))
}

#[tokio::test]
async fn openshell_client_assertion_form_returns_an_oauth_token_response() -> Result<(), String> {
    let (app, validator_calls, resolver_calls) = app(Ok(ValidatedWorkload {
        spiffe_id: WORKLOAD.to_owned(),
    }))?;

    let (status, headers, body) = call(app, "POST", "/token", VALID_FORM).await?;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(headers["pragma"], "no-cache");
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

    let (status, _, body) = call(app, "POST", "/token", &form).await?;

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

    let (status, _, body) = call(app, "POST", "/token", &form).await?;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_scope");
    assert_eq!(validator_calls.load(Ordering::SeqCst), 0);
    assert_eq!(resolver_calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn rejected_svid_returns_a_sanitized_invalid_client_error() -> Result<(), String> {
    let (app, validator_calls, resolver_calls) = app(Err(SvidValidationError::Rejected))?;

    let (status, _, body) = call(app, "POST", "/token", VALID_FORM).await?;

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

    let (status, _, body) = call(app, "POST", "/token", form).await?;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_request");
    assert_eq!(validator_calls.load(Ordering::SeqCst), 0);
    assert_eq!(resolver_calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn unavailable_spire_returns_a_retryable_sanitized_error() -> Result<(), String> {
    let (app, validator_calls, resolver_calls) = app(Err(SvidValidationError::Unavailable))?;

    let (status, _, body) = call(app, "POST", "/token", VALID_FORM).await?;

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

    let (status, _, body) = call(app, "GET", "/.well-known/jwks.json", "").await?;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["keys"][0]["alg"], "EdDSA");
    assert!(body["keys"][0].get("d").is_none());
    Ok(())
}

struct RevocableResolver {
    state: Arc<AtomicU8>,
}

struct MutableResolver {
    binding: Arc<Mutex<AuthorityBinding>>,
}

impl AuthorityResolver for MutableResolver {
    fn resolve(
        &self,
        _workload: &ValidatedWorkload,
    ) -> impl Future<Output = Result<AuthorityBinding, MintError>> + Send {
        ready(
            self.binding
                .lock()
                .map(|binding| binding.clone())
                .map_err(|_| MintError::AuthorityUnavailable),
        )
    }
}

impl AuthorityResolver for RevocableResolver {
    fn resolve(
        &self,
        _workload: &ValidatedWorkload,
    ) -> impl Future<Output = Result<AuthorityBinding, MintError>> + Send {
        let state = if self.state.load(Ordering::SeqCst) == 0 {
            AuthorityState::Active
        } else {
            AuthorityState::Revoked
        };
        ready(
            person_authority()
                .map_err(|_| MintError::AuthorityUnavailable)
                .map(|canonical_authority| AuthorityBinding {
                    workload_id: WORKLOAD.to_owned(),
                    runtime: RuntimeId("runtime-uid-a".to_owned()),
                    runtime_namespace: "team-a".to_owned(),
                    principal: Principal::User {
                        acting_user: Email("alice@example.com".to_owned()),
                    },
                    canonical_authority: Some(canonical_authority),
                    tools: Vec::new(),
                    state,
                }),
        )
    }
}

#[tokio::test]
async fn already_issued_hop1_is_inactive_immediately_after_revocation() -> Result<(), String> {
    let signing_key = SigningKey::generate(&mut OsRng);
    let authority_state = Arc::new(AtomicU8::new(0));
    let mint = Mint::new(
        MintConfig {
            issuer: "https://mint.example.test".to_owned(),
            audience: "mcp-gw.example.test".to_owned(),
            allowed_scopes: vec!["tools".to_owned()],
            svid_audience: "https://mint.example.test".to_owned(),
            authority_ttl: DEFAULT_AUTHORITY_TTL,
            introspection_client_credential: IntrospectionClientCredential::new(
                "gateway-credential".to_owned(),
            ),
        },
        MintSigningKey::from_bytes(&signing_key.to_bytes()),
        FixedValidator {
            calls: Arc::new(AtomicUsize::new(0)),
            outcome: Ok(ValidatedWorkload {
                spiffe_id: WORKLOAD.to_owned(),
            }),
        },
        RevocableResolver {
            state: authority_state.clone(),
        },
    )
    .map_err(|error| format!("test mint config must be valid: {error:?}"))?;
    let app = router(Arc::new(mint));
    let (status, _, token_response) = call(app.clone(), "POST", "/token", VALID_FORM).await?;
    if status != StatusCode::OK {
        return Err(format!("token exchange failed with {status}"));
    }
    let token = token_response["access_token"]
        .as_str()
        .ok_or_else(|| "token exchange must return an access token".to_owned())?;
    let form = format!("token={token}");
    let (status, headers, introspection) =
        call_as_gateway(app.clone(), "POST", "/introspect", &form).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(
        introspection["active"], true,
        "the online check must accept a current issued HOP-1 before revocation"
    );

    authority_state.store(1, Ordering::SeqCst);
    let (status, _, introspection) = call_as_gateway(app, "POST", "/introspect", &form).await?;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        introspection["active"], false,
        "a consumer check must reject an issued HOP-1 immediately after revocation"
    );
    Ok(())
}

#[tokio::test]
async fn introspection_uses_stable_canonical_subject_not_mutable_email() -> Result<(), String> {
    let signing_key = SigningKey::generate(&mut OsRng);
    let authority = Arc::new(Mutex::new(active_user_binding()?));
    let mint = Mint::new(
        MintConfig {
            issuer: "https://mint.example.test".to_owned(),
            audience: "mcp-gw.example.test".to_owned(),
            allowed_scopes: vec!["tools".to_owned()],
            svid_audience: "https://mint.example.test".to_owned(),
            authority_ttl: DEFAULT_AUTHORITY_TTL,
            introspection_client_credential: IntrospectionClientCredential::new(
                "gateway-credential".to_owned(),
            ),
        },
        MintSigningKey::from_bytes(&signing_key.to_bytes()),
        FixedValidator {
            calls: Arc::new(AtomicUsize::new(0)),
            outcome: Ok(ValidatedWorkload {
                spiffe_id: WORKLOAD.to_owned(),
            }),
        },
        MutableResolver {
            binding: authority.clone(),
        },
    )
    .map_err(|error| format!("test mint config must be valid: {error:?}"))?;
    let app = router(Arc::new(mint));
    let (status, _, token_response) = call(app.clone(), "POST", "/token", VALID_FORM).await?;
    if status != StatusCode::OK {
        return Err(format!("token exchange failed with {status}"));
    }
    let token = token_response["access_token"]
        .as_str()
        .ok_or_else(|| "token exchange must return an access token".to_owned())?;
    let form = format!("token={token}");

    {
        let mut current = authority
            .lock()
            .map_err(|_| "test authority mutex poisoned".to_owned())?;
        current.principal = Principal::User {
            acting_user: Email("alice-renamed@example.com".to_owned()),
        };
    }
    let (status, _, introspection) =
        call_as_gateway(app.clone(), "POST", "/introspect", &form).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        introspection["active"], true,
        "verified email rename must not revoke a stable canonical subject"
    );

    {
        let mut current = authority
            .lock()
            .map_err(|_| "test authority mutex poisoned".to_owned())?;
        current.canonical_authority = None;
    }
    let (status, _, introspection) =
        call_as_gateway(app.clone(), "POST", "/introspect", &form).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        introspection["active"], false,
        "removing person-bound canonical authority must immediately invalidate issued HOP-1"
    );

    {
        let mut current = authority
            .lock()
            .map_err(|_| "test authority mutex poisoned".to_owned())?;
        current.canonical_authority = Some(person_authority_for(OTHER_CANONICAL_USER)?);
    }
    let (status, _, introspection) = call_as_gateway(app, "POST", "/introspect", &form).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        introspection["active"], false,
        "a different canonical authority must immediately invalidate issued HOP-1"
    );
    Ok(())
}

#[tokio::test]
async fn introspection_requires_gateway_authentication_before_authority_lookup()
-> Result<(), String> {
    let (app, _, resolver_calls) = app(Ok(ValidatedWorkload {
        spiffe_id: WORKLOAD.to_owned(),
    }))?;
    let (status, _, token_response) = call(app.clone(), "POST", "/token", VALID_FORM).await?;
    if status != StatusCode::OK {
        return Err(format!("token exchange failed with {status}"));
    }
    let token = token_response["access_token"]
        .as_str()
        .ok_or_else(|| "token exchange must return an access token".to_owned())?;

    let form = format!("token={token}");
    let (status, _, body) = call(app.clone(), "POST", "/introspect", &form).await?;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "invalid_client");
    let workload_authorization = format!("Bearer {token}");
    let (status, _, body) = call_with_authorization(
        app,
        "POST",
        "/introspect",
        &form,
        Some(&workload_authorization),
    )
    .await?;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "invalid_client");
    assert_eq!(
        resolver_calls.load(Ordering::SeqCst),
        1,
        "only the configured gateway client may trigger authority resolution"
    );
    Ok(())
}

#[tokio::test]
async fn introspection_accepts_case_insensitive_bearer_with_multiple_spaces() -> Result<(), String>
{
    let (app, _, _) = app(Ok(ValidatedWorkload {
        spiffe_id: WORKLOAD.to_owned(),
    }))?;
    let (status, _, token_response) = call(app.clone(), "POST", "/token", VALID_FORM).await?;
    if status != StatusCode::OK {
        return Err(format!("token exchange failed with {status}"));
    }
    let token = token_response["access_token"]
        .as_str()
        .ok_or_else(|| "token exchange must return an access token".to_owned())?;

    let form = format!("token={token}");
    let (status, _, introspection) = call_with_authorization(
        app,
        "POST",
        "/introspect",
        &form,
        Some("bearer  gateway-credential"),
    )
    .await?;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(introspection["active"], true);
    Ok(())
}
