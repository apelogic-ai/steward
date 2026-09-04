//! Thin OpenShell integration seam.

#[cfg(feature = "runtime")]
use std::collections::HashMap;
#[cfg(feature = "runtime")]
use std::future::Future;
#[cfg(feature = "runtime")]
use std::path::{Path, PathBuf};
#[cfg(feature = "runtime")]
use std::sync::Arc;
#[cfg(feature = "runtime")]
use std::time::{Duration as StdDuration, Instant};

#[cfg(feature = "identity")]
use kube::Client;
#[cfg(feature = "identity")]
use kube::api::{Api, ListParams};
#[cfg(feature = "identity")]
use kube::core::DynamicObject;
#[cfg(feature = "identity")]
use kube::discovery::Discovery;
#[cfg(feature = "runtime")]
use openshell_sdk::raw::proto::datamodel::v1::{ObjectMeta, Provider};
#[cfg(feature = "runtime")]
use openshell_sdk::raw::proto::{
    AttachSandboxProviderRequest, CreateProviderRequest, DetachSandboxProviderRequest,
    ExecSandboxEvent, ExecSandboxRequest, GetProviderRequest, GetSandboxRequest,
    ListSandboxProvidersRequest, Sandbox as RawSandbox, SandboxPhase as RawSandboxPhase,
    exec_sandbox_event,
};
#[cfg(feature = "runtime")]
use openshell_sdk::{
    EdgeAuthInterceptor, ExecOptions, OpenShellClient, SandboxPhase, SandboxSpec, SdkError,
};
#[cfg(feature = "runtime")]
use reqwest::header::CACHE_CONTROL;
#[cfg(feature = "runtime")]
use reqwest::{Certificate as HttpCertificate, Client as HttpClient, StatusCode, Url};
#[cfg(feature = "runtime")]
use serde::Deserialize;
use sha2::{Digest, Sha256};
#[cfg(feature = "runtime")]
use steward_ports::PortError;
#[cfg(feature = "runtime")]
use steward_ports::{
    ProviderControlExecutionBindings, SandboxExecutionClass, SandboxObservation, SandboxRequest,
    SandboxRuntime, SandboxTaskOutput, SandboxTaskRequest, SandboxTaskRuntime,
};
#[cfg(feature = "runtime")]
use steward_types::{AgentType, RuntimeRefs};
#[cfg(feature = "runtime")]
use tokio::sync::Mutex;
#[cfg(feature = "runtime")]
use tonic::Streaming;
#[cfg(feature = "runtime")]
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

pub const IMPLEMENTED_PORTS: [&str; 0] = [];
const NAME_LENGTH: usize = 19;
const HASH_CHARACTERS: usize = NAME_LENGTH - 2;
const LOWER_BASE36: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
#[cfg(feature = "runtime")]
const LOWER_HEX: &[u8; 16] = b"0123456789abcdef";
#[cfg(feature = "runtime")]
const TAR_BLOCK_BYTES: usize = 512;
#[cfg(feature = "runtime")]
const STAGING_EXEC_STDIN_CHUNK_BYTES: usize = 512 * 1024;
#[cfg(feature = "runtime")]
const KUBERNETES_DNS_SUBDOMAIN_MAX_LENGTH: usize = 253;
#[cfg(feature = "runtime")]
const KUBERNETES_DNS_LABEL_MAX_LENGTH: usize = 63;
#[cfg(feature = "runtime")]
const RUNTIME_UID_LABEL: &str = "agents.apelogic.ai/runtime-uid";
#[cfg(feature = "runtime")]
const EXECUTION_BINDING_LABEL: &str = "agents.apelogic.ai/execution-binding";
/// Server-authored agent type for the one-shot Connections bridge operation.
pub const CONNECTIONS_BRIDGE_AGENT_TYPE: &str = "connections-bridge";
#[cfg(feature = "runtime")]
const TOOL_PROVIDER: &str = "steward-mcp-gw";
#[cfg(feature = "runtime")]
const INFERENCE_PROVIDER: &str = "steward-litellm";
#[cfg(feature = "runtime")]
const GRPC_NOT_FOUND: i32 = 5;
#[cfg(feature = "runtime")]
const GRPC_ALREADY_EXISTS: i32 = 6;
#[cfg(feature = "runtime")]
const WORKLOAD_EXCHANGE_PATH: &str = "/v1/workload/exchange";
#[cfg(feature = "runtime")]
const WORKLOAD_ACCESS_TOKEN_MAX_TTL: StdDuration = StdDuration::from_secs(120);
#[cfg(feature = "runtime")]
const WORKLOAD_ACCESS_TOKEN_REFRESH_SKEW: StdDuration = StdDuration::from_secs(30);
#[cfg(feature = "runtime")]
const WORKLOAD_EXCHANGE_MAX_RESPONSE_BYTES: u64 = 16 * 1024;
#[cfg(feature = "runtime")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProviderReconciliation {
    Attach,
    Detach,
}

#[cfg(feature = "runtime")]
fn provider_reconciliation(attached: bool, desired: bool) -> Option<ProviderReconciliation> {
    match (attached, desired) {
        (false, true) => Some(ProviderReconciliation::Attach),
        (true, false) => Some(ProviderReconciliation::Detach),
        (false, false) | (true, true) => None,
    }
}

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
fn execution_binding_label_value(binding_digest: &str) -> String {
    let digest = binding_digest
        .strip_prefix("sha256:")
        .and_then(|value| (value.len() == 64).then_some(value))
        .unwrap_or(binding_digest);
    let digest = Sha256::digest(digest.as_bytes());
    let mut label = String::with_capacity(63);
    label.push_str("sha256-");
    for byte in digest.iter().take(28) {
        label.push(char::from(LOWER_HEX[usize::from(byte >> 4)]));
        label.push(char::from(LOWER_HEX[usize::from(byte & 0x0f)]));
    }
    label
}

#[cfg(feature = "runtime")]
struct OpenShellProjection {
    workspace: String,
    workspace_key: String,
    sandbox: String,
    providers: Vec<String>,
    runtime_uid: String,
    image: Option<String>,
    execution_binding_digest: Option<String>,
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
fn deletion_names(request: &SandboxRequest) -> (String, String) {
    match (&request.refs.workspace, &request.refs.sandbox) {
        (Some(workspace), Some(sandbox)) if !workspace.is_empty() && !sandbox.is_empty() => {
            (workspace.clone(), sandbox.clone())
        }
        _ => (
            stable_name(NameKind::Workspace, request.workspace_key.as_bytes()),
            stable_name(NameKind::Sandbox, request.runtime.0.as_bytes()),
        ),
    }
}

#[cfg(feature = "runtime")]
fn bridge_image_for_execution(
    agent_type: &AgentType,
    execution_class: SandboxExecutionClass,
    execution_binding: Option<&steward_types::DisposableExecutionBinding>,
    stable_bridge_image: Option<&str>,
    connections_bridge_image: Option<&str>,
) -> Result<Option<String>, PortError> {
    if let Some(binding) = execution_binding {
        validate_disposable_execution_binding(binding)?;
        if execution_class != SandboxExecutionClass::Agent || binding.agent_ref != agent_type.name {
            return Err(PortError::Rejected {
                reason: "persisted execution binding does not match the requested agent runtime"
                    .to_owned(),
            });
        }
        return Ok(Some(binding.image.clone()));
    }
    match (agent_type.name.as_str(), execution_class) {
        ("base", SandboxExecutionClass::Agent) => Ok(None),
        (CONNECTIONS_BRIDGE_AGENT_TYPE, SandboxExecutionClass::Agent) => stable_bridge_image
            .filter(|image| is_digest_pinned_image(image))
            .map(str::to_owned)
            .map(Some)
            .ok_or_else(|| PortError::Rejected {
                reason: "Stable connections bridge requires its own provenance-verified digest-pinned image"
                    .to_owned(),
            }),
        (CONNECTIONS_BRIDGE_AGENT_TYPE, SandboxExecutionClass::ProviderControl) => connections_bridge_image
            .filter(|image| is_digest_pinned_image(image))
            .map(str::to_owned)
            .map(Some)
            .ok_or_else(|| PortError::Rejected {
                reason: "Governed Connections bridge requires its own provenance-verified digest-pinned image"
                    .to_owned(),
            }),
        (other, _) => Err(PortError::Rejected {
            reason: format!("unsupported agent type and execution class: {other}"),
        }),
    }
}

#[cfg(feature = "runtime")]
fn validate_disposable_execution_binding(
    binding: &steward_types::DisposableExecutionBinding,
) -> Result<(), PortError> {
    binding
        .validate()
        .map_err(|reason| PortError::Rejected { reason })?;
    let mut digest = Sha256::new();
    digest.update(steward_types::TASK_EXECUTION_BINDING_DIGEST_DOMAIN);
    digest.update(
        binding
            .canonical_content()
            .map_err(|reason| PortError::Rejected { reason })?,
    );
    if format!("sha256:{:x}", digest.finalize()) != binding.binding_digest {
        return Err(PortError::Rejected {
            reason: "persisted execution binding digest does not match its content".to_owned(),
        });
    }
    Ok(())
}

#[cfg(feature = "runtime")]
fn output_archive_command(
    agent_type: &AgentType,
    execution_binding: Option<&steward_types::DisposableExecutionBinding>,
) -> &'static str {
    if agent_type.name == CONNECTIONS_BRIDGE_AGENT_TYPE {
        "set -eu; test -f /sandbox/steward-output/response.json; tar -cf - -C /sandbox/steward-output response.json"
    } else if execution_binding.is_some() {
        "set -eu; test -s /sandbox/steward-output/result.txt; test -d /sandbox/steward-output/out; tar -cf - -C /sandbox/steward-output out"
    } else {
        "set -eu; tar -cf - -C /sandbox/steward-output ."
    }
}

#[cfg(feature = "runtime")]
fn task_agent_failure_category(stderr: &[u8]) -> &'static str {
    let stderr = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    if stderr.contains("steward-connections-bridge:")
        && stderr.contains("mcp-gw rejected runtime authentication")
    {
        "bridge-runtime-authentication"
    } else if stderr.contains("steward-connections-bridge:")
        && stderr.contains("mcp-gw rejected runtime authorization")
    {
        "bridge-runtime-authorization"
    } else if stderr.contains("steward-connections-bridge:")
        && stderr.contains("mcp-gw response violated its bounded contract")
    {
        "bridge-response-contract"
    } else if stderr.contains("steward-connections-bridge:")
        && stderr.contains("mcp-gw transport is unavailable")
    {
        "bridge-gateway-transport"
    } else if stderr.contains("steward-connections-bridge:")
        && stderr.contains("mcp-gw returned an unexpected status")
    {
        "bridge-gateway-status"
    } else if stderr.contains("steward-connections-bridge:")
        && stderr.contains("mcp-gw response body is unavailable")
    {
        "bridge-gateway-body"
    } else if stderr.contains("steward-connections-bridge:")
        && stderr.contains("mcp-gw is unavailable")
    {
        "bridge-gateway-unavailable"
    } else if stderr.contains("steward-connections-bridge:")
        && (stderr.contains("request.json input is unavailable")
            || stderr.contains("request.json input is invalid")
            || stderr.contains("request.json input is unreadable"))
    {
        "bridge-input"
    } else if stderr.contains("steward-connections-bridge:")
        && (stderr.contains("invocation must contain")
            || stderr.contains("operation is not allowlisted")
            || stderr.contains("request.json violates the operation contract"))
    {
        "bridge-contract"
    } else if stderr.contains("steward-connections-bridge:")
        && (stderr.contains("steward_mcp_gw_origin") || stderr.contains("steward_output_dir"))
    {
        "bridge-configuration"
    } else if stderr.contains("steward-connections-bridge:")
        && stderr.contains("did not receive a valid mcp-gw response")
    {
        "bridge-gateway"
    } else if stderr.contains("steward-connections-bridge:")
        && (stderr.contains("response could not be serialized")
            || stderr.contains("response.json could not be persisted"))
    {
        "bridge-output"
    } else if stderr.contains("policy_denied") || stderr.contains("policy denied") {
        "policy"
    } else if stderr.contains("unauthorized")
        || stderr.contains("forbidden")
        || stderr.contains("status 401")
        || stderr.contains("status 403")
        || stderr.contains("invalid api key")
    {
        "authentication"
    } else if stderr.contains("config.toml")
        || stderr.contains("error loading configuration")
        || stderr.contains("failed to load configuration")
        || stderr.contains("toml parse error")
    {
        "configuration"
    } else if stderr.contains("unexpected argument")
        || stderr.contains("usage: codex exec")
        || stderr.contains("invalid value")
    {
        "cli-usage"
    } else if stderr.contains("model")
        && (stderr.contains("not found")
            || stderr.contains("unsupported")
            || stderr.contains("unknown"))
    {
        "model"
    } else if stderr.contains("error sending request")
        || stderr.contains("connection refused")
        || stderr.contains("connection reset")
        || stderr.contains("timed out")
        || stderr.contains("dns error")
        || stderr.contains("failed to connect")
        || stderr.contains("stream disconnected")
    {
        "network"
    } else {
        "agent"
    }
}

#[cfg(feature = "runtime")]
fn is_digest_pinned_image(value: &str) -> bool {
    let Some((repository, digest)) = value.rsplit_once('@') else {
        return false;
    };
    !repository.is_empty()
        && digest.starts_with("sha256:")
        && digest.len() == "sha256:".len() + 64
        && digest["sha256:".len()..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(feature = "runtime")]
fn is_operator_pinned_image(value: &str) -> bool {
    if value
        .bytes()
        .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
        || value.matches('@').count() != 1
        || value.contains("://")
    {
        return false;
    }
    let Some((repository, digest)) = value.split_once('@') else {
        return false;
    };
    let mut components = repository.split('/');
    let Some(registry) = components.next() else {
        return false;
    };
    let (registry, port) = registry
        .split_once(':')
        .map_or((registry, None), |(registry, port)| (registry, Some(port)));
    let valid_component = |component: &str| {
        !component.is_empty()
            && component.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'_' | b'-')
            })
    };
    if !valid_component(registry)
        || port
            .is_some_and(|port| port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()))
        || components.any(|component| !valid_component(component))
    {
        return false;
    }
    digest.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

#[cfg(feature = "runtime")]
fn validate_connections_bridge_archive(
    archive: &[u8],
    expected_file: &str,
) -> Result<(), PortError> {
    let reject = || PortError::Rejected {
        reason: "Connections bridge archive must contain exactly one expected regular file"
            .to_owned(),
    };
    if expected_file.is_empty()
        || archive.len() < TAR_BLOCK_BYTES * 3
        || !archive.len().is_multiple_of(TAR_BLOCK_BYTES)
    {
        return Err(reject());
    }
    let header = &archive[..TAR_BLOCK_BYTES];
    if header.iter().all(|byte| *byte == 0)
        || tar_checksum(header).is_none()
        || header[156] != 0 && header[156] != b'0'
        || header[345..500].iter().any(|byte| *byte != 0)
        || header[157..257].iter().any(|byte| *byte != 0)
        || tar_string(&header[..100]) != Some(expected_file)
    {
        return Err(reject());
    }
    let size = tar_octal(&header[124..136]).ok_or_else(reject)?;
    let body_end = TAR_BLOCK_BYTES
        .checked_add(size)
        .and_then(|end| end.checked_add(TAR_BLOCK_BYTES - 1))
        .map(|end| end / TAR_BLOCK_BYTES * TAR_BLOCK_BYTES)
        .ok_or_else(reject)?;
    if body_end > archive.len() - TAR_BLOCK_BYTES * 2
        || archive[body_end..].iter().any(|byte| *byte != 0)
    {
        return Err(reject());
    }
    Ok(())
}

#[cfg(feature = "runtime")]
fn tar_checksum(header: &[u8]) -> Option<()> {
    if header.len() != TAR_BLOCK_BYTES {
        return None;
    }
    tar_octal(&header[148..156]).and_then(|expected| {
        let actual = header
            .iter()
            .enumerate()
            .map(|(index, byte)| {
                u32::from(if (148..156).contains(&index) {
                    b' '
                } else {
                    *byte
                })
            })
            .sum::<u32>();
        (u32::try_from(expected).ok()? == actual).then_some(())
    })
}

#[cfg(feature = "runtime")]
fn tar_octal(field: &[u8]) -> Option<usize> {
    let field = field
        .strip_prefix(&[b' '][..])
        .unwrap_or(field)
        .split(|byte| *byte == 0 || *byte == b' ')
        .next()
        .filter(|value| !value.is_empty())?;
    field.iter().try_fold(0_usize, |value, byte| {
        (b'0'..=b'7')
            .contains(byte)
            .then(|| value.checked_mul(8)?.checked_add(usize::from(*byte - b'0')))
            .flatten()
    })
}

#[cfg(feature = "runtime")]
fn tar_string(field: &[u8]) -> Option<&str> {
    let nul = field.iter().position(|byte| *byte == 0)?;
    field[nul..].iter().all(|byte| *byte == 0).then_some(())?;
    std::str::from_utf8(&field[..nul]).ok()
}

#[cfg(feature = "runtime")]
fn project_request(
    request: &SandboxRequest,
    stable_bridge_image: Option<&str>,
    connections_bridge_image: Option<&str>,
) -> Result<OpenShellProjection, PortError> {
    let image = bridge_image_for_execution(
        &request.agent_type,
        request.execution_class,
        request.execution_binding.as_ref(),
        stable_bridge_image,
        connections_bridge_image,
    )?;
    let (tool_provider, inference_provider) = request.execution_binding.as_ref().map_or(
        (Some(TOOL_PROVIDER), Some(INFERENCE_PROVIDER)),
        |binding| {
            (
                binding
                    .provider_profiles
                    .tools
                    .as_ref()
                    .map(|profile| profile.id.as_str()),
                binding
                    .provider_profiles
                    .inference
                    .as_ref()
                    .map(|profile| profile.id.as_str()),
            )
        },
    );
    if !request.tools.is_empty() && tool_provider.is_none() {
        return Err(PortError::Rejected {
            reason: "persisted execution binding has no tool provider profile".to_owned(),
        });
    }
    if !request.models.is_empty() && inference_provider.is_none() {
        return Err(PortError::Rejected {
            reason: "persisted execution binding has no inference provider profile".to_owned(),
        });
    }
    Ok(OpenShellProjection {
        workspace: stable_name(NameKind::Workspace, request.workspace_key.as_bytes()),
        workspace_key: request.workspace_key.clone(),
        sandbox: stable_name(NameKind::Sandbox, request.runtime.0.as_bytes()),
        providers: [
            (!request.tools.is_empty()).then(|| tool_provider.unwrap_or_default().to_owned()),
            (!request.models.is_empty()).then(|| inference_provider.unwrap_or_default().to_owned()),
        ]
        .into_iter()
        .flatten()
        .collect(),
        runtime_uid: request.runtime.0.clone(),
        image,
        execution_binding_digest: request
            .execution_binding
            .as_ref()
            .map(|binding| binding.binding_digest.clone()),
    })
}

#[cfg(feature = "runtime")]
fn sandbox_spec(projection: &OpenShellProjection) -> SandboxSpec {
    let mut labels = HashMap::new();
    labels.insert(RUNTIME_UID_LABEL.to_owned(), projection.runtime_uid.clone());
    if let Some(binding_digest) = &projection.execution_binding_digest {
        labels.insert(
            EXECUTION_BINDING_LABEL.to_owned(),
            execution_binding_label_value(binding_digest),
        );
    }
    SandboxSpec {
        name: Some(projection.sandbox.clone()),
        image: projection.image.clone(),
        labels,
        providers: projection.providers.clone(),
        ..SandboxSpec::default()
    }
}

#[cfg(feature = "runtime")]
fn valid_kubernetes_runtime_class_name(value: &str) -> bool {
    if value.is_empty() || value.len() > KUBERNETES_DNS_SUBDOMAIN_MAX_LENGTH {
        return false;
    }
    value.split('.').all(|label| {
        if label.is_empty() || label.len() > KUBERNETES_DNS_LABEL_MAX_LENGTH {
            return false;
        }
        let bytes = label.as_bytes();
        let valid_boundary = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
        valid_boundary(bytes[0])
            && valid_boundary(bytes[bytes.len() - 1])
            && bytes
                .iter()
                .all(|byte| valid_boundary(*byte) || *byte == b'-')
    })
}

#[cfg(feature = "runtime")]
/// Controls whether a task process is mirrored into the controller's ordinary log stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenShellTaskLogMode {
    /// Retain task output for result handling without emitting it to controller logs.
    Off,
    /// Emit escaped stdout and stderr events live while retaining the result buffers.
    Full,
}

#[cfg(feature = "runtime")]
fn task_log_mode_for_execution(
    configured: OpenShellTaskLogMode,
    execution_class: SandboxExecutionClass,
) -> OpenShellTaskLogMode {
    if execution_class == SandboxExecutionClass::ProviderControl {
        OpenShellTaskLogMode::Off
    } else {
        configured
    }
}

#[cfg(feature = "runtime")]
#[derive(Debug, Eq, PartialEq)]
struct TaskProcessResult {
    exit_code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[cfg(feature = "runtime")]
#[derive(Clone, Copy)]
struct TaskProcessLogContext<'a> {
    runtime_uid: &'a str,
    workspace: &'a str,
    sandbox: &'a str,
}

#[cfg(feature = "runtime")]
#[derive(Clone, Copy)]
enum TaskProcessStream {
    Stdout,
    Stderr,
}

#[cfg(feature = "runtime")]
impl TaskProcessStream {
    fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

#[cfg(feature = "runtime")]
trait TaskProcessEventStream {
    async fn next_event(&mut self) -> Result<Option<ExecSandboxEvent>, PortError>;
}

#[cfg(feature = "runtime")]
impl TaskProcessEventStream for Streaming<ExecSandboxEvent> {
    async fn next_event(&mut self) -> Result<Option<ExecSandboxEvent>, PortError> {
        self.message().await.map_err(raw_port_failure)
    }
}

#[cfg(feature = "runtime")]
trait TaskProcessLogSink {
    fn emit(
        &mut self,
        context: TaskProcessLogContext<'_>,
        stream: TaskProcessStream,
        message: &[u8],
    );
}

#[cfg(feature = "runtime")]
struct ControllerTaskProcessLogSink;

#[cfg(feature = "runtime")]
impl TaskProcessLogSink for ControllerTaskProcessLogSink {
    fn emit(
        &mut self,
        context: TaskProcessLogContext<'_>,
        stream: TaskProcessStream,
        message: &[u8],
    ) {
        eprintln!("{}", task_process_log_record(context, stream, message));
    }
}

#[cfg(feature = "runtime")]
fn task_process_log_record(
    context: TaskProcessLogContext<'_>,
    stream: TaskProcessStream,
    message: &[u8],
) -> String {
    format!(
        "openshell_task_process runtime_uid=\"{}\" workspace=\"{}\" sandbox=\"{}\" stream={} message=\"{}\"",
        escaped_task_log_value(context.runtime_uid.as_bytes()),
        escaped_task_log_value(context.workspace.as_bytes()),
        escaped_task_log_value(context.sandbox.as_bytes()),
        stream.as_str(),
        escaped_task_log_value(message),
    )
}

#[cfg(feature = "runtime")]
fn escaped_task_log_value(value: &[u8]) -> String {
    fn append_valid_utf8(output: &mut String, value: &str) {
        for character in value.chars() {
            match character {
                '\\' => output.push_str("\\\\"),
                '"' => output.push_str("\\\""),
                '\n' => output.push_str("\\n"),
                '\r' => output.push_str("\\r"),
                '\t' => output.push_str("\\t"),
                character if character.is_control() => output.extend(character.escape_default()),
                character => output.push(character),
            }
        }
    }

    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(value.len());
    let mut remaining = value;
    while !remaining.is_empty() {
        match std::str::from_utf8(remaining) {
            Ok(valid) => {
                append_valid_utf8(&mut output, valid);
                break;
            }
            Err(error) => {
                let valid_bytes = &remaining[..error.valid_up_to()];
                if let Ok(valid) = std::str::from_utf8(valid_bytes) {
                    append_valid_utf8(&mut output, valid);
                }
                let invalid_length = error
                    .error_len()
                    .unwrap_or(remaining.len() - error.valid_up_to());
                let invalid_end = error.valid_up_to() + invalid_length;
                for byte in &remaining[error.valid_up_to()..invalid_end] {
                    output.push_str("\\x");
                    output.push(char::from(HEX[usize::from(byte >> 4)]));
                    output.push(char::from(HEX[usize::from(byte & 0x0f)]));
                }
                remaining = &remaining[invalid_end..];
            }
        }
    }
    output
}

#[cfg(feature = "runtime")]
async fn collect_task_process_stream<S, L>(
    stream: &mut S,
    mode: OpenShellTaskLogMode,
    context: TaskProcessLogContext<'_>,
    log_sink: &mut L,
) -> Result<TaskProcessResult, PortError>
where
    S: TaskProcessEventStream,
    L: TaskProcessLogSink,
{
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut exit_code = None;

    while let Some(event) = stream.next_event().await? {
        match event.payload {
            Some(exec_sandbox_event::Payload::Stdout(chunk)) => {
                if mode == OpenShellTaskLogMode::Full {
                    log_sink.emit(context, TaskProcessStream::Stdout, &chunk.data);
                }
                stdout.extend_from_slice(&chunk.data);
            }
            Some(exec_sandbox_event::Payload::Stderr(chunk)) => {
                if mode == OpenShellTaskLogMode::Full {
                    log_sink.emit(context, TaskProcessStream::Stderr, &chunk.data);
                }
                stderr.extend_from_slice(&chunk.data);
            }
            Some(exec_sandbox_event::Payload::Exit(exit)) => {
                exit_code = Some(exit.exit_code);
            }
            None => {}
        }
    }

    Ok(TaskProcessResult {
        exit_code: exit_code.unwrap_or(-1),
        stdout,
        stderr,
    })
}

#[cfg(feature = "runtime")]
#[derive(Clone)]
pub struct OpenShellConnectionConfig {
    pub endpoint: String,
    pub ca_certificate_pem: Vec<u8>,
    pub client_certificate_pem: Vec<u8>,
    pub client_private_key_pem: Vec<u8>,
    pub workload_exchange_endpoint: String,
    pub workload_exchange_server_name: String,
    pub workload_exchange_ca_certificate_pem: Vec<u8>,
    pub workload_source_credential_file: PathBuf,
    pub server_name: String,
    pub runtime_class_name: String,
    pub task_log_mode: OpenShellTaskLogMode,
    /// Existing stable-runtime bridge image. It remains isolated from one-shot provider control.
    pub stable_bridge_image: Option<String>,
    /// Existing stable-runtime bridge MCP-GW origin.
    pub stable_bridge_gateway_origin: Option<String>,
    /// Digest-pinned governed Connections bridge image. Its provenance is verified by the
    /// controller process before this adapter is constructed.
    pub bridge_image: Option<String>,
    /// Server-selected artifact trust mode paired with the governed Connections bridge image.
    pub bridge_artifact_trust_mode: Option<String>,
    /// Server-configured gateway origin passed only to the fixed bridge executable.
    pub bridge_gateway_origin: Option<String>,
    /// Exact MCP-GW release whose OAuth lifetime is pinned by the immutable authority.
    pub bridge_gateway_version: Option<String>,
    /// Namespace pinned into governed provider-control operations. It is paired with the bridge
    /// image and gateway origin so persisted operations can fail closed on rollout drift.
    pub bridge_runtime_namespace: Option<String>,
}

#[cfg(feature = "runtime")]
impl OpenShellConnectionConfig {
    fn validate(&self) -> Result<(), PortError> {
        if !self.endpoint.starts_with("https://") {
            return Err(PortError::Rejected {
                reason: "OpenShell gateway endpoint must use verified HTTPS".to_owned(),
            });
        }
        if self.server_name.trim().is_empty() {
            return Err(PortError::Rejected {
                reason: "OpenShell gateway TLS server name is required".to_owned(),
            });
        }
        for (description, material) in [
            ("CA certificate", self.ca_certificate_pem.as_slice()),
            ("client certificate", self.client_certificate_pem.as_slice()),
            ("client private key", self.client_private_key_pem.as_slice()),
        ] {
            if material.is_empty() {
                return Err(PortError::Rejected {
                    reason: format!("OpenShell gateway {description} is required"),
                });
            }
        }
        if self.workload_source_credential_file.as_os_str().is_empty() {
            return Err(PortError::Rejected {
                reason: "workload source credential file is required".to_owned(),
            });
        }
        validate_workload_exchange_endpoint(
            &self.workload_exchange_endpoint,
            &self.workload_exchange_server_name,
        )?;
        if self.workload_exchange_ca_certificate_pem.is_empty() {
            return Err(PortError::Rejected {
                reason: "workload exchange CA certificate is required".to_owned(),
            });
        }
        if !valid_kubernetes_runtime_class_name(&self.runtime_class_name) {
            return Err(PortError::Rejected {
                reason: "OpenShell gateway runtime class must be a valid Kubernetes DNS subdomain"
                    .to_owned(),
            });
        }
        match (
            self.stable_bridge_image.as_deref(),
            self.stable_bridge_gateway_origin.as_deref(),
        ) {
            (None, None) => {}
            (Some(image), Some(origin)) => {
                if !is_digest_pinned_image(image) {
                    return Err(PortError::Rejected {
                        reason: "Stable bridge image must be an immutable sha256 reference"
                            .to_owned(),
                    });
                }
                validate_connections_bridge_gateway_origin(origin)?;
            }
            _ => {
                return Err(PortError::Rejected {
                    reason: "Stable bridge image and gateway origin must be configured together"
                        .to_owned(),
                });
            }
        }
        match (
            self.bridge_image.as_deref(),
            self.bridge_artifact_trust_mode.as_deref(),
            self.bridge_gateway_origin.as_deref(),
            self.bridge_gateway_version.as_deref(),
            self.bridge_runtime_namespace.as_deref(),
        ) {
            (None, None, None, None, None) => {}
            (Some(image), Some(mode), Some(origin), Some(version), Some(namespace)) => {
                let image_is_valid = match mode {
                    "github-attestation" => is_digest_pinned_image(image),
                    "operator-pinned" => is_operator_pinned_image(image),
                    _ => false,
                };
                if !image_is_valid {
                    return Err(PortError::Rejected {
                        reason: "Connections bridge trust mode and immutable image reference are invalid"
                            .to_owned(),
                    });
                }
                validate_connections_bridge_gateway_origin(origin)?;
                if version != "0.3.2" {
                    return Err(PortError::Rejected {
                        reason: "Connections bridge requires the pinned MCP-GW 0.3.2 contract"
                            .to_owned(),
                    });
                }
                if namespace.trim().is_empty() || namespace.trim() != namespace {
                    return Err(PortError::Rejected {
                        reason: "Connections bridge runtime namespace must be exact and non-empty"
                            .to_owned(),
                    });
                }
            }
            _ => {
                return Err(PortError::Rejected {
                    reason:
                        "Connections bridge image, trust mode, gateway origin/version, and runtime namespace must be configured together"
                            .to_owned(),
                });
            }
        }
        Ok(())
    }
}

#[cfg(feature = "runtime")]
/// Rejects every value other than one exact HTTP(S) origin before a bridge runtime is created.
///
/// The controller calls this while loading its configuration so malformed gateway targets cannot
/// reach the OpenShell connection path.
pub fn validate_connections_bridge_gateway_origin(value: &str) -> Result<(), PortError> {
    if value.trim() != value {
        return Err(PortError::Rejected {
            reason: "Connections bridge gateway origin must be an exact HTTP(S) origin".to_owned(),
        });
    }
    let origin = Url::parse(value).map_err(|_| PortError::Rejected {
        reason: "Connections bridge gateway origin must be an exact HTTP(S) origin".to_owned(),
    })?;
    if !matches!(origin.scheme(), "http" | "https")
        || origin.host_str().is_none()
        || !origin.username().is_empty()
        || origin.password().is_some()
        || origin.path() != "/"
        || origin.query().is_some()
        || origin.fragment().is_some()
        || origin.port() == Some(0)
    {
        return Err(PortError::Rejected {
            reason: "Connections bridge gateway origin must be an exact HTTP(S) origin".to_owned(),
        });
    }
    Ok(())
}

#[cfg(feature = "runtime")]
fn load_source_credential(path: &Path) -> Result<OpaqueToken, PortError> {
    let token = std::fs::read_to_string(path).map_err(|_| PortError::Rejected {
        reason: "workload source credential is unavailable".to_owned(),
    })?;
    let token = token.trim();
    if token.is_empty() {
        return Err(PortError::Rejected {
            reason: "workload source credential is empty".to_owned(),
        });
    }
    Ok(OpaqueToken(token.to_owned()))
}

#[cfg(feature = "runtime")]
fn validate_workload_exchange_endpoint(
    endpoint: &str,
    server_name: &str,
) -> Result<Url, PortError> {
    let url = Url::parse(endpoint).map_err(|_| PortError::Rejected {
        reason: "workload exchange endpoint is invalid".to_owned(),
    })?;
    if url.scheme() != "https"
        || url.path() != WORKLOAD_EXCHANGE_PATH
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(PortError::Rejected {
            reason: format!(
                "workload exchange endpoint must be verified HTTPS at {WORKLOAD_EXCHANGE_PATH}"
            ),
        });
    }
    if server_name.trim().is_empty() || url.host_str() != Some(server_name) {
        return Err(PortError::Rejected {
            reason: "workload exchange server name must exactly match the endpoint host".to_owned(),
        });
    }
    Ok(url)
}

#[cfg(feature = "runtime")]
struct OpaqueToken(String);

#[cfg(feature = "runtime")]
impl OpaqueToken {
    fn expose_secret(&self) -> &str {
        &self.0
    }
}

#[cfg(feature = "runtime")]
impl Clone for OpaqueToken {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

#[cfg(feature = "runtime")]
struct CachedAccessToken {
    token: OpaqueToken,
    refresh_at: Instant,
}

#[cfg(feature = "runtime")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkloadExchangeResponse {
    access_token: String,
    token_type: String,
    expires_in: u64,
}

#[cfg(feature = "runtime")]
#[derive(Clone)]
struct WorkloadExchangeTokenProvider {
    cache: Arc<Mutex<Option<CachedAccessToken>>>,
    client: HttpClient,
    endpoint: Url,
    source_credential_file: PathBuf,
}

#[cfg(feature = "runtime")]
impl WorkloadExchangeTokenProvider {
    fn new(config: &OpenShellConnectionConfig) -> Result<Self, PortError> {
        let endpoint = validate_workload_exchange_endpoint(
            &config.workload_exchange_endpoint,
            &config.workload_exchange_server_name,
        )?;
        let certificates =
            HttpCertificate::from_pem_bundle(&config.workload_exchange_ca_certificate_pem)
                .map_err(|_| PortError::Rejected {
                    reason: "workload exchange CA certificate is invalid".to_owned(),
                })?;
        if certificates.is_empty() {
            return Err(PortError::Rejected {
                reason: "workload exchange CA certificate is empty".to_owned(),
            });
        }
        let mut builder = HttpClient::builder()
            .https_only(true)
            .tls_built_in_root_certs(false)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(StdDuration::from_secs(5))
            .timeout(StdDuration::from_secs(10));
        for certificate in certificates {
            builder = builder.add_root_certificate(certificate);
        }
        let client = builder.build().map_err(|_| PortError::Rejected {
            reason: "workload exchange HTTPS client configuration is invalid".to_owned(),
        })?;
        Ok(Self {
            cache: Arc::new(Mutex::new(None)),
            client,
            endpoint,
            source_credential_file: config.workload_source_credential_file.clone(),
        })
    }

    async fn access_token(&self) -> Result<OpaqueToken, PortError> {
        let mut cache = self.cache.lock().await;
        let now = Instant::now();
        if let Some(cached) = cache.as_ref().filter(|cached| now < cached.refresh_at) {
            return Ok(cached.token.clone());
        }
        let source_file = self.source_credential_file.clone();
        let source = tokio::task::spawn_blocking(move || load_source_credential(&source_file))
            .await
            .map_err(|_| PortError::Failed {
                reason: "workload source credential could not be loaded".to_owned(),
            })??;
        let mut response = self
            .client
            .post(self.endpoint.clone())
            .bearer_auth(source.expose_secret())
            .send()
            .await
            .map_err(workload_exchange_unavailable)?;
        if response.status() != StatusCode::OK {
            return Err(if response.status().is_client_error() {
                PortError::Rejected {
                    reason: "workload exchange rejected the source credential".to_owned(),
                }
            } else {
                PortError::Failed {
                    reason: "workload exchange is unavailable".to_owned(),
                }
            });
        }
        let cache_control = response
            .headers()
            .get(CACHE_CONTROL)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !cache_control
            .split(',')
            .any(|directive| directive.trim().eq_ignore_ascii_case("no-store"))
        {
            return Err(PortError::Rejected {
                reason: "workload exchange response must forbid storage".to_owned(),
            });
        }
        if response
            .content_length()
            .is_some_and(|length| length > WORKLOAD_EXCHANGE_MAX_RESPONSE_BYTES)
        {
            return Err(PortError::Rejected {
                reason: "workload exchange response is too large".to_owned(),
            });
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(workload_exchange_unavailable)?
        {
            let next_length = body.len().saturating_add(chunk.len());
            if u64::try_from(next_length).unwrap_or(u64::MAX) > WORKLOAD_EXCHANGE_MAX_RESPONSE_BYTES
            {
                return Err(PortError::Rejected {
                    reason: "workload exchange response is too large".to_owned(),
                });
            }
            body.extend_from_slice(&chunk);
        }
        let exchanged =
            serde_json::from_slice::<WorkloadExchangeResponse>(&body).map_err(|_| {
                PortError::Rejected {
                    reason: "workload exchange response is invalid".to_owned(),
                }
            })?;
        if exchanged.token_type != "Bearer"
            || exchanged.access_token.trim().is_empty()
            || exchanged.access_token.trim() != exchanged.access_token
            || exchanged.expires_in == 0
            || exchanged.expires_in > WORKLOAD_ACCESS_TOKEN_MAX_TTL.as_secs()
        {
            return Err(PortError::Rejected {
                reason: "workload exchange access token contract is invalid".to_owned(),
            });
        }
        let token = OpaqueToken(exchanged.access_token);
        let ttl = StdDuration::from_secs(exchanged.expires_in);
        let refresh_at = now + ttl.saturating_sub(WORKLOAD_ACCESS_TOKEN_REFRESH_SKEW);
        *cache = Some(CachedAccessToken {
            token: token.clone(),
            refresh_at,
        });
        Ok(token)
    }
}

#[cfg(feature = "runtime")]
fn workload_exchange_unavailable(_: reqwest::Error) -> PortError {
    PortError::Failed {
        reason: "workload exchange is unavailable".to_owned(),
    }
}

#[cfg(feature = "runtime")]
#[derive(Clone)]
pub struct OpenShellRuntime {
    channel: Channel,
    token_provider: WorkloadExchangeTokenProvider,
    task_log_mode: OpenShellTaskLogMode,
    stable_bridge_image: Option<String>,
    stable_bridge_gateway_origin: Option<String>,
    bridge_image: Option<String>,
    bridge_artifact_trust_mode: Option<String>,
    bridge_gateway_origin: Option<String>,
    bridge_gateway_version: Option<String>,
    runtime_class_name: String,
    bridge_runtime_namespace: Option<String>,
}

#[cfg(feature = "runtime")]
fn provider_control_execution_bindings(
    runtime: &OpenShellRuntime,
) -> Option<ProviderControlExecutionBindings> {
    Some(ProviderControlExecutionBindings {
        artifact_trust_mode: runtime.bridge_artifact_trust_mode.clone()?,
        bridge_image_digest: runtime.bridge_image.clone()?,
        mcp_gw_origin: runtime.bridge_gateway_origin.clone()?,
        mcp_gw_version: runtime.bridge_gateway_version.clone()?,
        namespace: runtime.bridge_runtime_namespace.clone()?,
        runtime_class: runtime.runtime_class_name.clone(),
    })
}

#[cfg(feature = "runtime")]
impl OpenShellRuntime {
    pub async fn connect(config: OpenShellConnectionConfig) -> Result<Self, PortError> {
        config.validate()?;
        let token_provider = WorkloadExchangeTokenProvider::new(&config)?;
        let tls = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(config.ca_certificate_pem))
            .identity(Identity::from_pem(
                config.client_certificate_pem,
                config.client_private_key_pem,
            ))
            .domain_name(config.server_name);
        let channel = Endpoint::from_shared(config.endpoint)
            .map_err(raw_port_failure)?
            .connect_timeout(StdDuration::from_secs(10))
            .tls_config(tls)
            .map_err(raw_port_failure)?
            .connect()
            .await
            .map_err(raw_port_failure)?;
        let runtime = Self {
            channel,
            token_provider,
            task_log_mode: config.task_log_mode,
            stable_bridge_image: config.stable_bridge_image,
            stable_bridge_gateway_origin: config.stable_bridge_gateway_origin,
            bridge_image: config.bridge_image,
            bridge_artifact_trust_mode: config.bridge_artifact_trust_mode,
            bridge_gateway_origin: config.bridge_gateway_origin,
            bridge_gateway_version: config.bridge_gateway_version,
            runtime_class_name: config.runtime_class_name,
            bridge_runtime_namespace: config.bridge_runtime_namespace,
        };
        runtime.authenticated_client().await?;
        Ok(runtime)
    }

    async fn authenticated_client(&self) -> Result<OpenShellClient, PortError> {
        let token = self.token_provider.access_token().await?;
        let interceptor =
            EdgeAuthInterceptor::new(Some(token.expose_secret()), None).map_err(|_| {
                PortError::Rejected {
                    reason: "OpenShell gateway caller token is invalid".to_owned(),
                }
            })?;
        Ok(OpenShellClient::from_parts(
            self.channel.clone(),
            interceptor,
        ))
    }

    async fn exec_task_process(
        &self,
        sandbox_id: &str,
        command: &[String],
        environment: HashMap<String, String>,
        execution_class: SandboxExecutionClass,
        context: TaskProcessLogContext<'_>,
    ) -> Result<TaskProcessResult, PortError> {
        let mut client = self.authenticated_client().await?.raw_grpc();
        let response = client
            .exec_sandbox(ExecSandboxRequest {
                sandbox_id: sandbox_id.to_owned(),
                command: command.to_vec(),
                workdir: "/sandbox/steward-input".to_owned(),
                environment,
                timeout_seconds: 30 * 60,
                stdin: Vec::new(),
                tty: false,
                cols: 0,
                rows: 0,
            })
            .await
            .map_err(raw_port_failure)?;
        let mut stream = response.into_inner();
        collect_task_process_stream(
            &mut stream,
            task_log_mode_for_execution(self.task_log_mode, execution_class),
            context,
            &mut ControllerTaskProcessLogSink,
        )
        .await
    }

    async fn stage_input_archive(
        &self,
        workspace: &str,
        sandbox: &str,
        input_archive: &[u8],
    ) -> Result<(), PortError> {
        let scoped = self.authenticated_client().await?.workspace(workspace);
        let prepare = scoped
            .exec(
                sandbox,
                &[
                    "/bin/sh".to_owned(),
                    "-c".to_owned(),
                    staging_prepare_command().to_owned(),
                ],
                ExecOptions {
                    timeout: Some(StdDuration::from_secs(120)),
                    ..ExecOptions::default()
                },
            )
            .await
            .map_err(port_failure)?;
        if prepare.exit_code != 0 {
            return Err(input_staging_rejected());
        }

        for chunk in staging_archive_chunks(input_archive) {
            let append = scoped
                .exec(
                    sandbox,
                    &[
                        "/bin/sh".to_owned(),
                        "-c".to_owned(),
                        staging_append_command().to_owned(),
                    ],
                    ExecOptions {
                        timeout: Some(StdDuration::from_secs(120)),
                        stdin: Some(chunk.to_vec()),
                        ..ExecOptions::default()
                    },
                )
                .await
                .map_err(port_failure)?;
            if append.exit_code != 0 {
                return Err(input_staging_rejected());
            }
        }

        let extract = scoped
            .exec(
                sandbox,
                &[
                    "/bin/sh".to_owned(),
                    "-c".to_owned(),
                    staging_extract_command().to_owned(),
                ],
                ExecOptions {
                    timeout: Some(StdDuration::from_secs(120)),
                    ..ExecOptions::default()
                },
            )
            .await
            .map_err(port_failure)?;
        if extract.exit_code != 0 {
            return Err(input_staging_rejected());
        }
        Ok(())
    }

    async fn resolve_raw_sandbox_binding(
        &self,
        workspace: &str,
        sandbox: &str,
        runtime_uid: &str,
        expected_image: Option<&str>,
        require_ready: bool,
    ) -> Result<String, PortError> {
        let mut client = self
            .authenticated_client()
            .await?
            .raw_grpc_fresh()
            .await
            .map_err(port_failure)?;
        let snapshot = client
            .get_sandbox(GetSandboxRequest {
                name: sandbox.to_owned(),
                workspace: workspace.to_owned(),
            })
            .await
            .map_err(raw_port_failure)?
            .into_inner()
            .sandbox
            .ok_or_else(|| PortError::Failed {
                reason: "OpenShell returned an empty sandbox response".to_owned(),
            })?;
        validate_raw_sandbox_binding(&snapshot, runtime_uid, expected_image, require_ready)?;
        snapshot
            .metadata
            .as_ref()
            .map(|metadata| metadata.id.clone())
            .filter(|sandbox_id| !sandbox_id.is_empty())
            .ok_or_else(|| PortError::Rejected {
                reason: "OpenShell raw sandbox has no stable ID".to_owned(),
            })
    }

    async fn ensure_workspace(&self, name: &str, workspace_key: &str) -> Result<(), PortError> {
        match self.authenticated_client().await?.get_workspace(name).await {
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
                match self
                    .authenticated_client()
                    .await?
                    .create_workspace(name, labels)
                    .await
                {
                    Ok(_) | Err(SdkError::AlreadyExists { .. }) => Ok(()),
                    Err(error) => Err(port_failure(error)),
                }
            }
            Err(error) => Err(port_failure(error)),
        }
    }

    async fn get_provider(
        &self,
        workspace: &str,
        provider_name: &str,
    ) -> Result<Option<Provider>, PortError> {
        let mut client = self.authenticated_client().await?.raw_grpc();
        match client
            .get_provider(GetProviderRequest {
                name: provider_name.to_owned(),
                workspace: workspace.to_owned(),
            })
            .await
        {
            Ok(response) => {
                response
                    .into_inner()
                    .provider
                    .map(Some)
                    .ok_or_else(|| PortError::Failed {
                        reason: "OpenShell returned an empty provider response".to_owned(),
                    })
            }
            Err(error) if error.code() as i32 == GRPC_NOT_FOUND => Ok(None),
            Err(error) => Err(raw_port_failure(error)),
        }
    }

    async fn ensure_provider(&self, workspace: &str, provider_name: &str) -> Result<(), PortError> {
        if let Some(provider) = self.get_provider(workspace, provider_name).await? {
            return validate_provider(&provider, workspace, provider_name);
        }

        let mut client = self.authenticated_client().await?.raw_grpc();
        let response = client
            .create_provider(CreateProviderRequest {
                provider: Some(Provider {
                    metadata: Some(ObjectMeta {
                        name: provider_name.to_owned(),
                        workspace: workspace.to_owned(),
                        ..ObjectMeta::default()
                    }),
                    r#type: provider_name.to_owned(),
                    profile_workspace: String::new(),
                    ..Provider::default()
                }),
                workspace: workspace.to_owned(),
            })
            .await;
        match response {
            Ok(response) => {
                let provider = response
                    .into_inner()
                    .provider
                    .ok_or_else(|| PortError::Failed {
                        reason: "OpenShell returned an empty provider response".to_owned(),
                    })?;
                validate_provider(&provider, workspace, provider_name)
            }
            Err(error) if error.code() as i32 == GRPC_ALREADY_EXISTS => {
                let provider = self
                    .get_provider(workspace, provider_name)
                    .await?
                    .ok_or_else(|| PortError::Failed {
                        reason: "OpenShell reported an existing provider that cannot be read"
                            .to_owned(),
                    })?;
                validate_provider(&provider, workspace, provider_name)
            }
            Err(error) => Err(raw_port_failure(error)),
        }
    }

    async fn attach_provider(
        &self,
        workspace: &str,
        sandbox: &str,
        provider_name: &str,
        resource_version: u64,
    ) -> Result<(), PortError> {
        let mut client = self.authenticated_client().await?.raw_grpc();
        client
            .attach_sandbox_provider(AttachSandboxProviderRequest {
                sandbox_name: sandbox.to_owned(),
                provider_name: provider_name.to_owned(),
                expected_resource_version: resource_version,
                workspace: workspace.to_owned(),
            })
            .await
            .map(|_| ())
            .map_err(raw_port_failure)
    }

    async fn provider_is_attached(
        &self,
        workspace: &str,
        sandbox: &str,
        provider_name: &str,
    ) -> Result<bool, PortError> {
        let mut client = self.authenticated_client().await?.raw_grpc();
        let providers = client
            .list_sandbox_providers(ListSandboxProvidersRequest {
                sandbox_name: sandbox.to_owned(),
                workspace: workspace.to_owned(),
            })
            .await
            .map_err(raw_port_failure)?
            .into_inner()
            .providers;
        let attached = providers
            .iter()
            .filter(|provider| {
                provider
                    .metadata
                    .as_ref()
                    .map(|metadata| metadata.name.as_str())
                    == Some(provider_name)
            })
            .collect::<Vec<_>>();
        let [provider] = attached.as_slice() else {
            return if attached.is_empty() {
                Ok(false)
            } else {
                Err(PortError::Rejected {
                    reason: "OpenShell returned duplicate Steward provider attachments".to_owned(),
                })
            };
        };
        validate_provider(provider, workspace, provider_name)?;
        Ok(true)
    }

    async fn detach_provider(
        &self,
        workspace: &str,
        sandbox: &str,
        provider_name: &str,
        resource_version: u64,
    ) -> Result<(), PortError> {
        let mut client = self.authenticated_client().await?.raw_grpc();
        client
            .detach_sandbox_provider(DetachSandboxProviderRequest {
                sandbox_name: sandbox.to_owned(),
                provider_name: provider_name.to_owned(),
                expected_resource_version: resource_version,
                workspace: workspace.to_owned(),
            })
            .await
            .map(|_| ())
            .map_err(raw_port_failure)
    }

    async fn reconcile_provider(
        &self,
        workspace: &str,
        sandbox: &str,
        provider_name: &str,
        desired: bool,
    ) -> Result<(), PortError> {
        let attached = self
            .provider_is_attached(workspace, sandbox, provider_name)
            .await?;
        match provider_reconciliation(attached, desired) {
            Some(ProviderReconciliation::Attach) => {
                let resource_version = self
                    .authenticated_client()
                    .await?
                    .workspace(workspace)
                    .get_sandbox(sandbox)
                    .await
                    .map_err(port_failure)?
                    .resource_version;
                self.attach_provider(workspace, sandbox, provider_name, resource_version)
                    .await
            }
            Some(ProviderReconciliation::Detach) => {
                let resource_version = self
                    .authenticated_client()
                    .await?
                    .workspace(workspace)
                    .get_sandbox(sandbox)
                    .await
                    .map_err(port_failure)?
                    .resource_version;
                self.detach_provider(workspace, sandbox, provider_name, resource_version)
                    .await
            }
            None => Ok(()),
        }
    }
}

#[cfg(feature = "runtime")]
fn validate_provider(
    provider: &Provider,
    workspace: &str,
    provider_name: &str,
) -> Result<(), PortError> {
    let metadata = provider
        .metadata
        .as_ref()
        .ok_or_else(|| PortError::Rejected {
            reason: "OpenShell provider has no identity metadata".to_owned(),
        })?;
    if provider.r#type != provider_name || metadata.workspace != workspace {
        return Err(PortError::Rejected {
            reason: "provider name resolved to a different type or workspace".to_owned(),
        });
    }
    Ok(())
}

#[cfg(feature = "runtime")]
fn validate_raw_sandbox_binding(
    snapshot: &RawSandbox,
    runtime_uid: &str,
    expected_image: Option<&str>,
    require_ready: bool,
) -> Result<(), PortError> {
    let metadata = snapshot
        .metadata
        .as_ref()
        .ok_or_else(|| PortError::Rejected {
            reason: "OpenShell raw sandbox has no identity metadata".to_owned(),
        })?;
    if metadata.labels.get(RUNTIME_UID_LABEL).map(String::as_str) != Some(runtime_uid) {
        return Err(PortError::Rejected {
            reason: "raw sandbox is bound to a different runtime UID".to_owned(),
        });
    }
    if let Some(expected_image) = expected_image {
        let actual_image = snapshot
            .spec
            .as_ref()
            .and_then(|spec| spec.template.as_ref())
            .map(|template| template.image.as_str());
        if actual_image != Some(expected_image) {
            return Err(PortError::Rejected {
                reason: "raw sandbox image does not match the provenance-verified digest"
                    .to_owned(),
            });
        }
    }
    if require_ready
        && snapshot.status.as_ref().map(|status| status.phase)
            != Some(RawSandboxPhase::Ready as i32)
    {
        return Err(PortError::Rejected {
            reason: "raw sandbox is not Ready for task execution".to_owned(),
        });
    }
    Ok(())
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
struct RuntimeDeleteClient<'a> {
    runtime: &'a OpenShellRuntime,
    workspace: &'a str,
}

#[cfg(feature = "runtime")]
impl SandboxDeleteClient for RuntimeDeleteClient<'_> {
    async fn sandbox_labels(
        &self,
        name: &str,
    ) -> Result<Option<HashMap<String, String>>, PortError> {
        match self
            .runtime
            .authenticated_client()
            .await?
            .workspace(self.workspace)
            .get_sandbox(name)
            .await
        {
            Ok(snapshot) => Ok(Some(snapshot.labels)),
            Err(SdkError::NotFound { .. }) => Ok(None),
            Err(error) => Err(port_failure(error)),
        }
    }

    async fn delete_sandbox(&self, name: &str) -> Result<bool, PortError> {
        match self
            .runtime
            .authenticated_client()
            .await?
            .workspace(self.workspace)
            .delete_sandbox(name)
            .await
        {
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
    fn provider_control_bindings(&self) -> Option<ProviderControlExecutionBindings> {
        provider_control_execution_bindings(self)
    }

    async fn ensure(&self, request: &SandboxRequest) -> Result<SandboxObservation, PortError> {
        let projection = project_request(
            request,
            self.stable_bridge_image.as_deref(),
            self.bridge_image.as_deref(),
        )?;
        self.ensure_workspace(&projection.workspace, &projection.workspace_key)
            .await?;
        for provider in &projection.providers {
            self.ensure_provider(&projection.workspace, provider)
                .await?;
        }
        let snapshot = match self
            .authenticated_client()
            .await?
            .workspace(&projection.workspace)
            .get_sandbox(&projection.sandbox)
            .await
        {
            Ok(snapshot) => snapshot,
            Err(SdkError::NotFound { .. }) => {
                match self
                    .authenticated_client()
                    .await?
                    .workspace(&projection.workspace)
                    .create_sandbox(sandbox_spec(&projection))
                    .await
                {
                    Ok(snapshot) => snapshot,
                    Err(SdkError::AlreadyExists { .. }) => self
                        .authenticated_client()
                        .await?
                        .workspace(&projection.workspace)
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
        let expected_execution_binding_label = projection
            .execution_binding_digest
            .as_deref()
            .map(execution_binding_label_value);
        if expected_execution_binding_label.as_ref() != snapshot.labels.get(EXECUTION_BINDING_LABEL)
        {
            return Err(PortError::Rejected {
                reason: "sandbox does not match the persisted execution binding".to_owned(),
            });
        }
        self.resolve_raw_sandbox_binding(
            &projection.workspace,
            &projection.sandbox,
            &projection.runtime_uid,
            projection.image.as_deref(),
            false,
        )
        .await?;
        for provider_name in &projection.providers {
            self.reconcile_provider(
                &projection.workspace,
                &projection.sandbox,
                provider_name,
                true,
            )
            .await?;
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
        let (workspace, sandbox) = deletion_names(request);
        let client = RuntimeDeleteClient {
            runtime: self,
            workspace: &workspace,
        };
        let deleted = delete_owned_sandbox(&client, &sandbox, &request.runtime.0).await?;
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
impl SandboxTaskRuntime for OpenShellRuntime {
    fn provider_control_bindings(&self) -> Option<ProviderControlExecutionBindings> {
        provider_control_execution_bindings(self)
    }

    async fn run_task(
        &self,
        request: &SandboxTaskRequest,
        input_archive: &[u8],
    ) -> Result<SandboxTaskOutput, PortError> {
        let workspace = request
            .refs
            .workspace
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| PortError::Rejected {
                reason: "task runtime has no sandbox workspace reference".to_owned(),
            })?;
        let sandbox = request
            .refs
            .sandbox
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| PortError::Rejected {
                reason: "task runtime has no sandbox name reference".to_owned(),
            })?;
        if request.command.is_empty() || request.command.iter().any(String::is_empty) {
            return Err(PortError::Rejected {
                reason: "task command must contain only non-empty arguments".to_owned(),
            });
        }
        let is_connections_bridge = request.agent_type.name == CONNECTIONS_BRIDGE_AGENT_TYPE;
        if is_connections_bridge {
            validate_connections_bridge_archive(input_archive, "request.json")?;
        }
        let snapshot = self
            .authenticated_client()
            .await?
            .workspace(workspace)
            .get_sandbox(sandbox)
            .await
            .map_err(port_failure)?;
        if snapshot.labels.get(RUNTIME_UID_LABEL).map(String::as_str)
            != Some(request.runtime.0.as_str())
        {
            return Err(PortError::Rejected {
                reason: "task sandbox is bound to a different runtime UID".to_owned(),
            });
        }
        let expected_image = bridge_image_for_execution(
            &request.agent_type,
            request.execution_class,
            request.execution_binding.as_ref(),
            self.stable_bridge_image.as_deref(),
            self.bridge_image.as_deref(),
        )?;
        let expected_execution_binding_label = request
            .execution_binding
            .as_ref()
            .map(|binding| execution_binding_label_value(&binding.binding_digest));
        if expected_execution_binding_label.as_ref() != snapshot.labels.get(EXECUTION_BINDING_LABEL)
        {
            return Err(PortError::Rejected {
                reason: "task sandbox does not match the persisted execution binding".to_owned(),
            });
        }
        self.resolve_raw_sandbox_binding(
            workspace,
            sandbox,
            &request.runtime.0,
            expected_image.as_deref(),
            true,
        )
        .await?;
        self.stage_input_archive(workspace, sandbox, input_archive)
            .await?;
        let sandbox_id = self
            .resolve_raw_sandbox_binding(
                workspace,
                sandbox,
                &request.runtime.0,
                expected_image.as_deref(),
                true,
            )
            .await?;
        let mut environment = HashMap::new();
        environment.insert(
            "STEWARD_OUTPUT_DIR".to_owned(),
            "/sandbox/steward-output".to_owned(),
        );
        if is_connections_bridge {
            let origin = match request.execution_class {
                SandboxExecutionClass::Agent => self.stable_bridge_gateway_origin.as_deref(),
                SandboxExecutionClass::ProviderControl => self.bridge_gateway_origin.as_deref(),
            }
            .ok_or_else(|| PortError::Rejected {
                reason: "Selected bridge gateway origin is not configured".to_owned(),
            })?;
            environment.insert("STEWARD_MCP_GW_ORIGIN".to_owned(), origin.to_owned());
        }
        let executed = self
            .exec_task_process(
                &sandbox_id,
                &request.command,
                environment,
                request.execution_class,
                TaskProcessLogContext {
                    runtime_uid: &request.runtime.0,
                    workspace,
                    sandbox,
                },
            )
            .await?;
        if executed.exit_code != 0 {
            let category = task_agent_failure_category(&executed.stderr);
            return Err(PortError::Failed {
                reason: format!(
                    "task agent exited with code {} (diagnostic-category={category})",
                    executed.exit_code
                ),
            });
        }
        let output_archive_command =
            output_archive_command(&request.agent_type, request.execution_binding.as_ref());
        let collected = self
            .authenticated_client()
            .await?
            .workspace(workspace)
            .exec(
                sandbox,
                &[
                    "/bin/sh".to_owned(),
                    "-c".to_owned(),
                    output_archive_command.to_owned(),
                ],
                ExecOptions {
                    timeout: Some(StdDuration::from_secs(120)),
                    ..ExecOptions::default()
                },
            )
            .await
            .map_err(port_failure)?;
        if collected.exit_code != 0 {
            return Err(PortError::Failed {
                reason: "task output archive could not be collected".to_owned(),
            });
        }
        if is_connections_bridge {
            validate_connections_bridge_archive(&collected.stdout, "response.json")?;
        }
        Ok(SandboxTaskOutput {
            archive: collected.stdout,
        })
    }
}

#[cfg(feature = "runtime")]
fn staging_archive_chunks(input_archive: &[u8]) -> std::slice::Chunks<'_, u8> {
    input_archive.chunks(STAGING_EXEC_STDIN_CHUNK_BYTES)
}

#[cfg(feature = "runtime")]
fn staging_prepare_command() -> &'static str {
    "set -eu; rm -rf /sandbox/steward-input /sandbox/steward-output; rm -f /sandbox/steward-input.tar; mkdir -p /sandbox/steward-input /sandbox/steward-output; : > /sandbox/steward-input.tar"
}

#[cfg(feature = "runtime")]
fn staging_append_command() -> &'static str {
    "set -eu; cat >> /sandbox/steward-input.tar"
}

#[cfg(feature = "runtime")]
fn staging_extract_command() -> &'static str {
    "set -eu; tar -xf /sandbox/steward-input.tar -C /sandbox/steward-input; rm -f /sandbox/steward-input.tar"
}

#[cfg(feature = "runtime")]
fn input_staging_rejected() -> PortError {
    PortError::Rejected {
        reason: "task input archive could not be staged".to_owned(),
    }
}

#[cfg(feature = "runtime")]
fn port_failure(error: SdkError) -> PortError {
    PortError::Failed {
        reason: error.to_string(),
    }
}

#[cfg(feature = "runtime")]
fn raw_port_failure(error: impl std::fmt::Display) -> PortError {
    PortError::Failed {
        reason: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "identity")]
    use std::collections::BTreeMap;
    #[cfg(feature = "runtime")]
    use std::collections::{HashMap, VecDeque};
    #[cfg(feature = "runtime")]
    use std::future::Future;
    #[cfg(feature = "runtime")]
    use std::io::{Read, Write};
    #[cfg(feature = "runtime")]
    use std::net::TcpListener;
    #[cfg(feature = "runtime")]
    use std::path::PathBuf;
    #[cfg(feature = "runtime")]
    use std::sync::Arc;
    #[cfg(feature = "runtime")]
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    #[cfg(feature = "runtime")]
    use std::sync::mpsc::{self, Receiver};
    #[cfg(feature = "runtime")]
    use std::task::{Context, Poll, Waker};
    #[cfg(feature = "runtime")]
    use std::thread::JoinHandle;
    #[cfg(feature = "runtime")]
    use std::time::Duration;

    #[cfg(feature = "runtime")]
    use openshell_sdk::raw::proto::datamodel::v1::ObjectMeta;
    #[cfg(feature = "runtime")]
    use openshell_sdk::raw::proto::{
        ExecSandboxEvent, ExecSandboxExit, ExecSandboxStderr, ExecSandboxStdout,
        Sandbox as RawSandbox, SandboxPhase as RawSandboxPhase, SandboxSpec as RawSandboxSpec,
        SandboxStatus as RawSandboxStatus, SandboxTemplate as RawSandboxTemplate,
        exec_sandbox_event,
    };
    #[cfg(feature = "runtime")]
    use reqwest::{Client as HttpClient, Url};
    #[cfg(feature = "runtime")]
    use sha2::{Digest, Sha256};
    #[cfg(feature = "runtime")]
    use tokio::sync::{Mutex, mpsc as tokio_mpsc};

    #[cfg(feature = "runtime")]
    use steward_ports::{PortError, SandboxExecutionClass, SandboxRequest};
    #[cfg(feature = "runtime")]
    use steward_types::{
        AgentType, DisposableExecutionBinding, ExecutionProviderProfile, ExecutionProviderProfiles,
        ExecutionVersionProbe, ModelRef, RuntimeId, RuntimeRefs,
        TASK_EXECUTION_BINDING_SCHEMA_VERSION, ToolGrant,
    };

    #[cfg(feature = "runtime")]
    use super::validate_connections_bridge_archive;
    #[cfg(feature = "runtime")]
    use super::{
        CONNECTIONS_BRIDGE_AGENT_TYPE, OpenShellConnectionConfig, OpenShellTaskLogMode,
        ProviderReconciliation, STAGING_EXEC_STDIN_CHUNK_BYTES, SandboxDeleteClient,
        TaskProcessEventStream, TaskProcessLogContext, TaskProcessLogSink, TaskProcessStream,
        WorkloadExchangeTokenProvider, collect_task_process_stream, delete_owned_sandbox,
        deletion_names, load_source_credential, output_archive_command, project_request,
        provider_reconciliation, sandbox_spec, staging_append_command, staging_archive_chunks,
        staging_extract_command, staging_prepare_command, task_agent_failure_category,
        task_process_log_record, validate_raw_sandbox_binding, validate_workload_exchange_endpoint,
    };
    #[cfg(feature = "runtime")]
    use super::{EXECUTION_BINDING_LABEL, RUNTIME_UID_LABEL};
    #[cfg(feature = "identity")]
    use super::{IdentityResolutionError, SANDBOX_ID_LABEL, binding_from_sandbox};
    use super::{NameKind, stable_name};

    #[cfg(feature = "runtime")]
    fn seal_binding(binding: &mut DisposableExecutionBinding) -> Result<(), String> {
        let mut digest = Sha256::new();
        digest.update(steward_types::TASK_EXECUTION_BINDING_DIGEST_DOMAIN);
        digest.update(binding.canonical_content()?);
        let digest = format!("sha256:{:x}", digest.finalize());
        binding.binding_id.clone_from(&digest);
        binding.binding_digest = digest;
        Ok(())
    }

    #[cfg(feature = "runtime")]
    struct QueuedTaskProcessEventStream {
        events: VecDeque<Result<ExecSandboxEvent, PortError>>,
    }

    #[cfg(feature = "runtime")]
    impl TaskProcessEventStream for QueuedTaskProcessEventStream {
        async fn next_event(&mut self) -> Result<Option<ExecSandboxEvent>, PortError> {
            self.events.pop_front().transpose()
        }
    }

    #[cfg(feature = "runtime")]
    struct ChannelTaskProcessEventStream {
        events: tokio_mpsc::UnboundedReceiver<Result<ExecSandboxEvent, PortError>>,
    }

    #[cfg(feature = "runtime")]
    impl TaskProcessEventStream for ChannelTaskProcessEventStream {
        async fn next_event(&mut self) -> Result<Option<ExecSandboxEvent>, PortError> {
            self.events.recv().await.transpose()
        }
    }

    #[cfg(feature = "runtime")]
    #[derive(Default)]
    struct RecordingTaskProcessLogSink {
        records: Vec<String>,
    }

    #[cfg(feature = "runtime")]
    impl TaskProcessLogSink for RecordingTaskProcessLogSink {
        fn emit(
            &mut self,
            context: TaskProcessLogContext<'_>,
            stream: TaskProcessStream,
            message: &[u8],
        ) {
            self.records
                .push(task_process_log_record(context, stream, message));
        }
    }

    #[cfg(feature = "runtime")]
    struct ChannelTaskProcessLogSink {
        records: tokio_mpsc::UnboundedSender<String>,
    }

    #[cfg(feature = "runtime")]
    impl TaskProcessLogSink for ChannelTaskProcessLogSink {
        fn emit(
            &mut self,
            context: TaskProcessLogContext<'_>,
            stream: TaskProcessStream,
            message: &[u8],
        ) {
            let _ = self
                .records
                .send(task_process_log_record(context, stream, message));
        }
    }

    #[cfg(feature = "runtime")]
    fn stdout_event(message: &[u8]) -> ExecSandboxEvent {
        ExecSandboxEvent {
            payload: Some(exec_sandbox_event::Payload::Stdout(ExecSandboxStdout {
                data: message.to_vec(),
            })),
        }
    }

    #[cfg(feature = "runtime")]
    fn stderr_event(message: &[u8]) -> ExecSandboxEvent {
        ExecSandboxEvent {
            payload: Some(exec_sandbox_event::Payload::Stderr(ExecSandboxStderr {
                data: message.to_vec(),
            })),
        }
    }

    #[cfg(feature = "runtime")]
    fn exit_event(exit_code: i32) -> ExecSandboxEvent {
        ExecSandboxEvent {
            payload: Some(exec_sandbox_event::Payload::Exit(ExecSandboxExit {
                exit_code,
            })),
        }
    }

    #[cfg(feature = "runtime")]
    struct MockExchange {
        endpoint: Url,
        handle: JoinHandle<Result<(), String>>,
        requests: Receiver<String>,
    }

    #[cfg(feature = "runtime")]
    impl MockExchange {
        fn finish(self, expected_requests: usize) -> Result<Vec<String>, String> {
            let mut requests = Vec::with_capacity(expected_requests);
            for _ in 0..expected_requests {
                requests.push(
                    self.requests
                        .recv_timeout(Duration::from_secs(2))
                        .map_err(|error| format!("mock exchange captured no request: {error}"))?,
                );
            }
            self.handle
                .join()
                .map_err(|_| "mock exchange server panicked".to_owned())??;
            Ok(requests)
        }
    }

    #[cfg(feature = "runtime")]
    fn mock_exchange(response: String) -> Result<MockExchange, String> {
        mock_exchange_responses(vec![response])
    }

    #[cfg(feature = "runtime")]
    fn mock_exchange_responses(responses: Vec<String>) -> Result<MockExchange, String> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|error| format!("failed to bind mock exchange: {error}"))?;
        let address = listener
            .local_addr()
            .map_err(|error| format!("failed to inspect mock exchange address: {error}"))?;
        let endpoint = Url::parse(&format!("http://{address}/v1/workload/exchange"))
            .map_err(|error| format!("failed to build mock exchange URL: {error}"))?;
        let (sender, requests) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener
                    .accept()
                    .map_err(|error| format!("mock exchange accept failed: {error}"))?;
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .map_err(|error| format!("mock exchange timeout setup failed: {error}"))?;
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4096];
                loop {
                    let read = stream
                        .read(&mut buffer)
                        .map_err(|error| format!("mock exchange request read failed: {error}"))?;
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8(request)
                    .map_err(|_| "mock exchange request was not UTF-8".to_owned())?;
                sender
                    .send(request)
                    .map_err(|error| format!("mock exchange capture failed: {error}"))?;
                stream
                    .write_all(response.as_bytes())
                    .map_err(|error| format!("mock exchange response write failed: {error}"))?;
            }
            Ok(())
        });
        Ok(MockExchange {
            endpoint,
            handle,
            requests,
        })
    }

    #[cfg(feature = "runtime")]
    fn response(status: u16, cache_control: Option<&str>, body: &str) -> String {
        let cache_control = cache_control
            .map(|value| format!("Cache-Control: {value}\r\n"))
            .unwrap_or_default();
        format!(
            "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\n{cache_control}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[cfg(feature = "runtime")]
    fn source_credential_fixture() -> Result<PathBuf, String> {
        static TOKEN_FILE_ID: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "steward-workload-source-{}-{}",
            std::process::id(),
            TOKEN_FILE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, "obviously-fake-source-credential")
            .map_err(|error| format!("failed to create source credential fixture: {error}"))?;
        Ok(path)
    }

    #[cfg(feature = "runtime")]
    fn test_token_provider(
        endpoint: Url,
        source_credential_file: PathBuf,
    ) -> Result<WorkloadExchangeTokenProvider, String> {
        Ok(WorkloadExchangeTokenProvider {
            cache: Arc::new(Mutex::new(None)),
            client: HttpClient::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(2))
                .build()
                .map_err(|error| format!("failed to build test exchange client: {error}"))?,
            endpoint,
            source_credential_file,
        })
    }

    #[cfg(feature = "runtime")]
    fn valid_connection_config() -> OpenShellConnectionConfig {
        OpenShellConnectionConfig {
            endpoint: "https://gateway.example.test:8080".to_owned(),
            ca_certificate_pem: b"test-ca-certificate".to_vec(),
            client_certificate_pem: b"test-client-certificate".to_vec(),
            client_private_key_pem: b"test-client-private-key".to_vec(),
            workload_exchange_endpoint: "https://identity.example.test/v1/workload/exchange"
                .to_owned(),
            workload_exchange_server_name: "identity.example.test".to_owned(),
            workload_exchange_ca_certificate_pem: b"test-exchange-ca-certificate".to_vec(),
            workload_source_credential_file: PathBuf::from("/run/workload/source-credential"),
            server_name: "gateway.example.test".to_owned(),
            runtime_class_name: "kata-qemu".to_owned(),
            task_log_mode: super::OpenShellTaskLogMode::Off,
            stable_bridge_image: None,
            stable_bridge_gateway_origin: None,
            bridge_image: None,
            bridge_artifact_trust_mode: None,
            bridge_gateway_origin: None,
            bridge_gateway_version: None,
            bridge_runtime_namespace: None,
        }
    }

    #[cfg(feature = "runtime")]
    #[tokio::test]
    async fn full_task_logging_forwards_stdout_and_stderr_before_exit() -> Result<(), String> {
        let (event_sender, event_receiver) = tokio_mpsc::unbounded_channel();
        let (record_sender, mut record_receiver) = tokio_mpsc::unbounded_channel();
        let collection = tokio::spawn(async move {
            let mut stream = ChannelTaskProcessEventStream {
                events: event_receiver,
            };
            let mut log_sink = ChannelTaskProcessLogSink {
                records: record_sender,
            };
            collect_task_process_stream(
                &mut stream,
                OpenShellTaskLogMode::Full,
                TaskProcessLogContext {
                    runtime_uid: "runtime-123",
                    workspace: "workspace-a",
                    sandbox: "sandbox-a",
                },
                &mut log_sink,
            )
            .await
        });

        event_sender
            .send(Ok(stdout_event(b"reasoning\n")))
            .map_err(|_| "live task stream closed before stdout".to_owned())?;
        let stdout_record =
            tokio::time::timeout(Duration::from_millis(250), record_receiver.recv())
                .await
                .map_err(|_| "stdout was buffered instead of logged live".to_owned())?
                .ok_or_else(|| "task logger closed before stdout".to_owned())?;
        assert_eq!(
            stdout_record,
            "openshell_task_process runtime_uid=\"runtime-123\" workspace=\"workspace-a\" sandbox=\"sandbox-a\" stream=stdout message=\"reasoning\\n\""
        );

        event_sender
            .send(Ok(stderr_event(&[b'e', 0xff, b'\n'])))
            .map_err(|_| "live task stream closed before stderr".to_owned())?;
        let stderr_record =
            tokio::time::timeout(Duration::from_millis(250), record_receiver.recv())
                .await
                .map_err(|_| "stderr was buffered instead of logged live".to_owned())?
                .ok_or_else(|| "task logger closed before stderr".to_owned())?;
        assert_eq!(
            stderr_record,
            "openshell_task_process runtime_uid=\"runtime-123\" workspace=\"workspace-a\" sandbox=\"sandbox-a\" stream=stderr message=\"e\\xff\\n\""
        );

        event_sender
            .send(Ok(exit_event(0)))
            .map_err(|_| "live task stream closed before exit".to_owned())?;
        drop(event_sender);
        let result = tokio::time::timeout(Duration::from_secs(1), collection)
            .await
            .map_err(|_| "task stream did not finish after exit".to_owned())?
            .map_err(|error| format!("task stream collector failed to join: {error}"))?
            .map_err(|error| format!("task stream collector failed: {error:?}"))?;
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout, b"reasoning\n");
        assert_eq!(result.stderr, [b'e', 0xff, b'\n']);
        Ok(())
    }

    #[cfg(feature = "runtime")]
    #[tokio::test]
    async fn full_task_logging_records_escaped_task_output_material() -> Result<(), String> {
        let secret_bearing_output =
            b"Authorization: Bearer obviously-fake-task-credential\nrequest completed";
        let mut stream = QueuedTaskProcessEventStream {
            events: VecDeque::from([Ok(stdout_event(secret_bearing_output)), Ok(exit_event(0))]),
        };
        let mut log_sink = RecordingTaskProcessLogSink::default();

        let result = collect_task_process_stream(
            &mut stream,
            OpenShellTaskLogMode::Full,
            TaskProcessLogContext {
                runtime_uid: "runtime-123",
                workspace: "workspace-a",
                sandbox: "sandbox-a",
            },
            &mut log_sink,
        )
        .await
        .map_err(|error| format!("task stream collector failed: {error:?}"))?;

        assert_eq!(result.stdout, secret_bearing_output);
        assert_eq!(log_sink.records.len(), 1);
        let record = &log_sink.records[0];
        assert_eq!(
            record,
            "openshell_task_process runtime_uid=\"runtime-123\" workspace=\"workspace-a\" sandbox=\"sandbox-a\" stream=stdout message=\"Authorization: Bearer obviously-fake-task-credential\\nrequest completed\"",
            "full logging must expose the escaped task-process event"
        );
        Ok(())
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn connections_bridge_structurally_disables_full_task_output_logging() {
        assert_eq!(
            super::task_log_mode_for_execution(
                OpenShellTaskLogMode::Full,
                SandboxExecutionClass::ProviderControl,
            ),
            OpenShellTaskLogMode::Off,
            "OAuth continuation material must remain unloggable even when full task logging is enabled"
        );
        assert_eq!(
            super::task_log_mode_for_execution(
                OpenShellTaskLogMode::Full,
                SandboxExecutionClass::Agent,
            ),
            OpenShellTaskLogMode::Full,
            "the provider-control logging override must not change long-running agent logging"
        );
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn task_execution_resolves_the_verified_sandbox_id_after_staging() -> Result<(), String> {
        let source = include_str!("lib.rs");
        let implementation = source
            .split("impl SandboxTaskRuntime for OpenShellRuntime")
            .nth(1)
            .ok_or_else(|| "SandboxTaskRuntime implementation was not found".to_owned())?;
        let run_task = implementation
            .split("async fn run_task")
            .nth(1)
            .and_then(|value| value.split("async fn").next())
            .ok_or_else(|| "run_task implementation was not found".to_owned())?;
        let staging = run_task
            .find(".stage_input_archive(")
            .ok_or_else(|| "task input staging call was not found".to_owned())?;
        let resolution = run_task
            .rfind(".resolve_raw_sandbox_binding(")
            .ok_or_else(|| {
                "task sandbox ID is not re-resolved and verified after staging".to_owned()
            })?;
        let execution = run_task
            .find(".exec_task_process(")
            .ok_or_else(|| "task process execution call was not found".to_owned())?;

        assert!(
            staging < resolution && resolution < execution,
            "the exact sandbox ID must be resolved after name-based staging and before raw execution"
        );
        assert!(
            run_task[staging..resolution].contains("let sandbox_id =")
                && run_task[execution..].contains("&sandbox_id,"),
            "raw execution must use the post-staging verified sandbox ID"
        );
        assert!(
            !run_task[execution..].contains("&snapshot.id,"),
            "raw execution must not reuse the pre-staging sandbox ID"
        );
        Ok(())
    }

    #[cfg(feature = "runtime")]
    #[tokio::test]
    async fn off_task_logging_preserves_output_without_emitting_records() -> Result<(), String> {
        let mut stream = QueuedTaskProcessEventStream {
            events: VecDeque::from([
                Ok(stdout_event(b"collected stdout")),
                Ok(stderr_event(b"collected stderr")),
                Ok(exit_event(0)),
            ]),
        };
        let mut log_sink = RecordingTaskProcessLogSink::default();
        let result = collect_task_process_stream(
            &mut stream,
            OpenShellTaskLogMode::Off,
            TaskProcessLogContext {
                runtime_uid: "runtime-123",
                workspace: "workspace-a",
                sandbox: "sandbox-a",
            },
            &mut log_sink,
        )
        .await
        .map_err(|error| format!("task stream collector failed: {error:?}"))?;

        assert!(
            log_sink.records.is_empty(),
            "off mode must not mirror task-process output"
        );
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout, b"collected stdout");
        assert_eq!(result.stderr, b"collected stderr");
        Ok(())
    }

    #[cfg(feature = "runtime")]
    #[tokio::test]
    async fn streamed_nonzero_exit_retains_stderr_failure_classification() -> Result<(), String> {
        let mut stream = QueuedTaskProcessEventStream {
            events: VecDeque::from([
                Ok(stderr_event(b"provider returned unauthorized status 401")),
                Ok(exit_event(17)),
            ]),
        };
        let mut log_sink = RecordingTaskProcessLogSink::default();
        let result = collect_task_process_stream(
            &mut stream,
            OpenShellTaskLogMode::Full,
            TaskProcessLogContext {
                runtime_uid: "runtime-123",
                workspace: "workspace-a",
                sandbox: "sandbox-a",
            },
            &mut log_sink,
        )
        .await
        .map_err(|error| format!("task stream collector failed: {error:?}"))?;

        assert_eq!(result.exit_code, 17);
        assert_eq!(
            task_agent_failure_category(&result.stderr),
            "authentication"
        );
        assert_eq!(log_sink.records.len(), 1);
        Ok(())
    }

    #[cfg(feature = "runtime")]
    #[tokio::test]
    async fn task_stream_timeout_drops_the_pending_stream() -> Result<(), String> {
        let (event_sender, event_receiver) = tokio_mpsc::unbounded_channel();
        let timed = tokio::time::timeout(Duration::from_millis(25), async move {
            let mut stream = ChannelTaskProcessEventStream {
                events: event_receiver,
            };
            let mut log_sink = RecordingTaskProcessLogSink::default();
            collect_task_process_stream(
                &mut stream,
                OpenShellTaskLogMode::Full,
                TaskProcessLogContext {
                    runtime_uid: "runtime-123",
                    workspace: "workspace-a",
                    sandbox: "sandbox-a",
                },
                &mut log_sink,
            )
            .await
        })
        .await;

        assert!(
            timed.is_err(),
            "a silent task stream must reach its deadline"
        );
        assert!(
            event_sender.is_closed(),
            "timing out task collection must drop the pending stream"
        );
        Ok(())
    }

    #[cfg(feature = "runtime")]
    #[tokio::test]
    async fn task_stream_cancellation_drops_the_pending_stream() -> Result<(), String> {
        let (event_sender, event_receiver) = tokio_mpsc::unbounded_channel();
        let collection = tokio::spawn(async move {
            let mut stream = ChannelTaskProcessEventStream {
                events: event_receiver,
            };
            let mut log_sink = RecordingTaskProcessLogSink::default();
            collect_task_process_stream(
                &mut stream,
                OpenShellTaskLogMode::Full,
                TaskProcessLogContext {
                    runtime_uid: "runtime-123",
                    workspace: "workspace-a",
                    sandbox: "sandbox-a",
                },
                &mut log_sink,
            )
            .await
        });
        tokio::task::yield_now().await;
        collection.abort();
        let cancelled = tokio::time::timeout(Duration::from_millis(250), collection)
            .await
            .map_err(|_| "cancelled task stream did not stop".to_owned())?;
        assert!(
            matches!(cancelled, Err(ref error) if error.is_cancelled()),
            "the task stream collector must honor cancellation"
        );
        assert!(
            event_sender.is_closed(),
            "cancelling task collection must drop the pending stream"
        );
        Ok(())
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn task_archives_are_split_into_bounded_unary_exec_payloads() {
        let archive = vec![b'x'; 2 * 1024 * 1024 + 17];
        let chunks = staging_archive_chunks(&archive).collect::<Vec<_>>();
        assert!(
            chunks
                .iter()
                .all(|chunk| !chunk.is_empty() && chunk.len() <= STAGING_EXEC_STDIN_CHUNK_BYTES),
            "every unary stdin payload must remain below OpenShell's decoded-message ceiling"
        );
        assert_eq!(
            chunks.concat(),
            archive,
            "unary staging chunks must preserve every task archive byte in order"
        );
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn staging_uses_a_file_backed_archive_between_bounded_execs() {
        assert!(staging_prepare_command().contains(": > /sandbox/steward-input.tar"));
        assert_eq!(
            staging_append_command(),
            "set -eu; cat >> /sandbox/steward-input.tar"
        );
        assert!(
            staging_extract_command()
                .contains("tar -xf /sandbox/steward-input.tar -C /sandbox/steward-input")
        );
        assert!(staging_extract_command().contains("rm -f /sandbox/steward-input.tar"));
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn gateway_transport_rejects_plaintext() {
        let mut config = valid_connection_config();
        config.endpoint = "http://gateway.example.test:8080".to_owned();

        assert!(
            matches!(config.validate(), Err(PortError::Rejected { .. })),
            "the OpenShell adapter must reject plaintext gateway transport"
        );
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn workload_exchange_requires_exact_verified_https_endpoint() {
        for (endpoint, server_name) in [
            (
                "http://identity.example.test/v1/workload/exchange",
                "identity.example.test",
            ),
            (
                "https://identity.example.test/v1/exchange",
                "identity.example.test",
            ),
            (
                "https://identity.example.test/v1/workload/exchange?roles=admin",
                "identity.example.test",
            ),
            (
                "https://identity.example.test/v1/workload/exchange",
                "other.example.test",
            ),
        ] {
            assert!(
                matches!(
                    validate_workload_exchange_endpoint(endpoint, server_name),
                    Err(PortError::Rejected { .. })
                ),
                "the workload exchange must reject an inexact or unverified endpoint"
            );
        }
        assert!(
            validate_workload_exchange_endpoint(
                "https://identity.example.test/v1/workload/exchange",
                "identity.example.test",
            )
            .is_ok(),
            "the exact verified workload exchange endpoint must be accepted"
        );
    }

    #[cfg(feature = "runtime")]
    #[tokio::test]
    async fn workload_exchange_uses_empty_post_and_caches_only_the_access_token()
    -> Result<(), String> {
        let body = r#"{"access_token":"obviously-fake-access-token","token_type":"Bearer","expires_in":120}"#;
        let exchange = mock_exchange(response(200, Some("private, no-store"), body))?;
        let source_file = source_credential_fixture()?;
        let provider = test_token_provider(exchange.endpoint.clone(), source_file.clone())?;

        let (first, second, third) = tokio::join!(
            provider.access_token(),
            provider.access_token(),
            provider.access_token()
        );
        for token in [first, second, third] {
            assert_eq!(
                token
                    .map_err(|error| format!("exchange failed: {error:?}"))?
                    .expose_secret(),
                "obviously-fake-access-token"
            );
        }
        let [request] = exchange
            .finish(1)?
            .try_into()
            .map_err(|_| "mock exchange request count changed".to_owned())?;
        std::fs::remove_file(source_file)
            .map_err(|error| format!("failed to remove source credential fixture: {error}"))?;
        let (headers, body) = request
            .split_once("\r\n\r\n")
            .ok_or_else(|| "mock exchange request had no header terminator".to_owned())?;
        assert!(
            headers.starts_with("POST /v1/workload/exchange HTTP/1.1\r\n"),
            "the client must call only the exact workload exchange path"
        );
        assert!(
            headers.lines().any(|line| {
                line.eq_ignore_ascii_case("authorization: Bearer obviously-fake-source-credential")
            }),
            "the current source credential must be carried only as the bearer authorization"
        );
        assert!(
            body.is_empty(),
            "the workload exchange POST body must be empty"
        );
        assert!(
            !headers.contains("roles")
                && !headers.contains("algorithm")
                && !headers.contains("audience"),
            "the caller must not select output claims or signing behavior"
        );
        Ok(())
    }

    #[cfg(feature = "runtime")]
    #[tokio::test]
    async fn workload_exchange_response_contract_fails_closed_without_exposing_tokens()
    -> Result<(), String> {
        let cases = [
            (
                response(
                    200,
                    None,
                    r#"{"access_token":"obviously-fake-access-token","token_type":"Bearer","expires_in":120}"#,
                ),
                "missing no-store",
            ),
            (
                response(
                    200,
                    Some("no-store"),
                    r#"{"access_token":"obviously-fake-access-token","token_type":"bearer","expires_in":120}"#,
                ),
                "wrong token type",
            ),
            (
                response(
                    200,
                    Some("no-store"),
                    r#"{"access_token":"obviously-fake-access-token","token_type":"Bearer","expires_in":121}"#,
                ),
                "excessive expiry",
            ),
            (
                response(
                    200,
                    Some("no-store"),
                    r#"{"access_token":"obviously-fake-access-token","token_type":"Bearer","expires_in":120,"roles":["admin"]}"#,
                ),
                "caller-selected fields",
            ),
            (
                response(401, Some("no-store"), r#"{"error":"invalid_token"}"#),
                "rejected source",
            ),
        ];
        for (wire_response, description) in cases {
            let exchange = mock_exchange(wire_response)?;
            let source_file = source_credential_fixture()?;
            let provider = test_token_provider(exchange.endpoint.clone(), source_file.clone())?;
            let result = provider.access_token().await;
            assert!(
                matches!(&result, Err(PortError::Rejected { .. })),
                "the exchange must fail closed for {description}"
            );
            let rendered = result
                .err()
                .map(|error| format!("{error:?}"))
                .unwrap_or_default();
            assert!(
                !rendered.contains("obviously-fake-source-credential")
                    && !rendered.contains("obviously-fake-access-token"),
                "exchange errors must not expose either credential"
            );
            let _ = exchange.finish(1)?;
            std::fs::remove_file(source_file)
                .map_err(|error| format!("failed to remove source credential fixture: {error}"))?;
        }
        Ok(())
    }

    #[cfg(feature = "runtime")]
    #[tokio::test]
    async fn workload_exchange_does_not_follow_redirects() -> Result<(), String> {
        let exchange = mock_exchange(response(307, None, ""))?;
        let source_file = source_credential_fixture()?;
        let provider = test_token_provider(exchange.endpoint.clone(), source_file.clone())?;

        assert!(
            matches!(provider.access_token().await, Err(PortError::Failed { .. })),
            "the source credential must never follow a workload exchange redirect"
        );
        let _ = exchange.finish(1)?;
        std::fs::remove_file(source_file)
            .map_err(|error| format!("failed to remove source credential fixture: {error}"))?;
        Ok(())
    }

    #[cfg(feature = "runtime")]
    #[tokio::test]
    async fn expired_access_token_refreshes_from_the_current_source_credential()
    -> Result<(), String> {
        let exchange = mock_exchange_responses(vec![
            response(
                200,
                Some("no-store"),
                r#"{"access_token":"obviously-fake-first-access","token_type":"Bearer","expires_in":1}"#,
            ),
            response(
                200,
                Some("no-store"),
                r#"{"access_token":"obviously-fake-second-access","token_type":"Bearer","expires_in":120}"#,
            ),
        ])?;
        let source_file = source_credential_fixture()?;
        let provider = test_token_provider(exchange.endpoint.clone(), source_file.clone())?;
        let first = provider
            .access_token()
            .await
            .map_err(|error| format!("first exchange failed: {error:?}"))?;
        assert_eq!(first.expose_secret(), "obviously-fake-first-access");
        std::fs::write(&source_file, "obviously-fake-rotated-source")
            .map_err(|error| format!("failed to rotate source credential fixture: {error}"))?;
        let second = provider
            .access_token()
            .await
            .map_err(|error| format!("refresh exchange failed: {error:?}"))?;
        assert_eq!(second.expose_secret(), "obviously-fake-second-access");
        let requests = exchange.finish(2)?;
        assert!(
            requests[0].lines().any(|line| {
                line.eq_ignore_ascii_case("authorization: Bearer obviously-fake-source-credential")
            }),
            "the first exchange must use the initial source credential"
        );
        assert!(
            requests[1].lines().any(|line| {
                line.eq_ignore_ascii_case("authorization: Bearer obviously-fake-rotated-source")
            }),
            "refresh must load the current source credential"
        );
        std::fs::remove_file(source_file)
            .map_err(|error| format!("failed to remove source credential fixture: {error}"))?;
        Ok(())
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn gateway_transport_requires_explicit_trust_and_caller_identity() {
        for mutate in [
            |config: &mut OpenShellConnectionConfig| config.ca_certificate_pem.clear(),
            |config: &mut OpenShellConnectionConfig| config.client_certificate_pem.clear(),
            |config: &mut OpenShellConnectionConfig| config.client_private_key_pem.clear(),
            |config: &mut OpenShellConnectionConfig| config.workload_source_credential_file.clear(),
            |config: &mut OpenShellConnectionConfig| {
                config.workload_exchange_ca_certificate_pem.clear()
            },
            |config: &mut OpenShellConnectionConfig| config.server_name.clear(),
        ] {
            let mut config = valid_connection_config();
            mutate(&mut config);
            assert!(
                matches!(config.validate(), Err(PortError::Rejected { .. })),
                "the OpenShell adapter must reject missing CA, client identity, or server name material"
            );
        }
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn gateway_runtime_class_rejects_invalid_kubernetes_names() {
        let label_too_long = "a".repeat(64);
        let name_too_long = vec!["a".repeat(63); 4].join(".");
        for runtime_class_name in [
            "".to_owned(),
            "invalid/runtime".to_owned(),
            "Invalid".to_owned(),
            "invalid_name".to_owned(),
            "-leading".to_owned(),
            "trailing-".to_owned(),
            "two..labels".to_owned(),
            label_too_long,
            name_too_long,
        ] {
            let mut config = valid_connection_config();
            config.runtime_class_name = runtime_class_name.clone();

            assert!(
                matches!(config.validate(), Err(PortError::Rejected { .. })),
                "the OpenShell adapter must reject invalid Kubernetes RuntimeClass name {runtime_class_name:?}"
            );
        }
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn gateway_runtime_class_accepts_reviewed_and_legacy_kubernetes_names() {
        for runtime_class_name in ["openshell-runc", "kata-qemu"] {
            let mut config = valid_connection_config();
            config.runtime_class_name = runtime_class_name.to_owned();

            assert!(
                config.validate().is_ok(),
                "the OpenShell adapter must accept valid Kubernetes RuntimeClass name {runtime_class_name}"
            );
        }
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn source_credential_file_observes_rotation_and_fails_closed() -> Result<(), String> {
        static TOKEN_FILE_ID: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "steward-openshell-token-{}-{}",
            std::process::id(),
            TOKEN_FILE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let first = "obviously-fake-first-token";
        let second = "obviously-fake-rotated-token";
        std::fs::write(&path, first)
            .map_err(|error| format!("failed to create token fixture: {error}"))?;
        assert_eq!(
            load_source_credential(&path)
                .map_err(|error| format!("{error:?}"))?
                .expose_secret(),
            first,
        );
        std::fs::write(&path, second)
            .map_err(|error| format!("failed to rotate token fixture: {error}"))?;
        assert_eq!(
            load_source_credential(&path)
                .map_err(|error| format!("{error:?}"))?
                .expose_secret(),
            second,
            "each token load must observe the current file contents"
        );
        std::fs::write(&path, b"")
            .map_err(|error| format!("failed to empty token fixture: {error}"))?;
        let empty = load_source_credential(&path);
        assert!(
            matches!(&empty, Err(PortError::Rejected { .. })),
            "an empty projected token must fail closed"
        );
        std::fs::remove_file(&path)
            .map_err(|error| format!("failed to remove token fixture: {error}"))?;
        let missing = load_source_credential(&path);
        assert!(
            matches!(&missing, Err(PortError::Rejected { .. })),
            "a missing projected token must fail closed"
        );
        std::fs::create_dir(&path)
            .map_err(|error| format!("failed to create unreadable token fixture: {error}"))?;
        let unreadable = load_source_credential(&path);
        assert!(
            matches!(&unreadable, Err(PortError::Rejected { .. })),
            "an unreadable projected token must fail closed"
        );
        std::fs::remove_dir(&path)
            .map_err(|error| format!("failed to remove unreadable token fixture: {error}"))?;
        let rendered = [empty, missing, unreadable]
            .into_iter()
            .filter_map(Result::err)
            .map(|error| format!("{error:?}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            !rendered.contains(first) && !rendered.contains(second),
            "token load failures must not expose token material"
        );
        Ok(())
    }

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
        let projection = project_request(
            &SandboxRequest {
                runtime: RuntimeId("runtime-uid-a".to_owned()),
                workspace_key: "team-a".to_owned(),
                execution_class: SandboxExecutionClass::Agent,
                agent_type: AgentType {
                    name: "base".to_owned(),
                },
                models: Vec::new(),
                tools: Vec::new(),
                refs: RuntimeRefs::default(),
                execution_binding: None,
            },
            None,
            None,
        )
        .map_err(|error| format!("runtime projection failed: {error:?}"))?;

        assert_eq!(projection.workspace, "w-9086ou4eujpgku8z0");
        assert_eq!(projection.workspace_key, "team-a");
        assert_eq!(projection.sandbox, "s-tmtp1a3s40p1kixv2");
        assert!(
            sandbox_spec(&projection).image.is_none(),
            "the gateway's configured default sandbox image must remain authoritative"
        );
        assert_eq!(projection.runtime_uid, "runtime-uid-a");
        Ok(())
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn unbound_versioned_agents_fail_closed() {
        let request = |agent_type: &str| SandboxRequest {
            runtime: RuntimeId("runtime-uid-a".to_owned()),
            workspace_key: "team-a".to_owned(),
            execution_class: SandboxExecutionClass::Agent,
            agent_type: AgentType {
                name: agent_type.to_owned(),
            },
            models: Vec::new(),
            tools: Vec::new(),
            refs: RuntimeRefs::default(),
            execution_binding: None,
        };

        assert!(
            project_request(&request("example-agent@1.0.0"), None, None).is_err(),
            "a versioned agent without a persisted deployment binding must fail closed"
        );
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn persisted_disposable_binding_selects_exact_image_and_provider_profiles() -> Result<(), String>
    {
        let mut binding = DisposableExecutionBinding {
            schema_version: TASK_EXECUTION_BINDING_SCHEMA_VERSION.to_owned(),
            binding_id: format!("sha256:{}", "a".repeat(64)),
            binding_digest: format!("sha256:{}", "a".repeat(64)),
            agent_ref: "example-agent@1.0.0".to_owned(),
            display_name: Some("Example Agent".to_owned()),
            adapter: "codex-v1".to_owned(),
            image: format!(
                "registry.example.test/agents/example@sha256:{}",
                "b".repeat(64)
            ),
            executable: "/opt/example/bin/agent".to_owned(),
            version_probe: ExecutionVersionProbe {
                arguments: vec!["--version".to_owned()],
                expected_stdout: "example-agent 1.0.0".to_owned(),
            },
            provider_profiles: ExecutionProviderProfiles {
                tools: Some(ExecutionProviderProfile {
                    id: "example-tools-profile-v7".to_owned(),
                    digest: format!("sha256:{}", "c".repeat(64)),
                }),
                inference: Some(ExecutionProviderProfile {
                    id: "example-inference-profile-v7".to_owned(),
                    digest: format!("sha256:{}", "d".repeat(64)),
                }),
            },
        };
        seal_binding(&mut binding)?;
        let projection = project_request(
            &SandboxRequest {
                runtime: RuntimeId("runtime-uid-a".to_owned()),
                workspace_key: "team-a".to_owned(),
                execution_class: SandboxExecutionClass::Agent,
                agent_type: AgentType {
                    name: binding.agent_ref.clone(),
                },
                models: vec![ModelRef {
                    provider: "litellm".to_owned(),
                    model: "test-model".to_owned(),
                }],
                tools: vec![ToolGrant {
                    provider: "github".to_owned(),
                    resource: "repository".to_owned(),
                    action: "get_file_contents".to_owned(),
                }],
                refs: RuntimeRefs::default(),
                execution_binding: Some(binding.clone()),
            },
            Some("registry.example.test/unrelated@sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"),
            None,
        )
        .map_err(|error| format!("persisted binding was rejected: {error:?}"))?;

        assert_eq!(projection.image.as_deref(), Some(binding.image.as_str()));
        assert_eq!(
            projection.providers,
            ["example-tools-profile-v7", "example-inference-profile-v7"],
            "the persisted binding must select the exact deployment-owned OpenShell profiles"
        );
        let execution_binding_label = sandbox_spec(&projection)
            .labels
            .get(EXECUTION_BINDING_LABEL)
            .cloned()
            .ok_or_else(|| "sandbox must carry an execution-binding label".to_owned())?;
        assert_ne!(
            execution_binding_label, binding.binding_digest,
            "the exact binding digest must be encoded for OpenShell label compatibility"
        );
        assert!(
            execution_binding_label.starts_with("sha256-"),
            "the label must identify its deterministic digest encoding"
        );
        assert_eq!(
            execution_binding_label.len(),
            63,
            "the digest label must fit the Kubernetes/OpenShell label-value limit"
        );
        assert!(
            execution_binding_label
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'),
            "the digest label must use only OpenShell-compatible characters"
        );

        let mut incomplete = binding.clone();
        incomplete.provider_profiles.tools = None;
        seal_binding(&mut incomplete)?;
        assert!(
            project_request(
                &SandboxRequest {
                    runtime: RuntimeId("runtime-uid-b".to_owned()),
                    workspace_key: "team-a".to_owned(),
                    execution_class: SandboxExecutionClass::Agent,
                    agent_type: AgentType {
                        name: incomplete.agent_ref.clone(),
                    },
                    models: vec![ModelRef {
                        provider: "litellm".to_owned(),
                        model: "test-model".to_owned(),
                    }],
                    tools: vec![ToolGrant {
                        provider: "github".to_owned(),
                        resource: "repository".to_owned(),
                        action: "get_file_contents".to_owned(),
                    }],
                    refs: RuntimeRefs::default(),
                    execution_binding: Some(incomplete),
                },
                None,
                None,
            )
            .is_err(),
            "a required profile missing from the persisted binding must fail before sandbox creation"
        );

        let mut tampered = binding.clone();
        tampered.executable = "/opt/example/bin/other-agent".to_owned();
        assert!(
            matches!(
                project_request(
                    &SandboxRequest {
                        runtime: RuntimeId("runtime-uid-c".to_owned()),
                        workspace_key: "team-a".to_owned(),
                        execution_class: SandboxExecutionClass::Agent,
                        agent_type: AgentType {
                            name: tampered.agent_ref.clone(),
                        },
                        models: Vec::new(),
                        tools: Vec::new(),
                        refs: RuntimeRefs::default(),
                        execution_binding: Some(tampered),
                    },
                    None,
                    None,
                ),
                Err(PortError::Rejected { ref reason }) if reason.contains("digest")
            ),
            "runtime projection must recompute the content-bound digest"
        );
        Ok(())
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn bound_workflow_validates_the_standard_result_and_archives_declared_outputs() {
        let binding = DisposableExecutionBinding {
            schema_version: TASK_EXECUTION_BINDING_SCHEMA_VERSION.to_owned(),
            binding_id: format!("sha256:{}", "a".repeat(64)),
            binding_digest: format!("sha256:{}", "a".repeat(64)),
            agent_ref: "example-agent@1.0.0".to_owned(),
            display_name: None,
            adapter: "codex-v1".to_owned(),
            image: format!(
                "registry.example.test/agents/example@sha256:{}",
                "b".repeat(64)
            ),
            executable: "/opt/example/bin/agent".to_owned(),
            version_probe: ExecutionVersionProbe {
                arguments: vec!["--version".to_owned()],
                expected_stdout: "example-agent 1.0.0".to_owned(),
            },
            provider_profiles: ExecutionProviderProfiles::default(),
        };
        let command = output_archive_command(
            &AgentType {
                name: binding.agent_ref.clone(),
            },
            Some(&binding),
        );
        assert!(
            command.contains("test -s /sandbox/steward-output/result.txt"),
            "the runtime must reject a missing or empty standard result"
        );
        assert!(
            command.contains("test -d /sandbox/steward-output/out"),
            "the runtime must reject a missing declared output root"
        );
        assert!(
            command.ends_with(" out"),
            "the runtime must return the declared output root, not its internal result artifact"
        );
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn connections_bridge_requires_a_digest_pinned_verified_image() -> Result<(), String> {
        let request = SandboxRequest {
            runtime: RuntimeId("runtime-uid-a".to_owned()),
            workspace_key: "team-a".to_owned(),
            execution_class: SandboxExecutionClass::ProviderControl,
            agent_type: AgentType {
                name: CONNECTIONS_BRIDGE_AGENT_TYPE.to_owned(),
            },
            models: Vec::new(),
            tools: Vec::new(),
            refs: RuntimeRefs::default(),
            execution_binding: None,
        };
        assert!(
            project_request(&request, None, None).is_err(),
            "the bridge must not create a sandbox without a verified image"
        );
        assert!(
            project_request(
                &request,
                None,
                Some("registry.example.test/steward-bridge:latest")
            )
            .is_err(),
            "the bridge must reject mutable image tags"
        );
        let projection = project_request(
            &request,
            Some("registry.example.test/stable-bridge@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            Some("registry.example.test/steward-bridge@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        )
        .map_err(|error| format!("verified bridge image rejected: {error:?}"))?;
        assert_eq!(
            sandbox_spec(&projection).image.as_deref(),
            Some(
                "registry.example.test/steward-bridge@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            ),
            "only the exact digest that passed configuration may reach OpenShell"
        );
        let mut stable_request = request;
        stable_request.execution_class = SandboxExecutionClass::Agent;
        let stable_projection = project_request(
            &stable_request,
            Some("registry.example.test/stable-bridge@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            Some("registry.example.test/steward-bridge@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        )
        .map_err(|error| format!("stable bridge image rejected: {error:?}"))?;
        assert_eq!(
            sandbox_spec(&stable_projection).image.as_deref(),
            Some(
                "registry.example.test/stable-bridge@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            ),
            "a long-running bridge runtime must retain its independent stable image"
        );
        Ok(())
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn connections_bridge_configuration_requires_an_exact_server_origin() {
        let mut config = valid_connection_config();
        config.bridge_image = Some(
            "registry.example.test/steward-bridge@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
        );
        config.bridge_artifact_trust_mode = Some("github-attestation".to_owned());
        assert!(
            matches!(config.validate(), Err(PortError::Rejected { .. })),
            "a bridge image without its controller-owned gateway origin must fail before a sandbox can be created"
        );
        config.bridge_gateway_origin = Some("https://mcp-gw.example.test".to_owned());
        config.bridge_runtime_namespace = Some("steward-test".to_owned());
        assert!(
            matches!(config.validate(), Err(PortError::Rejected { .. })),
            "a bridge binding without the authority-pinned gateway version must fail closed"
        );
        config.bridge_gateway_version = Some("0.3.1".to_owned());
        assert!(
            matches!(config.validate(), Err(PortError::Rejected { .. })),
            "an incompatible gateway OAuth contract must fail closed"
        );
        config.bridge_gateway_version = Some("0.3.2".to_owned());
        assert!(config.validate().is_ok());
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn connections_bridge_archives_are_exact_single_files() {
        let valid_request = single_file_tar("request.json", br#"{}"#);
        assert!(
            validate_connections_bridge_archive(&valid_request, "request.json").is_ok(),
            "the bridge may stage exactly its one server-authored request file"
        );
        let valid_response = single_file_tar("response.json", br#"{}"#);
        assert!(
            validate_connections_bridge_archive(&valid_response, "response.json").is_ok(),
            "the bridge may persist exactly its one response file"
        );

        for (description, archive) in [
            (
                "path traversal",
                single_file_tar("../request.json", br#"{}"#),
            ),
            ("wrong filename", single_file_tar("other.json", br#"{}"#)),
            (
                "multiple entries",
                two_file_tar("request.json", br#"{}"#, "extra.json", br#"{}"#),
            ),
        ] {
            assert!(
                validate_connections_bridge_archive(&archive, "request.json").is_err(),
                "the bridge must reject {description} instead of extracting it"
            );
        }

        let mut corrupted_checksum = valid_request;
        corrupted_checksum[0] = b'X';
        assert!(
            validate_connections_bridge_archive(&corrupted_checksum, "request.json").is_err(),
            "the bridge must reject an archive whose checksum does not bind its path"
        );
    }

    #[cfg(feature = "runtime")]
    fn single_file_tar(name: &str, body: &[u8]) -> Vec<u8> {
        let mut archive = tar_header(name, body.len());
        archive.extend_from_slice(body);
        archive.resize((archive.len() + 511) & !511, 0);
        archive.extend_from_slice(&[0; 1024]);
        archive
    }

    #[cfg(feature = "runtime")]
    fn two_file_tar(
        first_name: &str,
        first_body: &[u8],
        second_name: &str,
        second_body: &[u8],
    ) -> Vec<u8> {
        let mut archive = tar_header(first_name, first_body.len());
        archive.extend_from_slice(first_body);
        archive.resize((archive.len() + 511) & !511, 0);
        archive.extend_from_slice(&tar_header(second_name, second_body.len()));
        archive.extend_from_slice(second_body);
        archive.resize((archive.len() + 511) & !511, 0);
        archive.extend_from_slice(&[0; 1024]);
        archive
    }

    #[cfg(feature = "runtime")]
    fn tar_header(name: &str, size: usize) -> Vec<u8> {
        let mut header = vec![0_u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        let size = format!("{:011o}\0", size);
        header[124..136].copy_from_slice(size.as_bytes());
        header[148..156].fill(b' ');
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let checksum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
        let checksum = format!("{:06o}\0 ", checksum);
        header[148..156].copy_from_slice(checksum.as_bytes());
        header
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn raw_sandbox_binding_rejects_stale_uid_image_mismatch_and_not_ready() {
        let expected_image = "registry.example.test/steward-bridge@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let valid = RawSandbox {
            metadata: Some(ObjectMeta {
                labels: HashMap::from([(RUNTIME_UID_LABEL.to_owned(), "runtime-uid-a".to_owned())]),
                ..ObjectMeta::default()
            }),
            spec: Some(RawSandboxSpec {
                template: Some(RawSandboxTemplate {
                    image: expected_image.to_owned(),
                    ..RawSandboxTemplate::default()
                }),
                ..RawSandboxSpec::default()
            }),
            status: Some(RawSandboxStatus {
                phase: RawSandboxPhase::Ready as i32,
                ..RawSandboxStatus::default()
            }),
        };
        assert!(
            validate_raw_sandbox_binding(&valid, "runtime-uid-a", Some(expected_image), true)
                .is_ok(),
            "the controller may execute only its exact Ready sandbox image"
        );

        let mut stale_uid = valid.clone();
        stale_uid.metadata.as_mut().map(|metadata| {
            metadata
                .labels
                .insert(RUNTIME_UID_LABEL.to_owned(), "runtime-uid-b".to_owned())
        });
        assert!(
            validate_raw_sandbox_binding(&stale_uid, "runtime-uid-a", Some(expected_image), true)
                .is_err(),
            "a name-reused sandbox with a stale runtime UID must fail closed"
        );

        let mut mismatched_image = valid.clone();
        if let Some(template) = mismatched_image
            .spec
            .as_mut()
            .and_then(|spec| spec.template.as_mut())
        {
            template.image = "registry.example.test/steward-bridge@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned();
        }
        assert!(
            validate_raw_sandbox_binding(
                &mismatched_image,
                "runtime-uid-a",
                Some(expected_image),
                true,
            )
            .is_err(),
            "a raw image value that differs from the verified digest must fail closed"
        );

        let mut provisioning = valid;
        provisioning.status = Some(RawSandboxStatus {
            phase: RawSandboxPhase::Provisioning as i32,
            ..RawSandboxStatus::default()
        });
        assert!(
            validate_raw_sandbox_binding(
                &provisioning,
                "runtime-uid-a",
                Some(expected_image),
                true
            )
            .is_err(),
            "execution must not begin until the raw sandbox is Ready"
        );
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn teardown_uses_the_external_objects_recorded_in_status() {
        let request = SandboxRequest {
            runtime: RuntimeId("runtime-uid-a".to_owned()),
            workspace_key: "team-a".to_owned(),
            execution_class: SandboxExecutionClass::Agent,
            agent_type: AgentType {
                name: "base".to_owned(),
            },
            models: Vec::new(),
            tools: Vec::new(),
            refs: RuntimeRefs {
                workspace: Some("workspace-recorded".to_owned()),
                sandbox: Some("sandbox-recorded".to_owned()),
                litellm_key: Some("key-recorded".to_owned()),
            },
            execution_binding: None,
        };

        assert_eq!(
            deletion_names(&request),
            (
                "workspace-recorded".to_owned(),
                "sandbox-recorded".to_owned()
            ),
            "teardown must traverse status.refs instead of guessing external names from current spec"
        );
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn tool_authority_attaches_only_the_steward_gateway_provider() -> Result<(), String> {
        let projection = project_request(
            &SandboxRequest {
                runtime: RuntimeId("runtime-uid-a".to_owned()),
                workspace_key: "team-a".to_owned(),
                execution_class: SandboxExecutionClass::Agent,
                agent_type: AgentType {
                    name: "base".to_owned(),
                },
                models: Vec::new(),
                tools: vec![ToolGrant {
                    provider: "github".to_owned(),
                    resource: "search_repositories".to_owned(),
                    action: "read".to_owned(),
                }],
                refs: RuntimeRefs::default(),
                execution_binding: None,
            },
            None,
            None,
        )
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
    fn inference_authority_attaches_only_the_runtime_key_provider() -> Result<(), String> {
        let projection = project_request(
            &SandboxRequest {
                runtime: RuntimeId("runtime-uid-a".to_owned()),
                workspace_key: "team-a".to_owned(),
                execution_class: SandboxExecutionClass::Agent,
                agent_type: AgentType {
                    name: "base".to_owned(),
                },
                models: vec![steward_types::ModelRef {
                    provider: "openai".to_owned(),
                    model: "priced-model".to_owned(),
                }],
                tools: Vec::new(),
                refs: RuntimeRefs::default(),
                execution_binding: None,
            },
            None,
            None,
        )
        .map_err(|error| format!("runtime projection failed: {error:?}"))?;

        assert_eq!(
            projection.providers,
            ["steward-litellm"],
            "an inference-bearing runtime must receive only the runtime-bound token-grant provider"
        );
        Ok(())
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn task_agent_stderr_is_reduced_to_safe_categories() {
        assert_eq!(
            task_agent_failure_category(
                b"steward-connections-bridge: bridge operation did not receive a valid MCP-GW response"
            ),
            "bridge-gateway",
            "the bridge failure boundary must expose only an allowlisted non-secret category"
        );
        assert_eq!(
            task_agent_failure_category(
                b"steward-connections-bridge: bridge MCP-GW rejected runtime authentication"
            ),
            "bridge-runtime-authentication"
        );
        assert_eq!(
            task_agent_failure_category(
                b"steward-connections-bridge: bridge MCP-GW response violated its bounded contract"
            ),
            "bridge-response-contract"
        );
        assert_eq!(
            task_agent_failure_category(
                b"steward-connections-bridge: bridge MCP-GW transport is unavailable"
            ),
            "bridge-gateway-transport"
        );
        assert_eq!(
            task_agent_failure_category(
                b"steward-connections-bridge: bridge MCP-GW returned an unexpected status"
            ),
            "bridge-gateway-status"
        );
        assert_eq!(
            task_agent_failure_category(
                b"steward-connections-bridge: bridge MCP-GW response body is unavailable"
            ),
            "bridge-gateway-body"
        );
        assert_eq!(
            task_agent_failure_category(
                b"steward-connections-bridge: bridge request.json input is unreadable"
            ),
            "bridge-input",
            "bridge input failures must remain distinguishable without logging raw stderr"
        );
        assert_eq!(
            task_agent_failure_category(b"ERROR error sending request for url"),
            "network"
        );
        assert_eq!(
            task_agent_failure_category(b"Error loading configuration from config.toml"),
            "configuration"
        );
        assert_eq!(
            task_agent_failure_category(b"unexpected argument '--bad'\nUsage: codex exec"),
            "cli-usage"
        );
        assert_eq!(
            task_agent_failure_category(b"provider returned unauthorized status 401"),
            "authentication"
        );
        assert_eq!(task_agent_failure_category(b"opaque failure"), "agent");
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn removing_tool_authority_plans_provider_detach() {
        assert_eq!(
            provider_reconciliation(true, false),
            Some(ProviderReconciliation::Detach),
            "removing all tool grants must detach the Steward gateway provider"
        );
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn unchanged_provider_authority_is_a_reconciliation_noop() {
        assert_eq!(
            provider_reconciliation(true, true),
            None,
            "an already-attached desired provider must not be rewritten"
        );
        assert_eq!(
            provider_reconciliation(false, false),
            None,
            "an already-absent undesired provider must not be rewritten"
        );
        assert_eq!(
            provider_reconciliation(false, true),
            Some(ProviderReconciliation::Attach),
            "a newly granted tool capability must attach the gateway provider"
        );
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
