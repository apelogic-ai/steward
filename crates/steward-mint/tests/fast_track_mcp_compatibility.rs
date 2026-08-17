//! Proof for the existing standalone MCP-GW identity contract.

use std::future::{Future, ready};

use ed25519_dalek::SigningKey;
use jwt_compact::alg::Ed25519;
use jwt_compact::prelude::UntrustedToken;
use jwt_compact::{AlgorithmExt, Token};
use rand_core::OsRng;
use steward_mint::{
    AuthorityBinding, AuthorityResolver, AuthorityState, CredentialGrant, CredentialGrantResolver,
    DEFAULT_AUTHORITY_TTL, HOP1_CLAIMS_VERSION, Hop1Token, IntrospectionClientCredential, Mint,
    MintConfig, MintError, MintSigningKey, OpaqueAccessToken, SPIFFE_CLIENT_ASSERTION_TYPE,
    SvidAssertion, SvidValidationError, SvidValidator, TokenGrantRequest, ValidatedWorkload,
    authority_from_runtime_refs,
};
use steward_types::{
    AgentRuntime, AgentRuntimeSpec, AgentRuntimeStatus, AgentType, Budget,
    CanonicalAuthorityBinding, CanonicalUserId, Duration, Email, Phase, Principal, RuntimeId,
    RuntimeRefs, ToolGrant,
};

const EMAIL: &str = "alice@example.com";
const WORKLOAD: &str = "spiffe://example.org/agent/runtime-canonical-subject";
const S5_WORKLOAD: &str = "spiffe://example.org/openshell/sandbox/s5-sandbox";
const S5_WORKSPACE: &str = "s5-workspace";
const S5_SANDBOX: &str = "s5-sandbox";
const S5_RUNTIME_NAME: &str = "runtime-revocation";
const S5_RUNTIME_UID: &str = "s5-runtime-uid-00000000";

struct Validator;

impl SvidValidator for Validator {
    fn validate(
        &self,
        _audience: &str,
        _assertion: &SvidAssertion,
    ) -> impl Future<Output = Result<ValidatedWorkload, SvidValidationError>> + Send {
        ready(Ok(ValidatedWorkload {
            spiffe_id: WORKLOAD.to_owned(),
        }))
    }
}

#[derive(Clone)]
struct Resolver(AuthorityBinding);

impl AuthorityResolver for Resolver {
    fn resolve(
        &self,
        _workload: &ValidatedWorkload,
    ) -> impl Future<Output = Result<AuthorityBinding, MintError>> + Send {
        ready(Ok(self.0.clone()))
    }
}

fn authority() -> Result<AuthorityBinding, String> {
    let user = CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?;
    Ok(AuthorityBinding {
        workload_id: WORKLOAD.to_owned(),
        runtime: RuntimeId("runtime-canonical-subject".to_owned()),
        runtime_namespace: "steward-preview".to_owned(),
        principal: Principal::Service {
            name: "steward-run".to_owned(),
            acting_user: Some(Email::parse(EMAIL)?),
        },
        canonical_authority: Some(CanonicalAuthorityBinding::new(user.clone(), Some(user))?),
        tools: vec![ToolGrant {
            provider: "github".to_owned(),
            resource: "get_file_contents".to_owned(),
            action: "read".to_owned(),
        }],
        state: AuthorityState::Active,
    })
}

fn s5_runtime() -> Result<AgentRuntime, String> {
    let user = CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?;
    let mut runtime = AgentRuntime::new(
        S5_RUNTIME_NAME,
        AgentRuntimeSpec {
            principal: Principal::User {
                acting_user: Email::parse(EMAIL)?,
            },
            owner: Email::parse(EMAIL)?,
            canonical_authority: Some(CanonicalAuthorityBinding::new(user.clone(), Some(user))?),
            agent_type: AgentType {
                name: "base".to_owned(),
            },
            llms: Vec::new(),
            tools: vec![ToolGrant {
                provider: "github".to_owned(),
                resource: "search_repositories".to_owned(),
                action: "read".to_owned(),
            }],
            budget: Budget {
                monthly_limit: "1.00".to_owned(),
                currency: "USD".to_owned(),
            },
            ttl: Duration("1h".to_owned()),
            bindings: None,
        },
    );
    runtime.metadata.namespace = Some("team-a".to_owned());
    runtime.metadata.uid = Some(S5_RUNTIME_UID.to_owned());
    runtime.status = Some(AgentRuntimeStatus {
        phase: Phase::Running,
        observed_generation: 1,
        spec_digest: "s5-authority-fixture".to_owned(),
        refs: RuntimeRefs {
            workspace: Some(S5_WORKSPACE.to_owned()),
            sandbox: Some(S5_SANDBOX.to_owned()),
            litellm_key: None,
        },
        conditions: Vec::new(),
        spend: None,
    });
    Ok(runtime)
}

struct S5Validator;

impl SvidValidator for S5Validator {
    fn validate(
        &self,
        _audience: &str,
        _assertion: &SvidAssertion,
    ) -> impl Future<Output = Result<ValidatedWorkload, SvidValidationError>> + Send {
        ready(Ok(ValidatedWorkload {
            spiffe_id: S5_WORKLOAD.to_owned(),
        }))
    }
}

#[derive(Clone)]
struct S5Resolver(AgentRuntime);

impl AuthorityResolver for S5Resolver {
    fn resolve(
        &self,
        workload: &ValidatedWorkload,
    ) -> impl Future<Output = Result<AuthorityBinding, MintError>> + Send {
        let authority = if workload.spiffe_id == S5_WORKLOAD {
            authority_from_runtime_refs(
                workload,
                S5_WORKSPACE,
                S5_SANDBOX,
                std::slice::from_ref(&self.0),
            )
        } else {
            Err(MintError::WorkloadMismatch)
        };
        ready(authority)
    }
}

/// Models the production resolver's security boundary without carrying a
/// credential into the fixture: only the runtime's immutable Kubernetes UID
/// may resolve the per-runtime inference credential.
struct S5InferenceCredentialResolver;

impl CredentialGrantResolver for S5InferenceCredentialResolver {
    async fn resolve(
        &self,
        scope: &[String],
        authority: &AuthorityBinding,
    ) -> Result<CredentialGrant, MintError> {
        if scope != ["inference"] {
            return Ok(CredentialGrant::NotHandled);
        }
        if authority.runtime.0 != S5_RUNTIME_UID || authority.runtime_namespace != "team-a" {
            return Err(MintError::CredentialUnavailable);
        }
        OpaqueAccessToken::new("s5-fixture-inference-token".to_owned())
            .map(CredentialGrant::AccessToken)
            .map_err(|_| MintError::CredentialUnavailable)
    }
}

#[tokio::test]
async fn s5_inference_grant_rejects_the_runtime_name_instead_of_its_uid() -> Result<(), String> {
    let workload = ValidatedWorkload {
        spiffe_id: S5_WORKLOAD.to_owned(),
    };
    let mut authority = S5Resolver(s5_runtime()?)
        .resolve(&workload)
        .await
        .map_err(|error| format!("resolve S5 authority: {error:?}"))?;
    let resolver = S5InferenceCredentialResolver;

    assert!(matches!(
        resolver
            .resolve(&["inference".to_owned()], &authority)
            .await,
        Ok(CredentialGrant::AccessToken(_))
    ));

    authority.runtime = RuntimeId(S5_RUNTIME_NAME.to_owned());
    assert!(matches!(
        resolver
            .resolve(&["inference".to_owned()], &authority)
            .await,
        Err(MintError::CredentialUnavailable)
    ));
    Ok(())
}

#[tokio::test]
async fn governed_runtime_projects_canonical_subject_and_verified_email() -> Result<(), String> {
    let signing_key = SigningKey::generate(&mut OsRng);
    let verifier = signing_key.verifying_key();
    let mint = Mint::new(
        MintConfig {
            issuer: "https://steward-mint.example.org".to_owned(),
            audience: "steward-mcp".to_owned(),
            allowed_scopes: vec!["mcp".to_owned()],
            svid_audience: "steward-mint".to_owned(),
            authority_ttl: DEFAULT_AUTHORITY_TTL,
            introspection_client_credential: IntrospectionClientCredential::new(
                "preview-introspection".to_owned(),
            ),
        },
        MintSigningKey::from_bytes(&signing_key.to_bytes()),
        Validator,
        Resolver(authority()?),
    )
    .map_err(|error| format!("create preview mint: {error:?}"))?;
    let response = mint
        .exchange(TokenGrantRequest {
            grant_type: "client_credentials".to_owned(),
            client_assertion_type: SPIFFE_CLIENT_ASSERTION_TYPE.to_owned(),
            client_assertion: SvidAssertion::new("fixture-svid".to_owned()),
            audience: "steward-mcp".to_owned(),
            scope: vec!["mcp".to_owned()],
        })
        .await
        .map_err(|error| format!("mint preview token: {error:?}"))?;
    let untrusted = UntrustedToken::new(response.access_token())
        .map_err(|error| format!("parse preview token: {error}"))?;
    let token: Token<serde_json::Value> = Ed25519
        .validator(&verifier)
        .validate(&untrusted)
        .map_err(|error| format!("verify preview token: {error}"))?;
    let claims = &token.claims().custom;

    assert_eq!(claims["iss"], "https://steward-mint.example.org");
    assert_eq!(claims["aud"], serde_json::json!(["steward-mcp"]));
    assert_eq!(claims["sub"], "usr_0123456789abcdef0123456789abcdef");
    assert_eq!(claims["email"], EMAIL);
    assert_eq!(claims["steward"]["acting_as"], "service_for_user");
    assert_eq!(claims["steward"]["service"], "steward-run");
    assert_eq!(claims["steward"]["version"], 3);
    assert_eq!(
        claims["steward"]["tools"][0],
        serde_json::json!({
            "provider": "github",
            "resource": "get_file_contents",
            "action": "read"
        })
    );
    assert!(claims.get("canonical_user_id").is_none());
    assert!(claims.get("user_id").is_none());
    Ok(())
}

#[tokio::test]
async fn s5_hop1_v3_authority_mints_and_introspects_the_required_mcp_grant() -> Result<(), String> {
    assert_eq!(
        HOP1_CLAIMS_VERSION, 3,
        "S5 requires the current HOP-1 v3 contract"
    );

    let signing_key = SigningKey::generate(&mut OsRng);
    let verifier = signing_key.verifying_key();
    let mint = Mint::new_with_credential_resolver(
        MintConfig {
            issuer: "https://steward-mint.example.org".to_owned(),
            audience: "steward-mcp".to_owned(),
            allowed_scopes: vec!["mcp".to_owned(), "inference".to_owned()],
            svid_audience: "steward-mint".to_owned(),
            authority_ttl: DEFAULT_AUTHORITY_TTL,
            introspection_client_credential: IntrospectionClientCredential::new(
                "preview-introspection".to_owned(),
            ),
        },
        MintSigningKey::from_bytes(&signing_key.to_bytes()),
        S5Validator,
        S5Resolver(s5_runtime()?),
        S5InferenceCredentialResolver,
    )
    .map_err(|error| format!("create S5 mint: {error:?}"))?;
    let response = mint
        .exchange(TokenGrantRequest {
            grant_type: "client_credentials".to_owned(),
            client_assertion_type: SPIFFE_CLIENT_ASSERTION_TYPE.to_owned(),
            client_assertion: SvidAssertion::new("fixture-svid".to_owned()),
            audience: "steward-mcp".to_owned(),
            scope: vec!["mcp".to_owned()],
        })
        .await
        .map_err(|error| format!("mint S5 token: {error:?}"))?;
    let untrusted = UntrustedToken::new(response.access_token())
        .map_err(|error| format!("parse S5 token: {error}"))?;
    let token: Token<serde_json::Value> = Ed25519
        .validator(&verifier)
        .validate(&untrusted)
        .map_err(|error| format!("verify S5 token: {error}"))?;
    let claims = &token.claims().custom;

    assert_eq!(claims["iss"], "https://steward-mint.example.org");
    assert_eq!(claims["aud"], serde_json::json!(["steward-mcp"]));
    assert_eq!(claims["sub"], "usr_0123456789abcdef0123456789abcdef");
    assert_eq!(claims["email"], EMAIL);
    assert_eq!(claims["steward"]["acting_as"], "user");
    assert!(claims["steward"].get("service").is_none());
    assert_eq!(claims["steward"]["runtime_uid"], S5_RUNTIME_UID);
    assert_ne!(claims["steward"]["runtime_uid"], S5_RUNTIME_NAME);
    assert_eq!(claims["steward"]["version"], HOP1_CLAIMS_VERSION);
    assert_eq!(
        claims["steward"]["tools"][0],
        serde_json::json!({
            "provider": "github",
            "resource": "search_repositories",
            "action": "read"
        })
    );

    // The S5 runtime does not consume the exchange response directly: its MCP
    // gateway sends the resulting HOP-1 token back to Mint for introspection.
    // Keep that exact second half of the path in this cheap preflight so a
    // grant/authority mismatch cannot first surface after sandbox provisioning.
    let hop1: Hop1Token = serde_json::from_value(serde_json::Value::String(
        response.access_token().to_owned(),
    ))
    .map_err(|error| format!("deserialize S5 token for introspection: {error}"))?;
    let introspection = mint
        .introspect(&hop1)
        .await
        .map_err(|error| format!("introspect S5 token: {error:?}"))?;
    let introspection = serde_json::to_value(introspection)
        .map_err(|error| format!("serialize S5 introspection response: {error}"))?;
    assert_eq!(
        introspection["active"], true,
        "the exact S5 HOP-1 grant must remain active through Mint introspection"
    );

    let inference = mint
        .exchange(TokenGrantRequest {
            grant_type: "client_credentials".to_owned(),
            client_assertion_type: SPIFFE_CLIENT_ASSERTION_TYPE.to_owned(),
            client_assertion: SvidAssertion::new("fixture-svid".to_owned()),
            audience: "steward-mcp".to_owned(),
            scope: vec!["inference".to_owned()],
        })
        .await
        .map_err(|error| format!("mint S5 inference credential: {error:?}"))?;
    assert_eq!(inference.access_token(), "s5-fixture-inference-token");
    assert_eq!(inference.scope(), "inference");
    Ok(())
}
