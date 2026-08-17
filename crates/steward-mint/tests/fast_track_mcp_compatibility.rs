//! FAST-TRACK / NON-PROMOTABLE proof for the existing standalone MCP-GW identity contract.

use std::future::{Future, ready};

use ed25519_dalek::SigningKey;
use jwt_compact::alg::Ed25519;
use jwt_compact::prelude::UntrustedToken;
use jwt_compact::{AlgorithmExt, Token};
use rand_core::OsRng;
use steward_mint::{
    AuthorityBinding, AuthorityResolver, AuthorityState, DEFAULT_AUTHORITY_TTL,
    IntrospectionClientCredential, Mint, MintConfig, MintError, MintSigningKey,
    SPIFFE_CLIENT_ASSERTION_TYPE, SvidAssertion, SvidValidationError, SvidValidator,
    TokenGrantRequest, ValidatedWorkload,
};
use steward_types::{
    CanonicalAuthorityBinding, CanonicalUserId, Email, Principal, RuntimeId, ToolGrant,
};

const EMAIL: &str = "alice@example.com";
const WORKLOAD: &str = "spiffe://example.org/agent/runtime-fast-track";

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
        runtime: RuntimeId("runtime-fast-track".to_owned()),
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

#[tokio::test]
async fn governed_runtime_projects_existing_mcp_gateway_email_subject() -> Result<(), String> {
    let signing_key = SigningKey::generate(&mut OsRng);
    let verifier = signing_key.verifying_key();
    let mint = Mint::new(
        MintConfig {
            issuer: "https://steward-mint.preview.example".to_owned(),
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

    assert_eq!(claims["iss"], "https://steward-mint.preview.example");
    assert_eq!(claims["aud"], serde_json::json!(["steward-mcp"]));
    assert_eq!(claims["sub"], EMAIL);
    assert_eq!(claims["email"], EMAIL);
    assert_eq!(claims["steward"]["acting_as"], "service_for_user");
    assert_eq!(
        claims["steward"]["canonical_user_id"],
        "usr_0123456789abcdef0123456789abcdef"
    );
    assert_eq!(claims["steward"]["service"], "steward-run");
    assert_eq!(claims["steward"]["version"], 2);
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
