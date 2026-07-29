use std::future::{Future, ready};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::Duration;
use ed25519_dalek::SigningKey;
use jwt_compact::alg::Ed25519;
use jwt_compact::prelude::{TimeOptions, UntrustedToken};
use jwt_compact::{AlgorithmExt, Token};
use rand_core::OsRng;
use serde::Deserialize;
use steward_mint::{
    AuthorityBinding, AuthorityResolver, AuthorityState, CredentialGrant, CredentialGrantResolver,
    DEFAULT_AUTHORITY_TTL, IntrospectionClientCredential, Mint, MintConfig, MintConfigError,
    MintError, MintSigningKey, OpaqueAccessToken, SPIFFE_CLIENT_ASSERTION_TYPE, SvidAssertion,
    SvidValidationError, SvidValidator, TokenGrantRequest, ValidatedWorkload,
};
use steward_types::{Email, Principal, RuntimeId, ToolGrant};

const EXPECTED_WORKLOAD: &str = "spiffe://example.org/agent/runtime-a";

struct FixedValidator {
    outcome: Result<ValidatedWorkload, SvidValidationError>,
}

impl SvidValidator for FixedValidator {
    fn validate(
        &self,
        _audience: &str,
        _assertion: &SvidAssertion,
    ) -> impl Future<Output = Result<ValidatedWorkload, SvidValidationError>> + Send {
        ready(self.outcome.clone())
    }
}

struct FixedResolver {
    calls: Arc<AtomicUsize>,
    outcome: Result<AuthorityBinding, MintError>,
}

type TestMint = Mint<FixedValidator, FixedResolver>;
type MintFixture = (TestMint, Arc<AtomicUsize>, ed25519_dalek::VerifyingKey);

impl AuthorityResolver for FixedResolver {
    fn resolve(
        &self,
        _workload: &ValidatedWorkload,
    ) -> impl Future<Output = Result<AuthorityBinding, MintError>> + Send {
        self.calls.fetch_add(1, Ordering::SeqCst);
        ready(self.outcome.clone())
    }
}

struct FixedCredentialResolver {
    calls: Arc<AtomicUsize>,
    token: Option<&'static str>,
}

impl CredentialGrantResolver for FixedCredentialResolver {
    async fn resolve(
        &self,
        scope: &[String],
        _authority: &AuthorityBinding,
    ) -> Result<CredentialGrant, MintError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if scope == ["inference"] {
            self.token
                .map(|token| {
                    OpaqueAccessToken::new(token.to_owned())
                        .map(CredentialGrant::AccessToken)
                        .map_err(|_| MintError::CredentialUnavailable)
                })
                .unwrap_or(Err(MintError::CredentialUnavailable))
        } else {
            Ok(CredentialGrant::NotHandled)
        }
    }
}

fn active_binding() -> AuthorityBinding {
    AuthorityBinding {
        workload_id: EXPECTED_WORKLOAD.to_owned(),
        runtime: RuntimeId("runtime-uid-a".to_owned()),
        runtime_namespace: "team-a".to_owned(),
        principal: Principal::User {
            acting_user: Email("alice@example.com".to_owned()),
        },
        tools: Vec::new(),
        state: AuthorityState::Active,
    }
}

fn binding_with_tool() -> AuthorityBinding {
    let mut binding = active_binding();
    binding.tools.push(ToolGrant {
        provider: "github".to_owned(),
        resource: "repositories".to_owned(),
        action: "list".to_owned(),
    });
    binding
}

fn request() -> TokenGrantRequest {
    TokenGrantRequest {
        grant_type: "client_credentials".to_owned(),
        client_assertion_type: SPIFFE_CLIENT_ASSERTION_TYPE.to_owned(),
        client_assertion: SvidAssertion::new("test-svid".to_owned()),
        audience: "mcp-gw.example.test".to_owned(),
        scope: vec!["tools".to_owned()],
    }
}

fn mint(
    validator_outcome: Result<ValidatedWorkload, SvidValidationError>,
    resolver_outcome: Result<AuthorityBinding, MintError>,
) -> Result<MintFixture, String> {
    let signing_key = SigningKey::generate(&mut OsRng);
    let verifying_key = signing_key.verifying_key();
    let calls = Arc::new(AtomicUsize::new(0));
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
            outcome: validator_outcome,
        },
        FixedResolver {
            calls: calls.clone(),
            outcome: resolver_outcome,
        },
    )
    .map_err(|error| format!("test mint config must be valid: {error:?}"))?;
    Ok((mint, calls, verifying_key))
}

fn mint_for_inference_without_credential_resolver() -> Result<TestMint, String> {
    let signing_key = SigningKey::generate(&mut OsRng);
    Mint::new(
        MintConfig {
            issuer: "https://mint.example.test".to_owned(),
            audience: "mcp-gw.example.test".to_owned(),
            allowed_scopes: vec!["inference".to_owned()],
            svid_audience: "https://mint.example.test".to_owned(),
            authority_ttl: DEFAULT_AUTHORITY_TTL,
            introspection_client_credential: IntrospectionClientCredential::new(
                "gateway-credential".to_owned(),
            ),
        },
        MintSigningKey::from_bytes(&signing_key.to_bytes()),
        FixedValidator {
            outcome: Ok(validated_workload()),
        },
        FixedResolver {
            calls: Arc::new(AtomicUsize::new(0)),
            outcome: Ok(active_binding()),
        },
    )
    .map_err(|error| format!("test mint config must be valid: {error:?}"))
}

fn validated_workload() -> ValidatedWorkload {
    ValidatedWorkload {
        spiffe_id: EXPECTED_WORKLOAD.to_owned(),
    }
}

fn valid_config() -> MintConfig {
    MintConfig {
        issuer: "https://mint.example.test".to_owned(),
        audience: "mcp-gw.example.test".to_owned(),
        allowed_scopes: vec!["tools".to_owned()],
        svid_audience: "https://mint.example.test".to_owned(),
        authority_ttl: DEFAULT_AUTHORITY_TTL,
        introspection_client_credential: IntrospectionClientCredential::new(
            "gateway-credential".to_owned(),
        ),
    }
}

#[test]
fn mint_config_rejects_empty_identity_fields_and_out_of_bounds_ttls() {
    assert_eq!(valid_config().validate(), Ok(()));

    let mut config = valid_config();
    config.issuer.clear();
    assert_eq!(config.validate(), Err(MintConfigError::EmptyIssuer));

    let mut config = valid_config();
    config.audience.clear();
    assert_eq!(config.validate(), Err(MintConfigError::EmptyAudience));

    let mut config = valid_config();
    config.svid_audience.clear();
    assert_eq!(config.validate(), Err(MintConfigError::EmptySvidAudience));

    let mut config = valid_config();
    config.allowed_scopes.clear();
    assert_eq!(
        config.validate(),
        Err(MintConfigError::InvalidAllowedScopes)
    );

    let mut config = valid_config();
    config.introspection_client_credential = IntrospectionClientCredential::new(String::new());
    assert_eq!(
        config.validate(),
        Err(MintConfigError::InvalidIntrospectionClientCredential)
    );

    for credential in [
        " gateway-credential",
        "gateway-credential ",
        "gateway-credential\n",
        "gateway credential",
        "gateway:credential",
        "=",
        "==",
    ] {
        let mut config = valid_config();
        config.introspection_client_credential =
            IntrospectionClientCredential::new(credential.to_owned());
        assert_eq!(
            config.validate(),
            Err(MintConfigError::InvalidIntrospectionClientCredential),
            "credential outside the Bearer token grammar must fail configuration"
        );
    }

    let mut config = valid_config();
    config.allowed_scopes = vec!["tools".to_owned(), "tools".to_owned()];
    assert_eq!(
        config.validate(),
        Err(MintConfigError::InvalidAllowedScopes)
    );

    let mut config = valid_config();
    config.authority_ttl = std::time::Duration::ZERO;
    assert_eq!(config.validate(), Err(MintConfigError::InvalidAuthorityTtl));

    let mut config = valid_config();
    config.authority_ttl = DEFAULT_AUTHORITY_TTL + std::time::Duration::from_secs(1);
    assert_eq!(config.validate(), Err(MintConfigError::InvalidAuthorityTtl));
}

#[tokio::test]
async fn forged_svid_fails_before_authority_lookup() -> Result<(), String> {
    let (mint, resolver_calls, _) = mint(Err(SvidValidationError::Rejected), Ok(active_binding()))?;

    let result = mint.exchange(request()).await;

    assert_eq!(
        result.err(),
        Some(MintError::InvalidSvid),
        "a forged JWT-SVID must fail closed"
    );
    assert_eq!(
        resolver_calls.load(Ordering::SeqCst),
        0,
        "an unverified workload must not reach authority resolution"
    );
    Ok(())
}

#[tokio::test]
async fn expired_svid_fails_before_authority_lookup() -> Result<(), String> {
    let (mint, resolver_calls, _) = mint(Err(SvidValidationError::Expired), Ok(active_binding()))?;

    let result = mint.exchange(request()).await;

    assert_eq!(
        result.err(),
        Some(MintError::InvalidSvid),
        "an expired JWT-SVID must fail closed"
    );
    assert_eq!(
        resolver_calls.load(Ordering::SeqCst),
        0,
        "an expired workload must not reach authority resolution"
    );
    Ok(())
}

#[tokio::test]
async fn validated_svid_for_a_different_workload_fails_closed() -> Result<(), String> {
    let mut binding = active_binding();
    binding.workload_id = "spiffe://example.org/agent/runtime-b".to_owned();
    let (mint, _, _) = mint(Ok(validated_workload()), Ok(binding))?;

    let result = mint.exchange(request()).await;

    assert_eq!(
        result.err(),
        Some(MintError::WorkloadMismatch),
        "authority must be bound to the validated SPIFFE workload"
    );
    Ok(())
}

#[tokio::test]
async fn replay_after_runtime_revocation_fails_closed() -> Result<(), String> {
    let mut binding = active_binding();
    binding.state = AuthorityState::Revoked;
    let (mint, _, _) = mint(Ok(validated_workload()), Ok(binding))?;

    let result = mint.exchange(request()).await;

    assert_eq!(
        result.err(),
        Some(MintError::AuthorityInactive),
        "a revoked runtime must not re-mint with a replayed SVID"
    );
    Ok(())
}

#[tokio::test]
async fn terminated_runtime_cannot_obtain_a_token() -> Result<(), String> {
    let mut binding = active_binding();
    binding.state = AuthorityState::Terminated;
    let (mint, _, _) = mint(Ok(validated_workload()), Ok(binding))?;

    let result = mint.exchange(request()).await;

    assert_eq!(
        result.err(),
        Some(MintError::AuthorityInactive),
        "a terminated runtime must not obtain HOP-1"
    );
    Ok(())
}

#[tokio::test]
async fn service_principal_is_rejected_in_v0_1_0() -> Result<(), String> {
    let mut binding = active_binding();
    binding.principal = Principal::Service {
        name: "service-a".to_owned(),
    };
    let (mint, _, _) = mint(Ok(validated_workload()), Ok(binding))?;

    let result = mint.exchange(request()).await;

    assert_eq!(
        result.err(),
        Some(MintError::UnsupportedPrincipal),
        "the service-principal arm is schema-only in v0.1.0"
    );
    Ok(())
}

#[tokio::test]
async fn inference_scope_fails_closed_without_a_runtime_credential_resolver() -> Result<(), String>
{
    let mint = mint_for_inference_without_credential_resolver()?;
    let mut inference_request = request();
    inference_request.scope = vec!["inference".to_owned()];

    let result = mint.exchange(inference_request).await;

    assert!(
        result.is_err(),
        "an inference exchange must never fall back to a signed HOP-1 when no runtime credential resolver is configured"
    );
    Ok(())
}

#[tokio::test]
async fn inference_scope_cannot_fall_back_to_hop1_when_combined_with_another_scope()
-> Result<(), String> {
    let signing_key = SigningKey::generate(&mut OsRng);
    let mint = Mint::new(
        MintConfig {
            issuer: "https://mint.example.test".to_owned(),
            audience: "mcp-gw.example.test".to_owned(),
            allowed_scopes: vec!["inference".to_owned(), "tools".to_owned()],
            svid_audience: "https://mint.example.test".to_owned(),
            authority_ttl: DEFAULT_AUTHORITY_TTL,
            introspection_client_credential: IntrospectionClientCredential::new(
                "gateway-credential".to_owned(),
            ),
        },
        MintSigningKey::from_bytes(&signing_key.to_bytes()),
        FixedValidator {
            outcome: Ok(validated_workload()),
        },
        FixedResolver {
            calls: Arc::new(AtomicUsize::new(0)),
            outcome: Ok(active_binding()),
        },
    )
    .map_err(|error| format!("test mint config must be valid: {error:?}"))?;
    let mut mixed_request = request();
    mixed_request.scope = vec!["inference".to_owned(), "tools".to_owned()];

    let result = mint.exchange(mixed_request).await;

    assert!(
        result.is_err(),
        "any request containing inference authority must fail closed instead of receiving HOP-1"
    );
    Ok(())
}

#[tokio::test]
async fn inference_scope_returns_only_the_runtime_bound_opaque_credential() -> Result<(), String> {
    let signing_key = SigningKey::generate(&mut OsRng);
    let credential_calls = Arc::new(AtomicUsize::new(0));
    let mint = Mint::new_with_credential_resolver(
        MintConfig {
            issuer: "https://mint.example.test".to_owned(),
            audience: "mcp-gw.example.test".to_owned(),
            allowed_scopes: vec!["inference".to_owned()],
            svid_audience: "https://mint.example.test".to_owned(),
            authority_ttl: DEFAULT_AUTHORITY_TTL,
            introspection_client_credential: IntrospectionClientCredential::new(
                "gateway-credential".to_owned(),
            ),
        },
        MintSigningKey::from_bytes(&signing_key.to_bytes()),
        FixedValidator {
            outcome: Ok(validated_workload()),
        },
        FixedResolver {
            calls: Arc::new(AtomicUsize::new(0)),
            outcome: Ok(active_binding()),
        },
        FixedCredentialResolver {
            calls: credential_calls.clone(),
            token: Some("sk-steward-test-runtime-key"),
        },
    )
    .map_err(|error| format!("test mint config must be valid: {error:?}"))?;
    let mut inference_request = request();
    inference_request.scope = vec!["inference".to_owned()];

    let response = mint
        .exchange(inference_request)
        .await
        .map_err(|error| format!("active runtime inference exchange failed: {error:?}"))?;

    assert_eq!(response.access_token(), "sk-steward-test-runtime-key");
    assert_eq!(response.scope(), "inference");
    assert_eq!(response.expires_in(), 60);
    assert_eq!(
        credential_calls.load(Ordering::SeqCst),
        1,
        "a verified active runtime must resolve exactly one runtime-bound credential"
    );
    Ok(())
}

#[tokio::test]
async fn inactive_runtime_fails_before_inference_credential_lookup() -> Result<(), String> {
    let signing_key = SigningKey::generate(&mut OsRng);
    let credential_calls = Arc::new(AtomicUsize::new(0));
    let mut binding = active_binding();
    binding.state = AuthorityState::Suspended;
    let mint = Mint::new_with_credential_resolver(
        MintConfig {
            issuer: "https://mint.example.test".to_owned(),
            audience: "mcp-gw.example.test".to_owned(),
            allowed_scopes: vec!["inference".to_owned()],
            svid_audience: "https://mint.example.test".to_owned(),
            authority_ttl: DEFAULT_AUTHORITY_TTL,
            introspection_client_credential: IntrospectionClientCredential::new(
                "gateway-credential".to_owned(),
            ),
        },
        MintSigningKey::from_bytes(&signing_key.to_bytes()),
        FixedValidator {
            outcome: Ok(validated_workload()),
        },
        FixedResolver {
            calls: Arc::new(AtomicUsize::new(0)),
            outcome: Ok(binding),
        },
        FixedCredentialResolver {
            calls: credential_calls.clone(),
            token: Some("sk-steward-test-runtime-key"),
        },
    )
    .map_err(|error| format!("test mint config must be valid: {error:?}"))?;
    let mut inference_request = request();
    inference_request.scope = vec!["inference".to_owned()];

    let result = mint.exchange(inference_request).await;

    assert_eq!(result.err(), Some(MintError::AuthorityInactive));
    assert_eq!(
        credential_calls.load(Ordering::SeqCst),
        0,
        "suspended authority must fail before any credential Secret lookup"
    );
    Ok(())
}

#[derive(Deserialize)]
struct TestHop1Claims {
    aud: Vec<String>,
    azp: String,
    email: String,
    iss: String,
    jti: String,
    steward: TestStewardClaims,
    sub: String,
}

#[derive(Deserialize)]
struct TestStewardClaims {
    acting_as: String,
    runtime_uid: String,
    tools: Vec<ToolGrant>,
    version: u8,
}

#[tokio::test]
async fn active_user_receives_a_versioned_sixty_second_eddsa_hop1() -> Result<(), String> {
    let (mint, _, verifying_key) = mint(Ok(validated_workload()), Ok(binding_with_tool()))?;

    let response = match mint.exchange(request()).await {
        Ok(response) => response,
        Err(error) => {
            return Err(format!(
                "an active, workload-bound user should receive HOP-1; got {error:?}"
            ));
        }
    };
    let untrusted = UntrustedToken::new(response.access_token())
        .map_err(|error| format!("mint returned malformed JWT: {error}"))?;
    let token: Token<TestHop1Claims> = Ed25519
        .validator(&verifying_key)
        .validate(&untrusted)
        .map_err(|error| format!("mint returned an invalid EdDSA signature: {error}"))?;
    let claims = token.claims();
    claims
        .validate_expiration(&TimeOptions::from_leeway(Duration::zero()))
        .map_err(|error| format!("fresh HOP-1 must be valid: {error}"))?;
    let issued_at = claims
        .issued_at
        .ok_or_else(|| "HOP-1 must carry iat".to_owned())?;
    let expiration = claims
        .expiration
        .ok_or_else(|| "HOP-1 must carry exp".to_owned())?;

    assert_eq!(response.expires_in(), 60);
    assert_eq!(response.token_type(), "Bearer");
    assert_eq!(response.scope(), "tools");
    assert_eq!(expiration - issued_at, Duration::seconds(60));
    assert_eq!(claims.custom.iss, "https://mint.example.test");
    assert_eq!(claims.custom.aud, ["mcp-gw.example.test"]);
    assert_eq!(claims.custom.sub, "alice@example.com");
    assert_eq!(claims.custom.email, "alice@example.com");
    assert_eq!(claims.custom.azp, EXPECTED_WORKLOAD);
    assert!(!claims.custom.jti.is_empty(), "HOP-1 must carry a jti");
    assert_eq!(claims.custom.steward.version, 1);
    assert_eq!(claims.custom.steward.acting_as, "user");
    assert_eq!(claims.custom.steward.runtime_uid, "runtime-uid-a");
    assert_eq!(claims.custom.steward.tools, binding_with_tool().tools);
    Ok::<(), String>(())
}

#[test]
fn jwks_contains_only_the_public_eddsa_key() -> Result<(), String> {
    let (mint, _, _) = mint(Ok(validated_workload()), Ok(active_binding()))?;

    let jwks = mint
        .jwks()
        .map_err(|error| format!("build public JWKS: {error:?}"))?;
    let document =
        serde_json::to_value(jwks).map_err(|error| format!("serialize public JWKS: {error}"))?;
    let keys = document["keys"]
        .as_array()
        .ok_or_else(|| "JWKS keys must be an array".to_owned())?;

    assert_eq!(keys.len(), 1, "JWKS must publish the active signing key");
    assert_eq!(keys[0]["alg"], "EdDSA");
    assert_eq!(keys[0]["crv"], "Ed25519");
    assert_eq!(keys[0]["kty"], "OKP");
    assert_eq!(keys[0]["use"], "sig");
    assert!(
        keys[0]["kid"]
            .as_str()
            .is_some_and(|value| !value.is_empty()),
        "JWKS key must have a stable identifier"
    );
    assert!(
        keys[0]["x"].as_str().is_some_and(|value| !value.is_empty()),
        "JWKS key must publish the Ed25519 public coordinate"
    );
    assert!(
        keys[0].get("d").is_none(),
        "JWKS must never contain private key material"
    );
    Ok(())
}
