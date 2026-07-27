use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use kube::Client;
use kube::api::{Api, ListParams};
use steward_adapter_openshell::{
    IdentityResolutionError, OpenShellIdentityResolver, SandboxBinding,
};
use steward_adapter_spire::SpireSvidValidator;
use steward_mint::{
    AuthorityBinding, AuthorityResolver, AuthorityState, DEFAULT_AUTHORITY_TTL,
    IntrospectionClientCredential, Mint, MintConfig, MintError, MintSigningKey, ValidatedWorkload,
    router,
};
use steward_types::{AgentRuntime, Phase, RuntimeId};
use tokio::net::TcpListener;

fn authority_from_runtimes(
    workload: &ValidatedWorkload,
    binding: &SandboxBinding,
    runtimes: &[AgentRuntime],
) -> Result<AuthorityBinding, MintError> {
    let mut matches = runtimes.iter().filter(|runtime| {
        runtime.status.as_ref().is_some_and(|status| {
            status.refs.workspace.as_deref() == Some(binding.workspace.as_str())
                && status.refs.sandbox.as_deref() == Some(binding.sandbox.as_str())
        })
    });
    let runtime = matches.next().ok_or(MintError::WorkloadMismatch)?;
    if matches.next().is_some() {
        return Err(MintError::WorkloadMismatch);
    }
    let runtime_uid = runtime
        .metadata
        .uid
        .clone()
        .filter(|uid| !uid.is_empty())
        .ok_or(MintError::WorkloadMismatch)?;
    let phase = runtime
        .status
        .as_ref()
        .map(|status| status.phase)
        .ok_or(MintError::WorkloadMismatch)?;
    let state = match phase {
        Phase::Running => AuthorityState::Active,
        Phase::Suspended => AuthorityState::Suspended,
        Phase::Terminating => AuthorityState::Revoked,
        Phase::Terminated => AuthorityState::Terminated,
        Phase::Pending | Phase::Admitted | Phase::Provisioning | Phase::Failed => {
            AuthorityState::Revoked
        }
    };
    Ok(AuthorityBinding {
        workload_id: workload.spiffe_id.clone(),
        runtime: RuntimeId(runtime_uid),
        principal: runtime.spec.principal.clone(),
        tools: runtime.spec.tools.clone(),
        state,
    })
}

#[derive(Clone)]
struct KubernetesAuthorityResolver {
    identity: OpenShellIdentityResolver,
    runtimes: Api<AgentRuntime>,
}

impl AuthorityResolver for KubernetesAuthorityResolver {
    async fn resolve(&self, workload: &ValidatedWorkload) -> Result<AuthorityBinding, MintError> {
        let binding =
            self.identity
                .resolve(&workload.spiffe_id)
                .await
                .map_err(|error| match error {
                    IdentityResolutionError::Rejected { .. } => MintError::WorkloadMismatch,
                    IdentityResolutionError::Unavailable { .. } => MintError::AuthorityUnavailable,
                    _ => MintError::AuthorityUnavailable,
                })?;
        let runtimes = self
            .runtimes
            .list(&ListParams::default())
            .await
            .map_err(|_| MintError::AuthorityUnavailable)?;
        authority_from_runtimes(workload, &binding, &runtimes.items)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let client = Client::try_default().await?;
    let identity = OpenShellIdentityResolver::discover(
        client.clone(),
        &env::var("STEWARD_OPENSHELL_NAMESPACE").unwrap_or_else(|_| "openshell".to_owned()),
        &env::var("STEWARD_SPIFFE_TRUST_DOMAIN").unwrap_or_else(|_| "openshell.local".to_owned()),
    )
    .await
    .map_err(|error| io::Error::other(format!("OpenShell identity discovery failed: {error:?}")))?;
    let resolver = KubernetesAuthorityResolver {
        identity,
        runtimes: Api::all(client),
    };
    let validator = SpireSvidValidator::connect_env()
        .await
        .map_err(|_| io::Error::other("SPIRE Workload API connection failed"))?;
    let signing_key = load_signing_key(&required("STEWARD_MINT_SIGNING_KEY_FILE")?)?;
    let introspection_credential = fs::read_to_string(required(
        "STEWARD_MINT_INTROSPECTION_CLIENT_CREDENTIAL_FILE",
    )?)?;
    let authority_ttl = match env::var("STEWARD_AUTHORITY_TTL") {
        Ok(value) => value.parse::<u64>().map(Duration::from_secs).map_err(|_| {
            io::Error::other("STEWARD_AUTHORITY_TTL must be an integer number of seconds")
        })?,
        Err(env::VarError::NotPresent) => DEFAULT_AUTHORITY_TTL,
        Err(env::VarError::NotUnicode(_)) => {
            return Err(io::Error::other("STEWARD_AUTHORITY_TTL must be valid UTF-8").into());
        }
    };
    let config = MintConfig {
        issuer: required("STEWARD_MINT_ISSUER")?,
        audience: required("STEWARD_MINT_AUDIENCE")?,
        allowed_scopes: required("STEWARD_MINT_ALLOWED_SCOPES")?
            .split_whitespace()
            .map(ToOwned::to_owned)
            .collect(),
        svid_audience: required("STEWARD_MINT_SVID_AUDIENCE")?,
        authority_ttl,
        introspection_client_credential: IntrospectionClientCredential::new(
            introspection_credential,
        ),
    };
    let mint = Mint::new(config, signing_key, validator, resolver)
        .map_err(|error| io::Error::other(format!("mint configuration is invalid: {error:?}")))?;
    let listener = TcpListener::bind(
        env::var("STEWARD_MINT_BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_owned()),
    )
    .await?;
    axum::serve(listener, router(Arc::new(mint))).await?;
    Ok(())
}

fn required(name: &str) -> Result<String, io::Error> {
    env::var(name).map_err(|_| io::Error::other(format!("{name} is required")))
}

fn load_signing_key(path: &str) -> Result<MintSigningKey, io::Error> {
    let mut material = fs::read(path)?;
    let mut bytes: [u8; 32] = material.as_slice().try_into().map_err(|_| {
        io::Error::other("STEWARD_MINT_SIGNING_KEY_FILE must contain exactly 32 bytes")
    })?;
    let key = MintSigningKey::from_bytes(&bytes);
    bytes.fill(0);
    material.fill(0);
    Ok(key)
}

#[cfg(test)]
mod tests {
    use steward_adapter_openshell::SandboxBinding;
    use steward_mint::{AuthorityState, MintError, ValidatedWorkload};
    use steward_types::{
        AgentRuntime, AgentRuntimeSpec, AgentRuntimeStatus, AgentType, Budget, Duration, Email,
        Phase, Principal, RuntimeRefs, ToolGrant,
    };

    use super::authority_from_runtimes;

    fn runtime(uid: &str, workspace: &str, sandbox: &str) -> AgentRuntime {
        let mut runtime = AgentRuntime::new(
            "runtime-a",
            AgentRuntimeSpec {
                principal: Principal::User {
                    acting_user: Email("alice@example.com".to_owned()),
                },
                owner: Email("alice@example.com".to_owned()),
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
        runtime.metadata.uid = Some(uid.to_owned());
        runtime.status = Some(AgentRuntimeStatus {
            phase: Phase::Running,
            observed_generation: 1,
            spec_digest: "digest-a".to_owned(),
            refs: RuntimeRefs {
                workspace: Some(workspace.to_owned()),
                sandbox: Some(sandbox.to_owned()),
                litellm_key: None,
            },
            conditions: Vec::new(),
            spend: None,
        });
        runtime
    }

    fn workload() -> ValidatedWorkload {
        ValidatedWorkload {
            spiffe_id: "spiffe://example.test/openshell/sandbox/sandbox-id-a".to_owned(),
        }
    }

    fn binding() -> SandboxBinding {
        SandboxBinding {
            workspace: "workspace-a".to_owned(),
            sandbox: "sandbox-a".to_owned(),
        }
    }

    #[test]
    fn authority_resolution_rejects_an_ambiguous_runtime_binding() {
        let result = authority_from_runtimes(
            &workload(),
            &binding(),
            &[
                runtime("runtime-uid-a", "workspace-a", "sandbox-a"),
                runtime("runtime-uid-b", "workspace-a", "sandbox-a"),
            ],
        );

        assert!(
            matches!(result, Err(MintError::WorkloadMismatch)),
            "one sandbox must never resolve to more than one runtime; got {result:?}"
        );
    }

    #[test]
    fn authority_resolution_binds_live_runtime_principal_and_tools() -> Result<(), String> {
        let authority = authority_from_runtimes(
            &workload(),
            &binding(),
            &[runtime("runtime-uid-a", "workspace-a", "sandbox-a")],
        )
        .map_err(|error| format!("live runtime authority was rejected: {error:?}"))?;

        assert_eq!(authority.workload_id, workload().spiffe_id);
        assert_eq!(authority.runtime.0, "runtime-uid-a");
        assert_eq!(
            authority.principal,
            Principal::User {
                acting_user: Email("alice@example.com".to_owned())
            }
        );
        assert_eq!(
            authority.tools,
            [ToolGrant {
                provider: "github".to_owned(),
                resource: "search_repositories".to_owned(),
                action: "read".to_owned(),
            }]
        );
        assert_eq!(authority.state, AuthorityState::Active);
        Ok(())
    }
}
