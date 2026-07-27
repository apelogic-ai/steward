//! Thin OpenShell integration seam.

#[cfg(feature = "runtime")]
use std::collections::HashMap;
#[cfg(feature = "runtime")]
use std::future::Future;

#[cfg(feature = "identity")]
use kube::Client;
#[cfg(feature = "identity")]
use kube::api::{Api, ListParams};
#[cfg(feature = "identity")]
use kube::core::DynamicObject;
#[cfg(feature = "identity")]
use kube::discovery::Discovery;
#[cfg(feature = "runtime")]
use openshell_sdk::{
    ClientConfig, OpenShellClient, SandboxPhase, SandboxSpec, SdkError, WorkspaceScopedClient,
};
use sha2::{Digest, Sha256};
#[cfg(feature = "runtime")]
use steward_ports::PortError;
#[cfg(feature = "runtime")]
use steward_ports::{SandboxObservation, SandboxRequest, SandboxRuntime};
#[cfg(feature = "runtime")]
use steward_types::RuntimeRefs;

pub const IMPLEMENTED_PORTS: [&str; 0] = [];
const NAME_LENGTH: usize = 19;
const HASH_CHARACTERS: usize = NAME_LENGTH - 2;
const LOWER_BASE36: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
#[cfg(feature = "runtime")]
const BASE_SANDBOX_IMAGE: &str = "ghcr.io/nvidia/openshell-community/sandboxes/base@sha256:aeef1c63f00e2913ea002ccb3aaf925f338b5c5d70e63576f0d95c16a138044e";
#[cfg(feature = "runtime")]
const RUNTIME_UID_LABEL: &str = "agents.apelogic.ai/runtime-uid";
#[cfg(feature = "runtime")]
const TOOL_PROVIDER: &str = "steward-mcp-gw";
#[cfg(feature = "identity")]
const SANDBOX_ID_LABEL: &str = "openshell.ai/sandbox-id";
#[cfg(feature = "identity")]
const SANDBOX_NAME_ANNOTATION: &str = "openshell.ai/sandbox-name";
#[cfg(feature = "identity")]
const SANDBOX_WORKSPACE_ANNOTATION: &str = "openshell.ai/sandbox-workspace";
#[cfg(feature = "identity")]
const MANAGED_BY_LABEL: &str = "openshell.ai/managed-by";
#[cfg(feature = "identity")]
const MANAGED_BY_VALUE: &str = "openshell";

#[cfg(feature = "identity")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SandboxBinding {
    pub sandbox: String,
    pub workspace: String,
}

#[cfg(feature = "identity")]
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum IdentityResolutionError {
    Rejected { reason: String },
    Unavailable { reason: String },
}

#[cfg(feature = "identity")]
fn binding_from_sandbox(
    sandbox_id: &str,
    object: &DynamicObject,
) -> Result<SandboxBinding, IdentityResolutionError> {
    let labels =
        object
            .metadata
            .labels
            .as_ref()
            .ok_or_else(|| IdentityResolutionError::Rejected {
                reason: "OpenShell sandbox identity has no labels".to_owned(),
            })?;
    if labels.get(SANDBOX_ID_LABEL).map(String::as_str) != Some(sandbox_id)
        || labels.get(MANAGED_BY_LABEL).map(String::as_str) != Some(MANAGED_BY_VALUE)
    {
        return Err(IdentityResolutionError::Rejected {
            reason: "OpenShell sandbox identity does not match the live managed object".to_owned(),
        });
    }
    let annotations =
        object
            .metadata
            .annotations
            .as_ref()
            .ok_or_else(|| IdentityResolutionError::Rejected {
                reason: "OpenShell sandbox identity has no annotations".to_owned(),
            })?;
    let sandbox = annotations
        .get(SANDBOX_NAME_ANNOTATION)
        .filter(|value| !value.is_empty())
        .cloned()
        .ok_or_else(|| IdentityResolutionError::Rejected {
            reason: "OpenShell sandbox identity has no immutable sandbox name".to_owned(),
        })?;
    let workspace = annotations
        .get(SANDBOX_WORKSPACE_ANNOTATION)
        .filter(|value| !value.is_empty())
        .cloned()
        .ok_or_else(|| IdentityResolutionError::Rejected {
            reason: "OpenShell sandbox identity has no immutable workspace name".to_owned(),
        })?;
    Ok(SandboxBinding { sandbox, workspace })
}

#[cfg(feature = "identity")]
#[derive(Clone)]
pub struct OpenShellIdentityResolver {
    sandboxes: Api<DynamicObject>,
    workload_prefix: String,
}

#[cfg(feature = "identity")]
impl OpenShellIdentityResolver {
    pub async fn discover(
        client: Client,
        namespace: &str,
        trust_domain: &str,
    ) -> Result<Self, IdentityResolutionError> {
        if namespace.is_empty() || trust_domain.is_empty() {
            return Err(IdentityResolutionError::Rejected {
                reason: "OpenShell identity namespace and trust domain are required".to_owned(),
            });
        }
        let discovery = Discovery::new(client.clone())
            .filter(&["agents.x-k8s.io"])
            .run()
            .await
            .map_err(identity_failure)?;
        let resource = discovery
            .get("agents.x-k8s.io")
            .and_then(|group| group.recommended_kind("Sandbox"))
            .map(|(resource, _)| resource)
            .ok_or_else(|| IdentityResolutionError::Unavailable {
                reason: "Agent Sandbox API is not discoverable".to_owned(),
            })?;
        Ok(Self {
            sandboxes: Api::namespaced_with(client, namespace, &resource),
            workload_prefix: format!("spiffe://{trust_domain}/openshell/sandbox/"),
        })
    }

    pub async fn resolve(
        &self,
        workload_id: &str,
    ) -> Result<SandboxBinding, IdentityResolutionError> {
        let sandbox_id = workload_id
            .strip_prefix(&self.workload_prefix)
            .filter(|value| {
                !value.is_empty()
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            })
            .ok_or_else(|| IdentityResolutionError::Rejected {
                reason: "workload is outside the configured OpenShell trust domain".to_owned(),
            })?;
        let objects = self
            .sandboxes
            .list(&ListParams::default().labels(&format!("{SANDBOX_ID_LABEL}={sandbox_id}")))
            .await
            .map_err(identity_failure)?;
        let [object] = objects.items.as_slice() else {
            return Err(IdentityResolutionError::Rejected {
                reason: "workload must resolve to exactly one live OpenShell sandbox".to_owned(),
            });
        };
        binding_from_sandbox(sandbox_id, object)
    }
}

#[cfg(feature = "identity")]
fn identity_failure(error: kube::Error) -> IdentityResolutionError {
    IdentityResolutionError::Unavailable {
        reason: format!("OpenShell identity lookup failed: {error}"),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NameKind {
    Workspace,
    Sandbox,
}

pub fn stable_name(kind: NameKind, identity: &[u8]) -> String {
    let prefix = match kind {
        NameKind::Workspace => "w-",
        NameKind::Sandbox => "s-",
    };
    let digest = Sha256::digest(identity);
    let mut name = String::with_capacity(NAME_LENGTH);
    name.push_str(prefix);
    let mut quotient = digest;
    let mut encoded = [b'0'; HASH_CHARACTERS];
    for digit in encoded.iter_mut().rev() {
        let mut remainder = 0_u16;
        for byte in &mut quotient {
            let value = (remainder << 8) | u16::from(*byte);
            *byte = (value / 36) as u8;
            remainder = value % 36;
        }
        *digit = LOWER_BASE36[usize::from(remainder)];
    }
    for byte in encoded {
        name.push(char::from(byte));
    }
    name
}

#[cfg(feature = "runtime")]
struct OpenShellProjection {
    workspace: String,
    workspace_key: String,
    sandbox: String,
    image: &'static str,
    providers: Vec<String>,
    runtime_uid: String,
}

#[cfg(feature = "runtime")]
fn runtime_refs(projection: &OpenShellProjection) -> RuntimeRefs {
    RuntimeRefs {
        workspace: Some(projection.workspace.clone()),
        sandbox: Some(projection.sandbox.clone()),
        litellm_key: None,
    }
}

#[cfg(feature = "runtime")]
fn project_request(request: &SandboxRequest) -> Result<OpenShellProjection, PortError> {
    let image = match request.agent_type.name.as_str() {
        "base" => BASE_SANDBOX_IMAGE,
        other => {
            return Err(PortError::Rejected {
                reason: format!("unsupported agent type: {other}"),
            });
        }
    };
    Ok(OpenShellProjection {
        workspace: stable_name(NameKind::Workspace, request.workspace_key.as_bytes()),
        workspace_key: request.workspace_key.clone(),
        sandbox: stable_name(NameKind::Sandbox, request.runtime.0.as_bytes()),
        image,
        providers: (!request.tools.is_empty())
            .then(|| TOOL_PROVIDER.to_owned())
            .into_iter()
            .collect(),
        runtime_uid: request.runtime.0.clone(),
    })
}

#[cfg(feature = "runtime")]
#[derive(Clone)]
pub struct OpenShellRuntime {
    client: OpenShellClient,
}

#[cfg(feature = "runtime")]
impl OpenShellRuntime {
    pub async fn connect(endpoint: impl Into<String>) -> Result<Self, PortError> {
        let client = OpenShellClient::connect(ClientConfig::new(endpoint.into()))
            .await
            .map_err(port_failure)?;
        Ok(Self { client })
    }

    async fn ensure_workspace(&self, name: &str, workspace_key: &str) -> Result<(), PortError> {
        match self.client.get_workspace(name).await {
            Ok(workspace) => {
                if workspace
                    .labels
                    .get("agents.apelogic.ai/workspace-key")
                    .map(String::as_str)
                    == Some(workspace_key)
                {
                    Ok(())
                } else {
                    Err(PortError::Rejected {
                        reason: "workspace name resolved to a different workspace key".to_owned(),
                    })
                }
            }
            Err(SdkError::NotFound { .. }) => {
                let mut labels = HashMap::new();
                labels.insert(
                    "agents.apelogic.ai/workspace-key".to_owned(),
                    workspace_key.to_owned(),
                );
                match self.client.create_workspace(name, labels).await {
                    Ok(_) | Err(SdkError::AlreadyExists { .. }) => Ok(()),
                    Err(error) => Err(port_failure(error)),
                }
            }
            Err(error) => Err(port_failure(error)),
        }
    }
}

#[cfg(feature = "runtime")]
trait SandboxDeleteClient {
    fn sandbox_labels(
        &self,
        name: &str,
    ) -> impl Future<Output = Result<Option<HashMap<String, String>>, PortError>> + Send;

    fn delete_sandbox(&self, name: &str) -> impl Future<Output = Result<bool, PortError>> + Send;
}

#[cfg(feature = "runtime")]
impl SandboxDeleteClient for WorkspaceScopedClient {
    async fn sandbox_labels(
        &self,
        name: &str,
    ) -> Result<Option<HashMap<String, String>>, PortError> {
        match self.get_sandbox(name).await {
            Ok(snapshot) => Ok(Some(snapshot.labels)),
            Err(SdkError::NotFound { .. }) => Ok(None),
            Err(error) => Err(port_failure(error)),
        }
    }

    async fn delete_sandbox(&self, name: &str) -> Result<bool, PortError> {
        match WorkspaceScopedClient::delete_sandbox(self, name).await {
            Ok(deleted) => Ok(deleted),
            Err(SdkError::NotFound { .. }) => Ok(false),
            Err(error) => Err(port_failure(error)),
        }
    }
}

#[cfg(feature = "runtime")]
async fn delete_owned_sandbox<C>(
    client: &C,
    sandbox: &str,
    runtime_uid: &str,
) -> Result<bool, PortError>
where
    C: SandboxDeleteClient + Sync,
{
    let Some(labels) = client.sandbox_labels(sandbox).await? else {
        return Ok(false);
    };
    if labels.get(RUNTIME_UID_LABEL).map(String::as_str) != Some(runtime_uid) {
        return Err(PortError::Rejected {
            reason: "sandbox name resolved to a different runtime UID".to_owned(),
        });
    }
    client.delete_sandbox(sandbox).await
}

#[cfg(feature = "runtime")]
impl SandboxRuntime for OpenShellRuntime {
    async fn ensure(&self, request: &SandboxRequest) -> Result<SandboxObservation, PortError> {
        let projection = project_request(request)?;
        self.ensure_workspace(&projection.workspace, &projection.workspace_key)
            .await?;
        let scoped = self.client.workspace(&projection.workspace);
        let snapshot = match scoped.get_sandbox(&projection.sandbox).await {
            Ok(snapshot) => snapshot,
            Err(SdkError::NotFound { .. }) => {
                let mut labels = HashMap::new();
                labels.insert(RUNTIME_UID_LABEL.to_owned(), projection.runtime_uid.clone());
                match scoped
                    .create_sandbox(SandboxSpec {
                        name: Some(projection.sandbox.clone()),
                        image: Some(projection.image.to_owned()),
                        labels,
                        providers: projection.providers.clone(),
                        ..SandboxSpec::default()
                    })
                    .await
                {
                    Ok(snapshot) => snapshot,
                    Err(SdkError::AlreadyExists { .. }) => scoped
                        .get_sandbox(&projection.sandbox)
                        .await
                        .map_err(port_failure)?,
                    Err(error) => return Err(port_failure(error)),
                }
            }
            Err(error) => return Err(port_failure(error)),
        };
        if snapshot.labels.get(RUNTIME_UID_LABEL) != Some(&projection.runtime_uid) {
            return Err(PortError::Rejected {
                reason: "sandbox name resolved to a different runtime UID".to_owned(),
            });
        }
        let refs = runtime_refs(&projection);
        match snapshot.phase {
            SandboxPhase::Ready => Ok(SandboxObservation::Running { refs }),
            SandboxPhase::Error => Err(PortError::Failed {
                reason: "sandbox entered an error phase".to_owned(),
            }),
            _ => Ok(SandboxObservation::Provisioning { refs }),
        }
    }

    async fn delete(&self, request: &SandboxRequest) -> Result<SandboxObservation, PortError> {
        let workspace = stable_name(NameKind::Workspace, request.workspace_key.as_bytes());
        let sandbox = stable_name(NameKind::Sandbox, request.runtime.0.as_bytes());
        let scoped = self.client.workspace(&workspace);
        let deleted = delete_owned_sandbox(&scoped, &sandbox, &request.runtime.0).await?;
        if deleted {
            Ok(SandboxObservation::Provisioning {
                refs: RuntimeRefs {
                    workspace: Some(workspace),
                    sandbox: Some(sandbox),
                    litellm_key: None,
                },
            })
        } else {
            Ok(SandboxObservation::Absent)
        }
    }
}

#[cfg(feature = "runtime")]
fn port_failure(error: SdkError) -> PortError {
    PortError::Failed {
        reason: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "identity")]
    use std::collections::BTreeMap;
    #[cfg(feature = "runtime")]
    use std::collections::HashMap;
    #[cfg(feature = "runtime")]
    use std::future::Future;
    #[cfg(feature = "runtime")]
    use std::sync::atomic::{AtomicBool, Ordering};
    #[cfg(feature = "runtime")]
    use std::task::{Context, Poll, Waker};

    #[cfg(feature = "runtime")]
    use steward_ports::PortError;
    #[cfg(feature = "runtime")]
    use steward_ports::SandboxRequest;
    #[cfg(feature = "runtime")]
    use steward_types::{AgentType, RuntimeId, ToolGrant};

    #[cfg(feature = "identity")]
    use super::{IdentityResolutionError, SANDBOX_ID_LABEL, binding_from_sandbox};
    use super::{NameKind, stable_name};
    #[cfg(feature = "runtime")]
    use super::{SandboxDeleteClient, delete_owned_sandbox, project_request};

    #[cfg(feature = "runtime")]
    struct FakeDeleteClient {
        labels: HashMap<String, String>,
        deleted: AtomicBool,
    }

    #[cfg(feature = "runtime")]
    impl SandboxDeleteClient for FakeDeleteClient {
        async fn sandbox_labels(
            &self,
            _name: &str,
        ) -> Result<Option<HashMap<String, String>>, PortError> {
            Ok(Some(self.labels.clone()))
        }

        async fn delete_sandbox(&self, _name: &str) -> Result<bool, PortError> {
            self.deleted.store(true, Ordering::SeqCst);
            Ok(true)
        }
    }

    #[cfg(feature = "runtime")]
    fn ready<F: Future>(future: F) -> Result<F::Output, String> {
        let mut future = std::pin::pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => Ok(output),
            Poll::Pending => Err("fake deletion future unexpectedly yielded".to_owned()),
        }
    }

    #[test]
    fn stable_names_fit_the_immutable_openshell_cap() {
        let workspace = stable_name(NameKind::Workspace, b"team-a");
        let sandbox = stable_name(NameKind::Sandbox, b"runtime-uid-1");

        assert_eq!(
            workspace, "w-9086ou4eujpgku8z0",
            "workspace names must encode the full 17-character base36 budget"
        );
        assert_eq!(
            sandbox, "s-78i56shpq2adzg64z",
            "sandbox names must encode the full 17-character base36 budget"
        );
        for name in [&workspace, &sandbox] {
            assert_eq!(name.len(), 19, "OpenShell names must fit its 19-char cap");
            assert!(
                name.bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'),
                "OpenShell names must remain DNS-safe: {name}"
            );
        }
    }

    #[cfg(feature = "identity")]
    #[test]
    fn identity_binding_rejects_a_reused_name_with_a_different_sandbox_id() {
        let resource = kube::core::ApiResource::from_gvk(&kube::core::GroupVersionKind::gvk(
            "agents.x-k8s.io",
            "v1alpha1",
            "Sandbox",
        ));
        let mut object = kube::core::DynamicObject::new("workspace--sandbox", &resource);
        object.metadata.labels = Some(BTreeMap::from([(
            SANDBOX_ID_LABEL.to_owned(),
            "sandbox-id-b".to_owned(),
        )]));

        let result = binding_from_sandbox("sandbox-id-a", &object);

        assert!(
            matches!(result, Err(IdentityResolutionError::Rejected { .. })),
            "the SVID sandbox UUID must match the live OpenShell object; got {result:?}"
        );
    }

    #[cfg(feature = "identity")]
    #[test]
    fn identity_binding_resolves_the_exact_live_sandbox() -> Result<(), String> {
        let resource = kube::core::ApiResource::from_gvk(&kube::core::GroupVersionKind::gvk(
            "agents.x-k8s.io",
            "v1alpha1",
            "Sandbox",
        ));
        let mut object = kube::core::DynamicObject::new("workspace--sandbox", &resource);
        object.metadata.labels = Some(BTreeMap::from([
            (SANDBOX_ID_LABEL.to_owned(), "sandbox-id-a".to_owned()),
            ("openshell.ai/managed-by".to_owned(), "openshell".to_owned()),
        ]));
        object.metadata.annotations = Some(BTreeMap::from([
            ("openshell.ai/sandbox-name".to_owned(), "sandbox".to_owned()),
            (
                "openshell.ai/sandbox-workspace".to_owned(),
                "workspace".to_owned(),
            ),
        ]));

        let binding = binding_from_sandbox("sandbox-id-a", &object)
            .map_err(|error| format!("exact sandbox binding was rejected: {error:?}"))?;

        assert_eq!(binding.sandbox, "sandbox");
        assert_eq!(binding.workspace, "workspace");
        Ok(())
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn runtime_projection_is_stable_and_uid_bound() -> Result<(), String> {
        let projection = project_request(&SandboxRequest {
            runtime: RuntimeId("runtime-uid-a".to_owned()),
            workspace_key: "team-a".to_owned(),
            agent_type: AgentType {
                name: "base".to_owned(),
            },
            tools: Vec::new(),
        })
        .map_err(|error| format!("runtime projection failed: {error:?}"))?;

        assert_eq!(projection.workspace, "w-9086ou4eujpgku8z0");
        assert_eq!(projection.workspace_key, "team-a");
        assert_eq!(projection.sandbox, "s-tmtp1a3s40p1kixv2");
        assert_eq!(
            projection.image,
            "ghcr.io/nvidia/openshell-community/sandboxes/base@sha256:aeef1c63f00e2913ea002ccb3aaf925f338b5c5d70e63576f0d95c16a138044e"
        );
        assert_eq!(projection.runtime_uid, "runtime-uid-a");
        Ok(())
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn tool_authority_attaches_only_the_steward_gateway_provider() -> Result<(), String> {
        let projection = project_request(&SandboxRequest {
            runtime: RuntimeId("runtime-uid-a".to_owned()),
            workspace_key: "team-a".to_owned(),
            agent_type: AgentType {
                name: "base".to_owned(),
            },
            tools: vec![ToolGrant {
                provider: "github".to_owned(),
                resource: "search_repositories".to_owned(),
                action: "read".to_owned(),
            }],
        })
        .map_err(|error| format!("runtime projection failed: {error:?}"))?;

        assert_eq!(
            projection.providers,
            ["steward-mcp-gw"],
            "a tool-bearing runtime must use the token-grant provider and no ambient provider"
        );
        Ok(())
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn delete_rejects_a_sandbox_owned_by_another_runtime() -> Result<(), String> {
        let client = FakeDeleteClient {
            labels: HashMap::from([(
                "agents.apelogic.ai/runtime-uid".to_owned(),
                "runtime-uid-b".to_owned(),
            )]),
            deleted: AtomicBool::new(false),
        };

        let result = ready(delete_owned_sandbox(
            &client,
            "s-tmtp1a3s40p1kixv2",
            "runtime-uid-a",
        ))?;

        assert!(
            matches!(result, Err(PortError::Rejected { .. })),
            "delete must reject a same-name sandbox owned by another runtime; got {result:?}"
        );
        assert!(
            !client.deleted.load(Ordering::SeqCst),
            "delete must not touch a sandbox owned by another runtime"
        );
        Ok(())
    }
}
